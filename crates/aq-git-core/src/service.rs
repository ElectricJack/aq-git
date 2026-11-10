use crate::{
    Error, Result,
    git::GitRunner,
    repository::RepositoryManager,
    state::{OperationStatus, StateRoot},
};
use aq_git_protocol::{Request, Response};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

pub struct RepositoryService {
    pub root: Arc<StateRoot>,
    pub repositories: RepositoryManager,
    diagnostics_only: bool,
    operation_locks: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
    mutation_locks: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
}
impl RepositoryService {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_state_root(StateRoot::open(path)?)
    }
    pub fn with_spawn_helper(mut self, path: PathBuf) -> Result<Self> {
        self.repositories.runner = self.repositories.runner.clone().with_spawn_helper(path)?;
        Ok(self)
    }
    pub fn from_state_root(state: StateRoot) -> Result<Self> {
        Self::from_state_root_with_git(state, PathBuf::from("/usr/bin/git"))
    }
    pub fn from_state_root_with_git(state: StateRoot, git: PathBuf) -> Result<Self> {
        Self::initialize(state, git, false)
    }
    /// Construct a fenced, metadata-only doctor without replaying local mutations.
    pub fn for_diagnostics(state: StateRoot) -> Result<Self> {
        Self::initialize(state, PathBuf::from("/usr/bin/git"), true)
    }
    fn initialize(state: StateRoot, git: PathBuf, diagnostics_only: bool) -> Result<Self> {
        if !git.is_absolute() {
            return Err(Error::new(
                "invalid_request",
                "administrator Git executable must be an absolute path",
            ));
        }
        let root = Arc::new(state);
        let limits = if diagnostics_only {
            crate::git::GitLimits::default()
        } else {
            root.read_json::<crate::git::GitLimits>("git_limits.json")?
                .unwrap_or_default()
        };
        if limits.max_children == 0
            || limits.max_children > 256
            || limits.timeout.is_zero()
            || limits.max_output_bytes == 0
            || limits.max_address_space_bytes == 0
        {
            return Err(Error::new(
                "invalid_request",
                "invalid administrator Git limits",
            ));
        }
        let runner = GitRunner::with_limits(git, limits);
        let repositories = RepositoryManager::new(root.clone(), runner);
        if !diagnostics_only {
            let registered = match std::fs::read_dir(root.path().join("repositories")) {
                Ok(entries) => {
                    let mut found = false;
                    for entry in entries {
                        let entry = entry?;
                        if entry.file_type()?.is_symlink() {
                            return Err(Error::new(
                                "invalid_path",
                                "symlink in repository registry",
                            ));
                        }
                        found |= entry.path().join("binding.json").exists();
                    }
                    found
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            if registered
                && crate::git::capabilities::cached_report(&root, &repositories.runner)?.is_none()
            {
                let fingerprint =
                    crate::git::capabilities::executable_fingerprint(&repositories.runner)?;
                let prior = root.read_json::<Value>("git-probe-intent.json")?;
                let id = prior
                    .as_ref()
                    .filter(|v| v["fingerprint"] == fingerprint)
                    .and_then(|v| v["operation_id"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                root.write_json(
                    "git-probe-intent.json",
                    &json!({"operation_id":id,"fingerprint":fingerprint}),
                )?;
                let _deadline = crate::git::DeadlineScope::new(std::time::Duration::from_secs(30));
                crate::git::capabilities::probe(&root, &repositories.runner, &id)?;
            }
            repositories.recover_bindings()?;
        }
        Ok(Self {
            repositories,
            root,
            diagnostics_only,
            operation_locks: Mutex::new(BTreeMap::new()),
            mutation_locks: Mutex::new(BTreeMap::new()),
        })
    }
    fn lock(map: &Mutex<BTreeMap<String, Weak<Mutex<()>>>>, key: &str) -> Arc<Mutex<()>> {
        let mut entries = map.lock().unwrap();
        entries.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = entries.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        entries.insert(key.to_string(), Arc::downgrade(&lock));
        lock
    }
    fn request_budget_ms(request: &Request) -> u64 {
        let default_ms = match request.operation.as_str() {
            "repo.sync"
            | "artifact.export"
            | "artifact.import"
            | "publish"
            | "operation.reconcile" => 300_000,
            op if op.starts_with("workspace.") || op.starts_with("candidate.") => 120_000,
            _ => 10_000,
        };
        request.deadline_ms.unwrap_or(default_ms).min(default_ms)
    }
    /// Preserve transport admission across blocking-executor and journal queues.
    pub fn execute_admitted(&self, mut request: Request, admitted: std::time::Instant) -> Response {
        let _admission = crate::repository::AdmissionScope::at(admitted);
        let budget = Self::request_budget_ms(&request);
        let elapsed = admitted.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if elapsed >= budget {
            return Response::error(
                &request,
                "deadline_exceeded",
                "request deadline expired before execution",
                Value::Null,
            );
        }
        request.deadline_ms = Some(budget - elapsed);
        self.execute(request)
    }
    pub fn execute(&self, request: Request) -> Response {
        let _admission = crate::repository::AdmissionScope::new();
        if self.diagnostics_only && request.operation != "doctor" {
            return Response::error(
                &request,
                "recovery_blocked",
                "diagnostic mode permits only read-only doctor",
                Value::Null,
            );
        }
        let _deadline = crate::git::DeadlineScope::new(std::time::Duration::from_millis(
            Self::request_budget_ms(&request),
        ));
        if let Err(error) = request.validate() {
            return Response::error(&request, error.code, error.message, error.details);
        }
        let answer = if request.is_mutation() {
            self.mutate(&request)
        } else {
            self.dispatch(&request)
        };
        let mut response = match answer {
            Ok(result) => Response::success(&request, result),
            Err(error) => Response::error(&request, error.code, error.message, error.details),
        };
        if request.is_mutation()
            && let Some(id) = &request.operation_id
            && let Ok(Some(record)) = self.root.operation(id)
        {
            response.operation_state = Some(
                match record.status {
                    OperationStatus::Complete => "complete",
                    OperationStatus::Failed => "failed",
                    OperationStatus::Pending => "pending",
                }
                .to_string(),
            );
        }
        let handle = response
            .result
            .as_ref()
            .and_then(|v| v.get("retention_handle"))
            .and_then(Value::as_str)
            .or(request.operation_id.as_deref());
        if let Some(handle) = handle
            && let Ok(Some(record)) = self.root.retention(handle)
        {
            response.resource_status = Some(json!({"handle_id":record.id,"state":record.state,
                    "generation":record.generation,"availability":"not_checked","observed_at":chrono::Utc::now().to_rfc3339()}));
        }
        response
    }
    fn mutate(&self, r: &Request) -> Result<Value> {
        let id = r
            .operation_id
            .as_deref()
            .ok_or_else(|| Error::new("invalid_request", "operation_id required"))?;
        let lock = Self::lock(&self.operation_locks, id);
        let _operation_guard = crate::git::lock_with_deadline(&lock)?;
        // Canonical terminal replay precedes every current-admission fence.
        let existing = self.root.operation(id)?;
        let payload = json!({"operation":r.operation,"repository_id":r.repository_id,"expected_binding_generation":r.expected_binding_generation,"params":r.params});
        if existing.is_some() {
            let record = self.root.begin_operation(id, &payload)?;
            match record.status {
                OperationStatus::Complete => return Ok(record.result.unwrap_or(Value::Null)),
                OperationStatus::Failed => {
                    return Err(serde_json::from_value(record.error.unwrap_or(Value::Null))?);
                }
                OperationStatus::Pending => {}
            }
        } else {
            crate::repository::validate_sensitive_input(r)?;
        }
        // The short read lease makes check+intent atomic against a drained
        // binding writer's blocker scan and fence installation. Never hold it
        // while dispatching a binding update (which takes the write lease).
        let admission = r
            .repository_id
            .as_deref()
            .map(|repository| self.repositories.repository_lifecycle(repository));
        {
            let _lease = admission
                .as_ref()
                .map(|gate| crate::git::read_with_deadline(gate))
                .transpose()?;
            self.repositories.check_request_admission(r)?;
            self.check_artifact_admission(r)?;
            if existing.is_none() {
                self.root.begin_operation(id, &payload)?;
            }
        }
        let lock = Self::lock(
            &self.mutation_locks,
            r.repository_id.as_deref().unwrap_or("root"),
        );
        let _mutation_guard = if r.operation == "repo.sync" {
            None
        } else {
            Some(crate::git::lock_with_deadline(&lock)?)
        };
        // Sync followers join the manager's flight without consuming a Git/heavy slot.
        let _heavy = if r.operation == "repo.sync" {
            None
        } else {
            Some(self.repositories.heavy_budget.acquire()?)
        };
        let answer = self.dispatch(r);
        let current = self
            .root
            .operation(id)?
            .ok_or_else(|| Error::new("io_error", "operation journal disappeared"))?;
        match &answer {
            Ok(value) => {
                self.root
                    .complete_operation(id, current.generation, value.clone())?;
            }
            Err(error) => {
                // Interrupted durable mutations remain pending for exact replay/reconciliation.
                if ![
                    "io_error",
                    "state_io",
                    "state_not_found",
                    "deadline_exceeded",
                    "operation_indeterminate",
                    "idempotency_conflict",
                    "recovery_blocked",
                    "git_failed",
                    "git_spawn",
                    "git_wait",
                    "git_timeout",
                    "output_limit",
                    "storage_limit",
                    "limit_exceeded",
                    "overloaded",
                ]
                .contains(&error.code.as_str())
                    && error.details["restart_required"] != true
                    && error.details["remote_effect_unknown"] != true
                {
                    self.root.fail_operation(
                        id,
                        current.generation,
                        serde_json::to_value(error)?,
                    )?;
                }
            }
        }
        answer
    }
    fn dispatch(&self, r: &Request) -> Result<Value> {
        if ["capabilities", "service.metrics"].contains(&r.operation.as_str())
            && !r.params.as_object().is_some_and(|p| p.is_empty())
        {
            return Err(Error::new(
                "invalid_request",
                "operation takes no parameters",
            ));
        }
        match r.operation.as_str() {
            "capabilities" => Ok(self.capabilities()),
            "service.metrics" => Ok(
                json!({"queries":self.repositories.query_metrics(),"git":self.repositories.runner.metrics(),
                    "heavy":{"active":self.repositories.heavy_budget.active(),"limit":self.repositories.heavy_budget.limit()},
                    "durability":crate::operations::durability_metrics()}),
            ),
            "operation.show" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Show {
                    target_operation_id: String,
                }
                let params: Show = serde_json::from_value(r.params.clone())?;
                crate::operations::id(&params.target_operation_id)?;
                Ok(serde_json::to_value(
                    self.root
                        .operation(&params.target_operation_id)?
                        .ok_or_else(|| Error::new("operation_not_found", "operation not found"))?,
                )?)
            }
            op if crate::publication::OPERATIONS.contains(&op) => self.publication_handle(r),
            op if crate::artifact::OPERATIONS.contains(&op) => self.artifact_handle(r),
            op if crate::lifecycle::OPERATIONS.contains(&op) => self.lifecycle_handle(r),
            op if crate::candidate::OPERATIONS.contains(&op) => self.candidate_handle(r),
            op if crate::workspace::OPERATIONS.contains(&op) => self.workspace_handle(r),
            op if crate::repository::OPERATIONS.contains(&op) => self.repositories.handle(r),
            _ => Err(Error::new(
                "unsupported_feature",
                "operation is not implemented or enabled",
            )),
        }
    }
    pub fn capabilities(&self) -> Value {
        let git_verification =
            match crate::git::capabilities::cached_report(&self.root, &self.repositories.runner) {
                Ok(Some(report)) => json!({"status":"verified","report":report}),
                Ok(None) => json!({"status":"unverified","probe_on":"repo.register"}),
                Err(error) => json!({"status":"unavailable","error_code":error.code}),
            };
        let mut operations = vec!["capabilities", "operation.show", "service.metrics"];
        operations.extend(crate::repository::OPERATIONS);
        operations.extend(crate::workspace::OPERATIONS);
        operations.extend(crate::candidate::OPERATIONS);
        operations.extend(crate::artifact::OPERATIONS);
        operations.extend(crate::lifecycle::OPERATIONS);
        operations.extend(crate::publication::OPERATIONS);
        json!({"protocol_version":1,"state_format_version":1,"artifact_format_version":1,
            "version":env!("CARGO_PKG_VERSION"),"operations":operations,
            "limits":{"request_bytes":1048576,"response_bytes":8388608,"batch_items":100,"candidate_sources":64,
                "connections":64,"request_admission":64,"concurrent_reads":32,"concurrent_mutations":4,"socket_io_workers":aq_git_protocol::SOCKET_IO_WORKERS,"cache_bytes":16777216,"cache_entries":4096,
                "query_response_bytes":65536,"batch_response_bytes":1048576,"git_child_waiters":crate::git::MAX_CHILD_WAITERS,
                "artifact":self.artifact_limits().map(|v|serde_json::to_value(v).unwrap_or(Value::Null)).unwrap_or_else(|e|json!({"configuration_error":e.code}))},
            "git":{"object_formats":["sha1"],"persistent_object_readers":true,"checkout_profile":"no_attributes_v1","metadata_attributes":["whitespace_diagnostics_v1"],"credential_profiles":true,"verification":git_verification,"process_limits":self.repositories.runner.limits(),
                "unsupported_features":["shallow","partial","submodules","lfs","attributes","signing","pruning"]},
            "direct_only_operations":["recovery.clear-fence"],"performance_acceptance":"not_certified"})
    }
}
