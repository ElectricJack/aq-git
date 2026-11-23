//! `repo.retain_ref`: sync one allowed ref from the remote and retain its head.
//!
//! One idempotent central-mode mutation that does the work of `repo.sync` for a
//! single ref and then pins a retention handle on exactly the synced head commit
//! (what `workspace.create` -> `workspace.checkpoint` -> `workspace.retire` does
//! today, without the workspace). The handle is the operation id, like a
//! checkpoint handle. When the head has not changed since the last retained sync
//! of that ref and that handle is still live, the existing handle is returned.
use crate::{Error, Result, operations, service::RepositoryService, state::RetentionRef};
use aq_git_protocol::{REPO_RETAIN_REF, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const OPERATIONS: &[&str] = &[REPO_RETAIN_REF];
/// Retention resource kind recorded on the handle.
const RESOURCE: &str = "retained_ref";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    #[serde(rename = "ref")]
    name: String,
}
/// Last retained head of one ref, so an unchanged head reuses its live handle.
#[derive(Serialize, Deserialize)]
struct RetainedRef {
    format_version: u32,
    #[serde(rename = "ref")]
    name: String,
    binding_generation: u64,
    head: String,
    retention_handle: String,
    synced_at_ms: i64,
}

impl RetainedRef {
    fn answer(&self, handle: &str) -> Value {
        json!({"ref": self.name, "head": operations::wire_oid(&self.head),
               "retention_handle": handle, "synced_at_ms": self.synced_at_ms})
    }

    fn matches(&self, other: &Self) -> bool {
        (
            self.format_version,
            &self.name,
            self.binding_generation,
            &self.head,
        ) == (
            other.format_version,
            &other.name,
            other.binding_generation,
            &other.head,
        )
    }
}

impl RepositoryService {
    pub(crate) fn retain_ref_handle(&self, r: &Request) -> Result<Value> {
        let p: Params = serde_json::from_value(r.params.clone())?;
        aq_git_protocol::validate_ref(&p.name).map_err(|e| Error::new(e.code, e.message))?;
        if !p.name.starts_with("refs/heads/") {
            return Err(Error::new("invalid_request", "ref must be a branch"));
        }
        let op = operations::opid(r)?;
        let repo = r
            .repository_id
            .as_deref()
            .ok_or_else(|| Error::new("invalid_request", "repository_id required"))?;
        let lifecycle = self.repositories.repository_lifecycle(repo);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let binding = self.repositories.checked_binding(r)?;
        if binding.mode != "central" {
            return Err(Error::new(
                "unsupported_feature",
                "Worker synchronization uses artifact import",
            ));
        }
        if !binding.allowed_refs.contains(&p.name) {
            return Err(Error::new(
                "invalid_request",
                "Requested ref is not in the binding's allowed_refs",
            ));
        }
        // The same fetch as repo.sync for this one ref, keyed by this operation id.
        let mut sync = r.clone();
        sync.operation = "repo.sync".into();
        sync.params = json!({"refs":[p.name]});
        let observation = self.repositories.sync_shared(&sync, &binding)?;
        let head = observation["refs"][&p.name]
            .as_str()
            .ok_or_else(|| {
                Error::new("object_unavailable", "Remote does not advertise the ref")
                    .details(json!({"ref":p.name}))
            })?
            .to_owned();
        let synced_at_ms = observation["completed_at_ms"]
            .as_i64()
            .ok_or_else(|| Error::new("recovery_blocked", "Observation has no completion time"))?;
        let observation = RetainedRef {
            format_version: 1,
            name: p.name,
            binding_generation: binding.binding_generation,
            head,
            retention_handle: op.into(),
            synced_at_ms,
        };
        self.retain_observation(repo, &observation)
    }

    fn retain_observation(&self, repo: &str, observation: &RetainedRef) -> Result<Value> {
        let head = &observation.head;
        let op = observation.retention_handle.as_str();
        // Pinning shares the repository's mutation coordination with other mutations.
        let lock = Self::lock(&self.mutation_locks, repo);
        let _mutation = crate::git::lock_with_deadline(&lock)?;
        let _heavy = self.repositories.heavy_budget.acquire()?;
        let state_path = format!(
            "repositories/{repo}/retained-refs/{}.json",
            hex::encode(Sha256::digest(observation.name.as_bytes()))
        );
        let mut last: Option<RetainedRef> = self.root.read_json(&state_path)?;
        if let Some(last) = last
            .as_mut()
            .filter(|last| last.matches(observation) && self.has_retained_head(repo, last))
        {
            // Retain the existing pin, and report this operation's remote
            // observation. Its timestamp also fences an older operation.
            if observation.synced_at_ms > last.synced_at_ms {
                last.synced_at_ms = observation.synced_at_ms;
                self.root.write_json(&state_path, last)?;
            }
            return Ok(observation.answer(&last.retention_handle));
        }
        self.commit_exists(repo, head)?;
        self.pin(operations::Pin {
            repo,
            generation: observation.binding_generation,
            handle: op,
            resource: RESOURCE,
            refs: vec![RetentionRef {
                name: format!("refs/aq-git/retained/{op}"),
                oid: head.clone(),
            }],
        })?;
        // A stale replay of an older operation must not move the record backwards.
        if last
            .as_ref()
            .is_none_or(|l| l.synced_at_ms <= observation.synced_at_ms)
        {
            self.root.write_json(&state_path, observation)?;
        }
        Ok(observation.answer(op))
    }

    fn has_retained_head(&self, repo: &str, record: &RetainedRef) -> bool {
        self.active_handle(repo, &record.retention_handle)
            .is_ok_and(|h| h.resource == RESOURCE && h.refs.iter().all(|x| x.oid == record.head))
    }
}
