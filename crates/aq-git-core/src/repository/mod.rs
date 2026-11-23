use crate::{Error, Result, git::GitRunner, state::StateRoot};
use aq_git_protocol::Request;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};

thread_local! {static ADMITTED_AT:std::cell::Cell<Option<Instant>>=const{std::cell::Cell::new(None)};}
pub struct AdmissionScope {
    previous: Option<Instant>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Default for AdmissionScope {
    fn default() -> Self {
        Self::new()
    }
}
impl AdmissionScope {
    pub fn new() -> Self {
        Self::at(Instant::now())
    }
    pub fn at(instant: Instant) -> Self {
        let previous = ADMITTED_AT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(previous.map_or(instant, |old| old.min(instant))));
            previous
        });
        Self {
            previous,
            _thread: std::marker::PhantomData,
        }
    }
}
impl Drop for AdmissionScope {
    fn drop(&mut self) {
        ADMITTED_AT.with(|slot| slot.set(self.previous));
    }
}
fn admitted_at() -> Instant {
    ADMITTED_AT
        .with(|slot| slot.get())
        .unwrap_or_else(Instant::now)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub repository_id: String,
    pub binding_generation: u64,
    pub identity: String,
    pub mode: String,
    pub object_format: String,
    #[serde(default)]
    pub allowed_refs: Vec<String>,
    #[serde(default)]
    pub allowed_targets: Vec<String>,
    /// Branch namespaces (for example `refs/heads/aq/`) whose branches may be
    /// created and replaced under an exact expected-old lease, such as
    /// pull-request head branches. Exact `allowed_targets` stay fast-forward only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_target_prefixes: Vec<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub credential_profile: Option<String>,
    #[serde(default)]
    pub allow_file_transport: bool,
    #[serde(default)]
    pub feature_profile: Value,
}
/// How a binding authorizes a publication destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetScope {
    /// Listed in `allowed_targets`: fast-forward only.
    Exact,
    /// Under an `allowed_target_prefixes` namespace: may also be replaced.
    Prefix,
}
impl Binding {
    pub fn target_scope(&self, target: &str) -> Option<TargetScope> {
        if !target.starts_with("refs/heads/") {
            return None;
        }
        if self.allowed_targets.iter().any(|t| t == target) {
            Some(TargetScope::Exact)
        } else if self
            .allowed_target_prefixes
            .iter()
            .any(|p| target.starts_with(p.as_str()))
        {
            Some(TargetScope::Prefix)
        } else {
            None
        }
    }
    /// Shared registration/replacement checks for refs and publication scopes.
    pub(crate) fn validate_ref_scopes(&self) -> Result<()> {
        for name in self.allowed_refs.iter().chain(self.allowed_targets.iter()) {
            aq_git_protocol::validate_ref(name).map_err(|e| Error::new(e.code, e.message))?;
        }
        if self
            .allowed_targets
            .iter()
            .any(|s| !s.starts_with("refs/heads/"))
        {
            return Err(Error::new(
                "invalid_request",
                "Publication targets must be branches",
            ));
        }
        self.validate_publication_namespaces()?;
        if self.mode == "worker" && !self.allowed_target_prefixes.is_empty() {
            return Err(Error::new(
                "invalid_request",
                "Worker bindings cannot publish",
            ));
        }
        Ok(())
    }
    fn validate_publication_namespaces(&self) -> Result<()> {
        // Try refs are local to the central repository and never published.
        let try_namespace = crate::git_http::TRY_PREFIX.trim_end_matches('/');
        if self
            .allowed_refs
            .iter()
            .chain(self.allowed_targets.iter())
            .any(|name| name.starts_with(crate::git_http::TRY_PREFIX))
            || self
                .allowed_target_prefixes
                .iter()
                .any(|prefix| prefix.starts_with(try_namespace))
        {
            return Err(Error::new(
                "invalid_request",
                "try refs under refs/heads/aq-try/ cannot be publication scopes",
            ));
        }
        for prefix in &self.allowed_target_prefixes {
            // A prefix names a branch namespace below refs/heads/, never all branches.
            let namespace = prefix.strip_prefix("refs/heads/").unwrap_or("");
            let branch_namespace = !namespace.is_empty() && prefix.ends_with('/');
            if !branch_namespace || aq_git_protocol::validate_ref(&format!("{prefix}x")).is_err() {
                return Err(Error::new(
                    "invalid_request",
                    "Publication target prefixes must be branch namespaces ending in '/' below refs/heads/",
                ));
            }
        }
        Ok(())
    }
}
type SyncResult = std::sync::Weak<Mutex<Option<Result<Value>>>>;
type SyncFlights = Arc<Mutex<BTreeMap<String, SyncResult>>>;
#[derive(Clone)]
pub struct RepositoryManager {
    pub root: Arc<StateRoot>,
    pub runner: GitRunner,
    pub heavy_budget: Arc<crate::limits::HeavyBudget>,
    readers: Arc<crate::git::objects::ObjectReaders>,
    cache: Arc<Mutex<Cache>>,
    bindings: Arc<Mutex<BTreeMap<String, Binding>>>,
    pending_bindings: Arc<Mutex<BTreeMap<String, String>>>,
    sync_flights: SyncFlights,
    gates: Arc<Mutex<BTreeMap<String, std::sync::Weak<RwLock<()>>>>>,
    flights: SyncFlights,
    /// Operator-selected profile configuration outside the state root, if any.
    credential_profiles: Option<PathBuf>,
}
#[derive(Default)]
struct Cache {
    entries: BTreeMap<String, (Arc<Value>, u64, usize)>,
    order: BTreeMap<u64, String>,
    completions: BTreeMap<String, Instant>,
    next: u64,
    bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    joins: u64,
}
impl Cache {
    fn lookup(&mut self, key: &str, count_miss: bool) -> Option<(Arc<Value>, usize)> {
        if let Some((v, old, serialized_len)) = self.entries.get(key).cloned() {
            self.hits += 1;
            self.order.remove(&old);
            self.next += 1;
            let stamp = self.next;
            self.order.insert(stamp, key.to_owned());
            self.entries.get_mut(key).unwrap().1 = stamp;
            Some((v, serialized_len))
        } else {
            if count_miss {
                self.misses += 1;
            }
            None
        }
    }
}
fn cache_entry_bytes(key: &str, value: &Value) -> usize {
    fn heap(value: &Value) -> usize {
        match value {
            Value::String(s) => s.capacity() + 32,
            Value::Array(values) => {
                values.capacity() * std::mem::size_of::<Value>()
                    + values.iter().map(heap).sum::<usize>()
                    + 32
            }
            Value::Object(values) => values
                .iter()
                .map(|(k, v)| k.capacity() + std::mem::size_of::<(String, Value)>() + 96 + heap(v))
                .sum(),
            _ => 0,
        }
    }
    key.len() * 4 + std::mem::size_of::<(String, Value, u64, usize)>() + 320 + heap(value)
}
impl RepositoryManager {
    pub fn new(root: Arc<StateRoot>, runner: GitRunner) -> Self {
        Self {
            gates: Arc::new(Mutex::new(BTreeMap::new())),
            sync_flights: Arc::new(Mutex::new(BTreeMap::new())),
            root,
            runner,
            heavy_budget: crate::limits::HeavyBudget::new(4),
            readers: Arc::new(crate::git::objects::ObjectReaders::default()),
            bindings: Arc::new(Mutex::new(BTreeMap::new())),
            pending_bindings: Arc::new(Mutex::new(BTreeMap::new())),
            cache: Arc::new(Mutex::new(Cache::default())),
            flights: Arc::new(Mutex::new(BTreeMap::new())),
            credential_profiles: None,
        }
    }
    /// Read credential profiles from an absolute operator-held file (for example a
    /// read-only vault mount) instead of `credential_profiles.json` in the state root.
    pub fn with_credential_profiles(mut self, path: PathBuf) -> Result<Self> {
        if !path.is_absolute()
            || path
                .to_str()
                .is_none_or(|p| p.chars().any(char::is_control))
        {
            return Err(Error::new(
                "invalid_configuration",
                "Credential profile configuration must be an absolute path",
            ));
        }
        self.credential_profiles = Some(path);
        Ok(self)
    }
    /// Labels of configured credential profiles, for capability reporting.
    pub(crate) fn credential_profile_labels(&self) -> Result<Vec<String>> {
        crate::git::credentials::labels(&self.root, self.credential_profiles.as_deref())
    }
    pub fn query_metrics(&self) -> Value {
        let coordination_entries = self.gates.lock().unwrap_or_else(|e| e.into_inner()).len();
        let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        json!({"cache_bytes":c.bytes,"cache_entries":c.entries.len(),"object_exchanges":self.readers.exchanges(),"cache_hits":c.hits,"cache_misses":c.misses,"cache_evictions":c.evictions,"singleflight_joins":c.joins,"coordination_entries":coordination_entries})
    }
    pub(crate) fn remote_run(
        &self,
        binding: &Binding,
        args: &[String],
        timeout: std::time::Duration,
    ) -> Result<crate::git::GitOutput> {
        let auth = crate::git::credentials::RemoteAuth::load(
            &self.root,
            self.credential_profiles.as_deref(),
            binding,
        )?;
        self.runner.run_remote(
            &self.pool_path(&binding.repository_id),
            args,
            &auth,
            timeout,
        )
    }
    fn remote_git(&self, binding: &Binding, args: Vec<String>) -> Result<Vec<u8>> {
        let out = self.remote_run(binding, &args, self.runner.limits.timeout)?;
        if !out.status.success() {
            return Err(Error::new(
                "remote_unavailable",
                "Remote Git command failed",
            ));
        }
        Ok(out.stdout)
    }
    pub fn pool_path(&self, id: &str) -> PathBuf {
        self.root
            .path()
            .join(format!("repositories/{id}/objects.git"))
    }
    pub fn binding(&self, id: &str) -> Result<Binding> {
        self.check_binding_admission(id, None)?;
        self.binding_unfenced(id)
    }
    fn binding_unfenced(&self, id: &str) -> Result<Binding> {
        aq_git_protocol::validate_id(id).map_err(|e| Error::new(e.code, e.message))?;
        let mut bindings = self.bindings.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(binding) = bindings.get(id) {
            return Ok(binding.clone());
        }
        let binding: Binding = self
            .root
            .read_json(format!("repositories/{id}/binding.json"))?
            .ok_or_else(|| Error::new("repository_not_found", "Repository is not registered"))?;
        bindings.insert(id.into(), binding.clone());
        Ok(binding)
    }
    pub fn checked_binding(&self, r: &Request) -> Result<Binding> {
        let b = self.binding(
            r.repository_id
                .as_deref()
                .ok_or_else(|| Error::new("invalid_request", "repository_id required"))?,
        )?;
        if r.expected_binding_generation
            .is_some_and(|g| g != b.binding_generation)
        {
            return Err(
                Error::new("binding_conflict", "Expected current binding generation")
                    .details(json!({"binding_generation":b.binding_generation})),
            );
        }
        Ok(b)
    }
    pub fn git(&self, id: &str, args: Vec<String>) -> Result<Vec<u8>> {
        let out = self.runner.run(&self.pool_path(id), &args)?;
        if !out.status.success() {
            return Err(Error::new("git_failed", "Git command failed")
                .details(json!({"exit_code":out.status.code()})));
        }
        Ok(out.stdout)
    }
    pub fn handle(&self, r: &Request) -> Result<Value> {
        validate_parameters(r)?;
        if r.operation == "repo.register" {
            return self.register(r);
        }
        let gate = self.repository_lifecycle(
            r.repository_id
                .as_deref()
                .ok_or_else(|| Error::new("invalid_request", "repository_id required"))?,
        );
        if r.operation == "repo.binding-update" {
            let _guard = crate::git::write_with_deadline(&gate)?;
            return self.update_binding(r);
        }
        let _guard = crate::git::read_with_deadline(&gate)?;
        let b = self.checked_binding(r)?;
        match r.operation.as_str() {
            "repo.binding" | "repo.inspect" | "repo.show" => Ok(serde_json::to_value(b)?),
            "repo.sync" => self.sync_shared(r, &b),
            "repo.observation" => self.observation(r, &b),
            "inspect" | "tree" | "diff" | "diff.patch" | "contains" | "batch" | "query.batch" => {
                self.query(r, &b)
            }
            _ => Err(Error::new(
                "unsupported_feature",
                "Repository operation is not enabled",
            )),
        }
    }
}
mod binding;
mod queries;
mod registration;
mod sync;
pub use queries::DIFF_PATCH_MAX_BYTES;
pub use registration::validate_sensitive_input;

pub const OPERATIONS: &[&str] = &[
    "repo.register",
    "repo.binding-update",
    "repo.show",
    "repo.sync",
    "repo.observation",
    "inspect",
    "tree",
    "diff",
    "diff.patch",
    "contains",
    "query.batch",
];
impl Drop for RepositoryManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.readers) == 1 {
            self.readers.clear();
        }
    }
}

fn validate_parameters(r: &Request) -> Result<()> {
    let allowed: &[&str] = match r.operation.as_str() {
        "repo.register" => {
            if r.params.get("binding").is_some() {
                &["binding"]
            } else {
                &[
                    "identity",
                    "mode",
                    "binding_generation",
                    "object_format",
                    "allowed_refs",
                    "allowed_targets",
                    "allowed_target_prefixes",
                    "remote_url",
                    "credential_profile",
                    "allow_file_transport",
                    "feature_profile",
                ]
            }
        }
        "repo.show" | "repo.inspect" | "repo.binding" => &[],
        "repo.binding-update" => &["binding", "reason"],
        "repo.sync" => &["refs", "min_started_at_ms"],
        "repo.observation" => &["refs", "max_age_ms"],
        "inspect" => &["oid"],
        "tree" => &["oid", "limit", "recursive"],
        "diff" => &["base", "target", "limit"],
        "diff.patch" => &["base", "target", "max_bytes"],
        "contains" => &["ancestor", "descendant"],
        "batch" | "query.batch" => &["items"],
        _ => {
            return Err(Error::new(
                "unsupported_feature",
                "Repository operation is not enabled",
            ));
        }
    };
    if r.params
        .as_object()
        .is_none_or(|p| p.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(Error::new("invalid_request", "Unknown operation parameter"));
    }
    Ok(())
}
