//! Git smart-HTTP endpoint for the project repository (Epic 0, spec 0016 section 5.1).
//!
//! aq-server authenticates a worker and forwards its fetch/push requests over a
//! private Unix socket, each carrying a *grant* in `AQ-*` request headers. This
//! module trusts the socket, validates the grant's shape and serves the request with
//! `git upload-pack` / `git receive-pack --stateless-rpc` on the repository's
//! central bare repository. It is a plain Git server: fetch sees `refs/heads/main`,
//! the `refs/heads/aq/*` task branches and the `refs/heads/aq-try/*` try refs; a
//! push goes straight into the repository under Git's own safety (fsck, input size
//! limit, quarantine) and a minimal `pre-receive` hook that admits only the grant's
//! task branch and, when granted, the attempt's own try ref. The branch ref is the
//! record of a result: there is no receipt and no retention of pushes. A try ref is
//! an attempt's work in progress for a builder; it is never published.
//! The endpoint decides nothing about delivery.
mod hook;
mod http;
mod server;

pub use hook::run_hook_if_requested;
pub use server::GitHttpServer;

use crate::{Error, Result, service::RepositoryService};
use aq_git_protocol::validate_ref;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Read operations this module adds to the JSON protocol.
pub const OPERATIONS: &[&str] = &["ref.show"];
pub(crate) const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// The branch every task starts from, advertised read-only.
pub(crate) const MAIN_REF: &str = "refs/heads/main";
/// Namespace of the task branches; a push may update only the grant's branch in it.
pub(crate) const TASK_PREFIX: &str = "refs/heads/aq/";
/// Namespace of the per-attempt try refs (`refs/heads/aq-try/<attempt-id>`).
/// Local to the central repository: never published, deletable only locally.
pub(crate) const TRY_PREFIX: &str = "refs/heads/aq-try/";

/// Whether `name` is exactly `refs/heads/aq-try/<canonical-uuid>`.
pub(crate) fn is_try_ref(name: &str) -> bool {
    name.strip_prefix(TRY_PREFIX)
        .is_some_and(|id| aq_git_protocol::validate_id(id).is_ok())
}

/// Settings read from `git_http_limits.json` in the state root.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitHttpLimits {
    /// Bytes allowed for a request line plus all headers.
    pub max_header_bytes: usize,
    /// Decoded bytes allowed in an upload-pack (fetch negotiation) request.
    pub max_upload_request_bytes: u64,
    /// Concurrent connections served; further connections get 503.
    pub max_connections: usize,
    /// Socket read/write inactivity timeout.
    pub io_timeout_secs: u64,
    /// Total time one Git child may run for a request.
    pub git_timeout_secs: u64,
    /// Seconds between `git gc --auto` passes over the repositories; 0 disables them.
    pub gc_interval_secs: u64,
    /// Git's `gc.auto`: loose objects that make `git gc --auto` pack the repository.
    pub gc_auto_objects: u64,
}
impl Default for GitHttpLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024,
            max_upload_request_bytes: 64 * 1024 * 1024,
            max_connections: 32,
            io_timeout_secs: 60,
            git_timeout_secs: 3600,
            gc_interval_secs: 3600,
            gc_auto_objects: 6700,
        }
    }
}
impl GitHttpLimits {
    pub fn load(service: &RepositoryService) -> Result<Self> {
        let limits: Self = service
            .root
            .read_json("git_http_limits.json")?
            .unwrap_or_default();
        let required_limits = [
            limits.max_connections as u64,
            limits.max_upload_request_bytes,
            limits.io_timeout_secs,
            limits.git_timeout_secs,
            limits.gc_auto_objects,
        ];
        if limits.max_header_bytes < 1024 || required_limits.contains(&0) {
            return Err(Error::new(
                "invalid_configuration",
                "git_http_limits.json values must be positive (header bytes at least 1024; gc_interval_secs may be 0)",
            ));
        }
        Ok(limits)
    }
}

/// What aq-server allows one request to do. Parsed from the `AQ-*` headers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub repository: String,
    /// The commit the task started from, as aq-server recorded it. Not enforced.
    pub base: String,
    /// `refs/heads/aq/<task-id>`: the only ref a push may update.
    pub task_ref: String,
    /// `refs/heads/aq-try/<attempt-id>`: the attempt's try ref a push may also
    /// create or force-update, when aq-server grants one.
    #[serde(default)]
    pub try_ref: Option<String>,
    pub max_push_bytes: u64,
    /// Identifies the request; aq-git keeps no record under it.
    pub operation: String,
}
fn is_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Grant {
    /// `headers` holds lower-case names. Every grant header must appear once.
    pub fn from_headers(headers: &[(String, String)]) -> Result<Self> {
        let bad = |message: String| Error::new("invalid_grant", message);
        let get = |name: &str| -> Result<Option<&str>> {
            let mut found = headers.iter().filter(|(n, _)| n == name);
            let first = found.next().map(|(_, v)| v.trim());
            if found.next().is_some() {
                return Err(bad(format!("duplicate grant header {name}")));
            }
            Ok(first)
        };
        let need = |name: &str| -> Result<&str> {
            get(name)?
                .filter(|v| !v.is_empty())
                .ok_or_else(|| bad(format!("missing grant header {name}")))
        };
        let repository = need("aq-repository")?;
        aq_git_protocol::validate_id(repository)
            .map_err(|_| bad("AQ-Repository must be a canonical UUID".into()))?;
        let base = need("aq-base")?;
        if !is_oid(base) {
            return Err(bad("AQ-Base must be 40 lowercase hex digits".into()));
        }
        let task_ref = need("aq-task-ref")?;
        validate_ref(task_ref).map_err(|_| bad("AQ-Task-Ref is not a valid ref".into()))?;
        if !task_ref.starts_with(TASK_PREFIX) {
            return Err(bad(format!("AQ-Task-Ref must be under {TASK_PREFIX}")));
        }
        let try_ref = match get("aq-try-ref")? {
            None => None,
            Some(value) if is_try_ref(value) => Some(value.to_owned()),
            Some(_) => {
                return Err(bad(format!("AQ-Try-Ref must be {TRY_PREFIX}<uuid>")));
            }
        };
        let max_push_bytes = need("aq-max-push-bytes")?
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| bad("AQ-Max-Push-Bytes must be a positive integer".into()))?;
        let operation = need("aq-operation")?;
        aq_git_protocol::validate_id(operation)
            .map_err(|_| bad("AQ-Operation must be a canonical UUID".into()))?;
        Ok(Self {
            repository: repository.into(),
            base: base.into(),
            task_ref: task_ref.into(),
            try_ref,
            max_push_bytes,
            operation: operation.into(),
        })
    }
}

impl RepositoryService {
    /// `ref.show {ref}`: the branch's commit in the central repository, or null when absent.
    pub(crate) fn ref_show(&self, r: &aq_git_protocol::Request) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            #[serde(rename = "ref")]
            name: String,
        }
        let params: Params = serde_json::from_value(r.params.clone())?;
        validate_ref(&params.name).map_err(|e| Error::new(e.code, e.message))?;
        if !params.name.starts_with("refs/heads/") {
            return Err(Error::new("invalid_request", "ref must be a branch"));
        }
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
                "worker repositories have no branches",
            ));
        }
        let oid = self.pool_ref(repo, &params.name)?;
        Ok(json!({"ref":params.name,"oid":oid.as_deref().map(crate::operations::wire_oid)}))
    }
}

/// Last head `repo.sync` gave `refs/heads/main`, so an older sync cannot move it back.
#[derive(Serialize, Deserialize)]
struct MainHead {
    binding_generation: u64,
    started_at_ms: i64,
    head: String,
}
/// A move of the repository's real `refs/heads/main` (which fetch serves) to a
/// synced head, made inside the sync's ref transaction. Holds the state root's
/// lock until [`finish`](Self::finish) records it.
pub(crate) struct MainAdvance<'a> {
    _guard: std::sync::MutexGuard<'a, ()>,
    path: String,
    record: MainHead,
}
impl crate::repository::RepositoryManager {
    /// The move of `refs/heads/main` for a sync that observed `refs`, unless the
    /// sync did not cover main or one that started later already moved it.
    pub(crate) fn plan_main_advance(
        &self,
        b: &crate::repository::Binding,
        refs: &std::collections::BTreeMap<String, String>,
        started_at_ms: i64,
    ) -> Result<Option<MainAdvance<'_>>> {
        let Some(head) = refs.get(MAIN_REF) else {
            return Ok(None);
        };
        let path = format!("repositories/{}/main-head.json", b.repository_id);
        let guard = crate::git::lock_with_deadline(&self.root.inner.mutex)?;
        let last: Option<MainHead> = self.root.read_json(&path)?;
        if last.as_ref().is_some_and(|last| {
            last.binding_generation == b.binding_generation && last.started_at_ms > started_at_ms
        }) {
            return Ok(None);
        }
        Ok(Some(MainAdvance {
            _guard: guard,
            path,
            record: MainHead {
                binding_generation: b.binding_generation,
                started_at_ms,
                head: head.clone(),
            },
        }))
    }
}
impl MainAdvance<'_> {
    /// The `git update-ref --stdin` instruction.
    pub(crate) fn instruction(&self) -> String {
        format!("update {MAIN_REF} {}\n", self.record.head)
    }
    /// Records the move after the transaction committed.
    pub(crate) fn finish(self, root: &crate::state::StateRoot) -> Result<()> {
        root.write_json(&self.path, &self.record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut all = vec![
            ("aq-repository", "7d1f2ad4-0f0b-4c5e-9a46-2b1c1b7a9d11"),
            ("aq-base", "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678"),
            ("aq-task-ref", "refs/heads/aq/task-1"),
            ("aq-max-push-bytes", "1000"),
            ("aq-operation", "0b0f1c2e-3a4b-4c5d-8e9f-a0b1c2d3e4f5"),
        ];
        all.extend_from_slice(extra);
        all.into_iter()
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .collect()
    }
    #[test]
    fn grant_parses_and_ignores_other_headers() {
        let grant = Grant::from_headers(&headers(&[("aq-main-ref", "refs/heads/main")])).unwrap();
        assert_eq!(grant.task_ref, "refs/heads/aq/task-1");
        assert_eq!(grant.max_push_bytes, 1000);
        assert_eq!(grant.try_ref, None);
    }
    const ATTEMPT: &str = "5e3c2b1a-9d8e-4f7a-b6c5-d4e3f2a1b0c9";
    #[test]
    fn grant_parses_a_valid_try_ref() {
        let value = format!("{TRY_PREFIX}{ATTEMPT}");
        let grant =
            Grant::from_headers(&headers(&[("aq-try-ref", &format!(" {value} "))])).unwrap();
        assert_eq!(grant.try_ref.as_deref(), Some(value.as_str()));
        assert_eq!(grant.task_ref, "refs/heads/aq/task-1");
        let json = serde_json::to_string(&grant).unwrap();
        assert_eq!(serde_json::from_str::<Grant>(&json).unwrap(), grant);
        let mut legacy: Value = serde_json::from_str(&json).unwrap();
        legacy.as_object_mut().unwrap().remove("try_ref");
        let parsed: Grant = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.try_ref, None);
    }
    #[test]
    fn grant_rejects_malformed_or_duplicate_try_refs() {
        let valid = format!("{TRY_PREFIX}{ATTEMPT}");
        let duplicate = headers(&[("aq-try-ref", &valid), ("aq-try-ref", &valid)]);
        let error = Grant::from_headers(&duplicate).unwrap_err();
        assert_eq!(error.code, "invalid_grant");
        for value in [
            String::new(),
            format!("{TASK_PREFIX}{ATTEMPT}"),
            TRY_PREFIX.to_owned(),
            format!("{TRY_PREFIX}{}", ATTEMPT.to_uppercase()),
            format!("{valid}/x"),
            format!("{valid}/"),
            format!("{TRY_PREFIX}not-a-uuid"),
            format!("refs/heads/other/{ATTEMPT}"),
            format!("{TRY_PREFIX}{}", ATTEMPT.replace('-', "")),
        ] {
            let error = Grant::from_headers(&headers(&[("aq-try-ref", &value)])).unwrap_err();
            assert_eq!(error.code, "invalid_grant", "{value}");
            assert_eq!(
                error.message, "AQ-Try-Ref must be refs/heads/aq-try/<uuid>",
                "{value}"
            );
        }
    }
    #[test]
    fn grant_rejects_malformed_or_duplicate_headers() {
        for extra in [
            ("aq-base", "abc"),
            ("aq-operation", "0b0f1c2e-3a4b-4c5d-8e9f-a0b1c2d3e4f5"),
            ("aq-base", "A1B2C3D4E5F60718293A4B5C6D7E8F9012345678"),
        ] {
            assert!(
                Grant::from_headers(&headers(&[extra])).is_err(),
                "{extra:?}"
            );
        }
        let mut missing = headers(&[]);
        missing.retain(|(n, _)| n != "aq-operation");
        assert!(Grant::from_headers(&missing).is_err());
        let mut bad_ref = headers(&[]);
        bad_ref.retain(|(n, _)| n != "aq-task-ref");
        for value in [
            "refs/heads/main",
            "refs/tags/aq/x",
            "refs/heads/aq/../x",
            "refs/heads/other",
        ] {
            let mut h = bad_ref.clone();
            h.push(("aq-task-ref".into(), value.into()));
            assert!(Grant::from_headers(&h).is_err(), "{value}");
        }
        let mut zero = headers(&[]);
        zero.retain(|(n, _)| n != "aq-max-push-bytes");
        zero.push(("aq-max-push-bytes".into(), "0".into()));
        assert!(Grant::from_headers(&zero).is_err());
    }
}
