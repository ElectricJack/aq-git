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
        for prefix in &self.allowed_target_prefixes {
            // A prefix names a branch namespace below refs/heads/, never all branches.
            let namespace = prefix.strip_prefix("refs/heads/").unwrap_or("");
            if namespace.is_empty()
                || !prefix.ends_with('/')
                || aq_git_protocol::validate_ref(&format!("{prefix}x")).is_err()
            {
                return Err(Error::new(
                    "invalid_request",
                    "Publication target prefixes must be branch namespaces ending in '/' below refs/heads/",
                ));
            }
        }
        if self.mode == "worker" && !self.allowed_target_prefixes.is_empty() {
            return Err(Error::new(
                "invalid_request",
                "Worker bindings cannot publish",
            ));
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
        }
    }
    pub fn query_metrics(&self) -> Value {
        let coordination_entries = self.gates.lock().unwrap_or_else(|e| e.into_inner()).len();
        let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        json!({"cache_bytes":c.bytes,"cache_entries":c.entries.len(),"object_exchanges":self.readers.exchanges(),"cache_hits":c.hits,"cache_misses":c.misses,"cache_evictions":c.evictions,"singleflight_joins":c.joins,"coordination_entries":coordination_entries})
    }
    fn validate_profiles_and_remote(&self, binding: &Binding) -> Result<()> {
        if !binding.feature_profile.is_null()
            && binding.feature_profile != json!("no_attributes_v1")
        {
            return Err(Error::new(
                "unsupported_feature",
                "Feature profile is not enabled",
            ));
        }
        if let Some(url) = &binding.remote_url {
            if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(Error::new(
                    "invalid_request",
                    "Remote URL contains invalid characters",
                ));
            }
            if let Some(authority) = url
                .strip_prefix("https://")
                .map(|s| s.split(['/', '?', '#']).next().unwrap_or(""))
                && authority.contains('@')
            {
                return Err(Error::new(
                    "invalid_request",
                    "Embedded HTTPS credentials are not allowed",
                ));
            }
            if let Some(authority) = url
                .strip_prefix("ssh://")
                .map(|s| s.split('/').next().unwrap_or(""))
                && authority
                    .split_once('@')
                    .is_some_and(|(user, _)| user.contains(':'))
            {
                return Err(Error::new(
                    "invalid_request",
                    "Embedded SSH passwords are not allowed",
                ));
            }
        }
        if binding.credential_profile.is_some() {
            crate::git::credentials::RemoteAuth::load(&self.root, binding)?;
        }
        Ok(())
    }
    pub(crate) fn remote_run(
        &self,
        binding: &Binding,
        args: &[String],
        timeout: std::time::Duration,
    ) -> Result<crate::git::GitOutput> {
        let auth = crate::git::credentials::RemoteAuth::load(&self.root, binding)?;
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
            "inspect" | "tree" | "diff" | "contains" | "batch" | "query.batch" => self.query(r, &b),
            _ => Err(Error::new(
                "unsupported_feature",
                "Repository operation is not enabled",
            )),
        }
    }
    fn register(&self, r: &Request) -> Result<Value> {
        let id = r
            .repository_id
            .as_deref()
            .ok_or_else(|| Error::new("invalid_request", "repository_id required"))?;
        aq_git_protocol::validate_id(id).map_err(|e| Error::new(e.code, e.message))?;
        let intent = format!("repositories/{id}/registration.json");
        let previous: Option<Value> = self.root.read_json(&intent)?;
        if self
            .root
            .read_json::<Binding>(format!("repositories/{id}/binding.json"))?
            .is_some()
        {
            if previous
                .as_ref()
                .is_some_and(|v| v["operation_id"] == json!(r.operation_id))
            {
                return Ok(serde_json::to_value(self.binding(id)?)?);
            }
            return Err(Error::new(
                "binding_conflict",
                "Repository already registered",
            ));
        }
        if previous
            .as_ref()
            .is_some_and(|v| v["operation_id"] != json!(r.operation_id))
        {
            return Err(Error::new(
                "binding_in_use",
                "An unfinished registration owns this repository ID",
            )
            .details(json!({"blockers":[previous.as_ref().unwrap()["operation_id"]]})));
        }
        if r.expected_binding_generation != Some(0) {
            return Err(Error::new(
                "binding_conflict",
                "Registration requires generation zero",
            ));
        }
        let p = r.params.get("binding").unwrap_or(&r.params);
        let mut v = p.clone();
        v["repository_id"] = json!(id);
        if v.get("binding_generation").is_none() {
            v["binding_generation"] = json!(1)
        }
        if v.get("object_format").is_none() {
            v["object_format"] = json!("sha1")
        }
        let b: Binding = serde_json::from_value(v)?;
        if !["central", "worker"].contains(&b.mode.as_str())
            || b.identity.is_empty()
            || b.binding_generation == 0
            || b.mode == "central" && b.binding_generation != 1
        {
            return Err(Error::new(
                "invalid_request",
                "Invalid binding mode, identity or generation",
            ));
        }
        if b.object_format != "sha1" {
            return Err(Error::new(
                "unsupported_feature",
                "Only complete SHA-1 repositories are supported",
            ));
        }
        b.validate_ref_scopes()?;
        if b.mode == "worker" && (b.remote_url.is_some() || b.credential_profile.is_some()) {
            return Err(Error::new(
                "invalid_request",
                "Worker bindings cannot contain remote credentials",
            ));
        }
        self.validate_profiles_and_remote(&b)?;
        if let Some(url) = &b.remote_url {
            if url.starts_with('-') || url.contains('\n') || url.contains('\0') {
                return Err(Error::new("invalid_request", "Invalid remote"));
            }
            if !(url.starts_with("https://")
                || url.starts_with("ssh://")
                || b.allow_file_transport && (url.starts_with('/') || url.starts_with("file://")))
            {
                return Err(Error::new(
                    "unsupported_feature",
                    "Remote protocol is not enabled",
                ));
            }
        }
        self.root
            .write_json(&intent, &json!({"operation_id":r.operation_id,"binding":b}))?;
        let pool = self
            .root
            .create_dir(format!("repositories/{id}/objects.git"))?;
        let o = self.runner.run(
            &pool,
            &[
                "init".into(),
                "--bare".into(),
                "--object-format=sha1".into(),
                ".".into(),
            ],
        )?;
        if !o.status.success() {
            return Err(Error::new("git_failed", "Cannot initialize pool"));
        }
        crate::git::capabilities::probe(
            &self.root,
            &self.runner,
            r.operation_id
                .as_deref()
                .ok_or_else(|| Error::new("invalid_request", "operation_id required"))?,
        )?;
        self.root.write_json(
            format!("repositories/{id}/pool.json"),
            &json!({"format_version":1,"storage_generation":1,"lifecycle":"present"}),
        )?;
        self.root
            .write_json(format!("repositories/{id}/binding.json"), &b)?;
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.into(), b.clone());
        Ok(serde_json::to_value(b)?)
    }
    fn scope(&self, r: &Request, b: &Binding) -> Result<Vec<String>> {
        let mut refs = match r.params.get("refs") {
            Some(v) => serde_json::from_value::<Vec<String>>(v.clone())?,
            None => b.allowed_refs.clone(),
        };
        if refs.is_empty() || refs.len() > 1000 || refs.iter().any(|s| !b.allowed_refs.contains(s))
        {
            return Err(Error::new(
                "invalid_request",
                "Requested refs must be a bounded configured scope",
            ));
        }
        refs.sort();
        refs.dedup();
        Ok(refs)
    }
    fn sync_shared(&self, r: &Request, b: &Binding) -> Result<Value> {
        let refs = self.scope(r, b)?;
        let key = format!(
            "{}:{}:{}",
            b.repository_id,
            b.binding_generation,
            serde_json::to_string(&refs)?
        );
        let minimum = match r.params.get("min_started_at_ms") {
            Some(v) => v.as_i64().ok_or_else(|| {
                Error::new("invalid_request", "min_started_at_ms must be integer")
            })?,
            None => 0,
        };
        if minimum > chrono::Utc::now().timestamp_millis() {
            return Err(Error::new(
                "invalid_request",
                "Minimum sync start cannot be in the future",
            ));
        }
        let first = self.sync_flight(r, b, key.clone(), admitted_at(), minimum)?;
        if first["started_at_ms"].as_i64().unwrap_or(0) >= minimum {
            return Ok(first);
        }
        self.sync_flight(
            r,
            b,
            format!("{key}:after:{minimum}"),
            admitted_at(),
            minimum,
        )
    }
    fn sync_flight(
        &self,
        r: &Request,
        b: &Binding,
        key: String,
        admitted: Instant,
        minimum: i64,
    ) -> Result<Value> {
        let flight = {
            let mut flights = self.sync_flights.lock().unwrap_or_else(|e| e.into_inner());
            flights.retain(|_, v| v.strong_count() > 0);
            if let Some(f) = flights.get(&key).and_then(|f| f.upgrade()) {
                f
            } else {
                let f = Arc::new(Mutex::new(None));
                flights.insert(key.clone(), Arc::downgrade(&f));
                f
            }
        };
        let mut result = crate::git::lock_with_deadline(&flight)?;
        if let Some(value) = &*result {
            return value.clone();
        }
        let completed_key = format!("{key}:completed-sync");
        let completed = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache
                .completions
                .get(&completed_key)
                .is_some_and(|completed| *completed >= admitted)
            {
                cache
                    .entries
                    .get(&completed_key)
                    .filter(|(value, _, _)| {
                        value["started_at_ms"]
                            .as_i64()
                            .is_some_and(|started| started >= minimum)
                    })
                    .map(|(value, _, _)| value.clone())
            } else {
                None
            }
        };
        if let Some(value) = completed {
            return Ok(value.as_ref().clone());
        }
        let _permit = self.heavy_budget.acquire()?;
        let value = self.sync(r, b);
        self.root.write_json(
            format!("repositories/{}/refresh.json", b.repository_id),
            &json!({"at_ms":chrono::Utc::now().timestamp_millis(),"error":value.as_ref().err()}),
        )?;
        if let Ok(value) = &value {
            self.remember(
                completed_key,
                value.clone(),
                serde_json::to_vec(&value)?.len(),
                Some(Instant::now()),
            );
        }
        *result = Some(value.clone());
        value
    }
    fn sync(&self, r: &Request, b: &Binding) -> Result<Value> {
        if b.mode != "central" {
            return Err(Error::new(
                "unsupported_feature",
                "Worker synchronization uses artifact import",
            ));
        }
        let refs = self.scope(r, b)?;
        let url = b
            .remote_url
            .as_ref()
            .ok_or_else(|| Error::new("invalid_request", "No remote configured"))?;
        let sid = r
            .operation_id
            .clone()
            .ok_or_else(|| Error::new("invalid_request", "operation_id required"))?;
        let intent_path = format!("repositories/{}/sync/{sid}.json", b.repository_id);
        let prior: Option<Value> = self.root.read_json(&intent_path)?;
        if let Some(result) = prior.as_ref().and_then(|i| i.get("result")) {
            return Ok(result.clone());
        }
        let started = prior
            .as_ref()
            .and_then(|i| i["started_at_ms"].as_i64())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let mut map = BTreeMap::new();
        if let Some(prior) = &prior {
            map = serde_json::from_value(prior["refs"].clone())?;
        } else {
            // A successful bounded advertisement establishes exact coverage, including absent configured refs.
            let mut args = vec![
                "ls-remote".into(),
                "--refs".into(),
                "--".into(),
                url.clone(),
            ];
            args.extend(refs.clone());
            let advertisement = self.remote_git(b, args)?;
            for line in advertisement
                .split(|c| *c == b'\n')
                .filter(|line| !line.is_empty())
            {
                let tab = line
                    .iter()
                    .position(|c| *c == b'\t')
                    .ok_or_else(|| Error::new("git_failed", "Malformed ref advertisement"))?;
                let name = String::from_utf8(line[tab + 1..].to_vec())
                    .map_err(|_| Error::new("git_failed", "Invalid ref advertisement"))?;
                aq_git_protocol::validate_ref(&name)
                    .map_err(|_| Error::new("git_failed", "Invalid advertised ref"))?;
                let hex = String::from_utf8(line[..tab].to_vec())
                    .map_err(|_| Error::new("git_failed", "Malformed advertised OID"))?;
                aq_git_protocol::ObjectId {
                    algorithm: "sha1".into(),
                    hex: hex.clone(),
                }
                .validate()
                .map_err(|_| Error::new("git_failed", "Malformed advertised OID"))?;
                if refs.contains(&name)
                    && map.insert(name, hex.clone()).is_some_and(|old| old != hex)
                {
                    return Err(Error::new("git_failed", "Inconsistent ref advertisement"));
                }
            }
            self.root.write_json(&intent_path,&json!({"format_version":1,"binding_generation":b.binding_generation,"observation_id":sid,"started_at_ms":started,"covered_refs":refs,"refs":map}))?;
        }
        let mut known = BTreeMap::new();
        if !map.is_empty() {
            let existing = self.git(
                &b.repository_id,
                vec![
                    "for-each-ref".into(),
                    "--format=%(refname) %(objectname)".into(),
                    format!("refs/aq-git/observations/{sid}/"),
                    format!("refs/aq-git/staging/{sid}/"),
                ],
            )?;
            for line in existing.split(|b| *b == b'\n') {
                if let Some(space) = line.iter().position(|b| *b == b' ') {
                    known.insert(
                        String::from_utf8_lossy(&line[..space]).into_owned(),
                        String::from_utf8_lossy(&line[space + 1..]).into_owned(),
                    );
                }
            }
            let mut expected = BTreeMap::new();
            for (i, (_, oid)) in map.iter().enumerate() {
                expected.insert(format!("refs/aq-git/staging/{sid}/{i}"), oid.clone());
                expected.insert(format!("refs/aq-git/observations/{sid}/{i}"), oid.clone());
            }
            if known
                .iter()
                .any(|(name, oid)| expected.get(name) != Some(oid))
            {
                return Err(Error::new(
                    "recovery_blocked",
                    "Sync refs changed unexpectedly",
                ));
            }
        }
        let mut fetch_duration_ms = 0;
        if !map.is_empty() {
            let mut args = vec![
                "-c".into(),
                "fetch.writeCommitGraph=true".into(),
                "fetch".into(),
                "--no-tags".into(),
                "--no-write-fetch-head".into(),
                "--".into(),
                url.clone(),
            ];
            for (i, (_, oid)) in map.iter().enumerate() {
                args.push(format!("{oid}:refs/aq-git/staging/{sid}/{i}"));
            }
            let fetch_started = Instant::now();
            self.remote_git(b, args)?;
            fetch_duration_ms = fetch_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        }
        if !map.is_empty() {
            let mut transaction = String::from("start\n");
            for (i, (_, oid)) in map.iter().enumerate() {
                let target = format!("refs/aq-git/observations/{sid}/{i}");
                if let Some(old) = known.get(&target) {
                    if old != oid {
                        return Err(Error::new(
                            "recovery_blocked",
                            "Observation pin changed unexpectedly",
                        ));
                    }
                } else {
                    transaction.push_str(&format!("create {target} {oid}\n"));
                }
                transaction.push_str(&format!("delete refs/aq-git/staging/{sid}/{i} {oid}\n"));
            }
            transaction.push_str("prepare\ncommit\n");
            let out = self.runner.run_with(
                &self.pool_path(&b.repository_id),
                &["update-ref".into(), "--stdin".into()],
                transaction.as_bytes(),
                &[],
                self.runner.limits.timeout,
            )?;
            if !out.status.success() {
                return Err(Error::new(
                    "recovery_blocked",
                    "Cannot install observation ref transaction",
                ));
            }
        }
        let info = self.pool_path(&b.repository_id).join("objects/info");
        let commit_graph_available = info.join("commit-graph").is_file()
            || info.join("commit-graphs/commit-graph-chain").is_file();
        let observation = json!({"format_version":1,"observation_id":sid,"binding_generation":b.binding_generation,"covered_refs":refs,"refs":map,"started_at_ms":started,"completed_at_ms":chrono::Utc::now().timestamp_millis(),"availability":"available","last_refresh_error":null,"fetch_duration_ms":fetch_duration_ms,"commit_graph_available":commit_graph_available});
        self.root.write_json(
            format!("repositories/{}/observations/{sid}.json", b.repository_id),
            &observation,
        )?;
        self.root.write_json(
            format!("repositories/{}/observation.json", b.repository_id),
            &observation,
        )?;
        self.root.write_json(&intent_path,&json!({"format_version":1,"binding_generation":b.binding_generation,"observation_id":sid,"started_at_ms":started,"covered_refs":refs,"refs":map,"result":observation}))?;
        Ok(observation)
    }
    fn observation(&self, r: &Request, b: &Binding) -> Result<Value> {
        let refs = self.scope(r, b)?;
        let covers = |o: &Value| {
            o["binding_generation"] == json!(b.binding_generation)
                && refs.iter().all(|x| {
                    o["covered_refs"]
                        .as_array()
                        .is_some_and(|a| a.contains(&json!(x)))
                })
        };
        let mut found = self
            .root
            .read_json::<Value>(format!("repositories/{}/observation.json", b.repository_id))?
            .filter(|o| covers(o));
        if found.is_none() {
            for path in
                self.json_files(&format!("repositories/{}/observations", b.repository_id))?
            {
                if let Some(candidate) = self.root.read_json::<Value>(path)?
                    && covers(&candidate)
                    && found.as_ref().is_none_or(|old| {
                        candidate["completed_at_ms"].as_i64() > old["completed_at_ms"].as_i64()
                    })
                {
                    found = Some(candidate);
                }
            }
        }
        let mut o = found.ok_or_else(|| {
            Error::new(
                "observation_unavailable",
                "Requested scope has no completed observation",
            )
        })?;
        if r.params
            .get("max_age_ms")
            .is_some_and(|v| v.as_u64().is_none())
        {
            return Err(Error::new(
                "invalid_request",
                "max_age_ms must be nonnegative integer",
            ));
        }
        if let Some(age) = r.params["max_age_ms"].as_i64()
            && chrono::Utc::now().timestamp_millis() - o["completed_at_ms"].as_i64().unwrap_or(0)
                > age
        {
            return Err(Error::new(
                "observation_stale",
                "Observation exceeds requested age",
            ));
        }
        if let Some(refresh) = self
            .root
            .read_json::<Value>(format!("repositories/{}/refresh.json", b.repository_id))?
        {
            o["last_refresh_error"] = refresh["error"].clone();
        }
        Ok(o)
    }
}
mod binding;
mod queries;

pub const OPERATIONS: &[&str] = &[
    "repo.register",
    "repo.binding-update",
    "repo.show",
    "repo.sync",
    "repo.observation",
    "inspect",
    "tree",
    "diff",
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
/// Validate fields that must never be recorded in a mutation intent before rejection.
/// Services call this after checking historical replay and before creating a journal record.
pub fn validate_sensitive_input(request: &Request) -> Result<()> {
    if !["repo.register", "repo.binding-update"].contains(&request.operation.as_str()) {
        return Ok(());
    }
    let value = request.params.get("binding").unwrap_or(&request.params);
    if !value.is_object() {
        return Err(Error::new("invalid_request", "Binding must be an object"));
    }
    if let Some(profile) = value.get("credential_profile") {
        crate::git::credentials::validate_name(profile)?;
    }
    if value
        .get("feature_profile")
        .is_some_and(|v| !v.is_null() && v != &json!("no_attributes_v1"))
    {
        return Err(Error::new(
            "unsupported_feature",
            "Feature profile is not enabled",
        ));
    }
    if let Some(url) = value.get("remote_url").filter(|v| !v.is_null()) {
        let url = url
            .as_str()
            .ok_or_else(|| Error::new("invalid_request", "Remote URL must be a string"))?;
        if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::new(
                "invalid_request",
                "Remote URL contains invalid characters",
            ));
        }
        if url
            .strip_prefix("https://")
            .is_some_and(|s| s.split(['/', '?', '#']).next().unwrap_or("").contains('@'))
        {
            return Err(Error::new(
                "invalid_request",
                "Embedded HTTPS credentials are not allowed",
            ));
        }
        if url.strip_prefix("ssh://").is_some_and(|s| {
            s.split('/')
                .next()
                .unwrap_or("")
                .split_once('@')
                .is_some_and(|(user, _)| user.contains(':'))
        }) {
            return Err(Error::new(
                "invalid_request",
                "Embedded SSH passwords are not allowed",
            ));
        }
    }
    Ok(())
}
