//! Git smart-HTTP endpoint for task attempts (Epic 0, spec 0016 section 5.1).
//!
//! aq-server authenticates a worker and forwards its fetch/push requests over a
//! private Unix socket, each carrying a *grant* in `AQ-*` request headers. This
//! module trusts the socket, validates the grant's shape, serves the request with
//! `git upload-pack` / `git receive-pack --stateless-rpc` on the repository's
//! central pool and, for a push, enforces the grant through git's own quarantine
//! (a `pre-receive` hook this process installs). An accepted push is pinned by a
//! retention handle and recorded in a receipt `push.receipt` reads back.
//! The endpoint decides nothing about delivery: it enforces what the grant says.
mod hook;
mod http;
mod journal;
mod server;
mod stage;

pub use hook::run_hook_if_requested;
pub use server::GitHttpServer;

use crate::{Error, Result, service::RepositoryService};
use aq_git_protocol::validate_ref;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Read operations this module adds to the JSON protocol.
pub const OPERATIONS: &[&str] = &["push.receipt"];
pub(crate) const ZERO_OID: &str = "0000000000000000000000000000000000000000";

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
    /// Absolute time for a whole request, body upload included. A slow body cannot
    /// hold a connection (or anything it took) past it.
    pub request_deadline_secs: u64,
    /// Bytes all connections together may buffer in memory (fetch requests, Git output).
    pub max_buffered_bytes: u64,
}
impl Default for GitHttpLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024,
            max_upload_request_bytes: 64 * 1024 * 1024,
            max_connections: 32,
            io_timeout_secs: 60,
            git_timeout_secs: 3600,
            request_deadline_secs: 3600,
            max_buffered_bytes: 256 * 1024 * 1024,
        }
    }
}
impl GitHttpLimits {
    pub fn load(service: &RepositoryService) -> Result<Self> {
        let limits: Self = service
            .root
            .read_json("git_http_limits.json")?
            .unwrap_or_default();
        if limits.max_header_bytes < 1024
            || limits.max_upload_request_bytes == 0
            || limits.max_connections == 0
            || limits.io_timeout_secs == 0
            || limits.git_timeout_secs == 0
            || limits.request_deadline_secs == 0
            || limits.max_buffered_bytes == 0
        {
            return Err(Error::new(
                "invalid_configuration",
                "git_http_limits.json values must be positive (header bytes at least 1024)",
            ));
        }
        Ok(limits)
    }
}

/// What aq-server allows one request to do. Parsed from the `AQ-*` headers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub repository: String,
    pub base: String,
    pub task_ref: String,
    pub lease: Option<String>,
    pub main_ref: String,
    pub max_push_bytes: u64,
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
        let main_ref = need("aq-main-ref")?;
        for (name, value) in [("AQ-Task-Ref", task_ref), ("AQ-Main-Ref", main_ref)] {
            validate_ref(value).map_err(|_| bad(format!("{name} is not a valid ref")))?;
            if !value.starts_with("refs/heads/") {
                return Err(bad(format!("{name} must be a branch under refs/heads/")));
            }
        }
        if !task_ref.starts_with("refs/heads/aq/") {
            return Err(bad("AQ-Task-Ref must be under refs/heads/aq/".into()));
        }
        if task_ref == main_ref {
            return Err(bad("AQ-Task-Ref must differ from AQ-Main-Ref".into()));
        }
        let lease = match get("aq-task-ref-lease")? {
            None => None,
            Some(v) if is_oid(v) => Some(v.to_owned()),
            Some(_) => {
                return Err(bad(
                    "AQ-Task-Ref-Lease must be 40 lowercase hex digits or absent".into(),
                ));
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
            lease,
            main_ref: main_ref.into(),
            max_push_bytes,
            operation: operation.into(),
        })
    }
}

/// What the `reference-transaction` hook writes, durably, before the task ref moves.
/// The file exists until the retention pin and the receipt are durable, so a crash
/// between the ref update and the receipt is repaired by [`journal::reconcile`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PushJournal {
    pub repository: String,
    pub operation_id: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub old: String,
    pub new: String,
    pub base: String,
    pub retention_handle: String,
    pub binding_generation: u64,
}
/// The pack files a push brought into the pool, written by `pre-receive` just before Git
/// promotes them. If the push then fails without moving the ref, they are removed again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PushPromotion {
    pub repository: String,
    pub new: String,
    pub files: Vec<String>,
}
pub(crate) fn promotion_file(operation: &str) -> String {
    format!("{operation}.promoted")
}
pub(crate) const JOURNAL_DIR: &str = "git-http/journal";
pub(crate) fn journal_file(operation: &str) -> String {
    format!("{operation}.json")
}

/// Receipt of an accepted push, read back by `push.receipt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushReceipt {
    pub repository: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub old: Option<String>,
    pub new: String,
    pub base: String,
    pub retention_handle: String,
    pub operation_id: String,
}
pub(crate) fn receipt_path(operation: &str) -> String {
    format!("push-receipts/{operation}.json")
}
impl RepositoryService {
    /// `push.receipt {operation_id}`: the receipt with the retention handle's live state.
    pub(crate) fn push_receipt(&self, r: &aq_git_protocol::Request) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            operation_id: String,
        }
        let params: Params = serde_json::from_value(r.params.clone())?;
        crate::operations::id(&params.operation_id)?;
        let not_found = || Error::new("push_receipt_not_found", "no push receipt for operation");
        let receipt: PushReceipt = self
            .root
            .read_json(receipt_path(&params.operation_id))?
            .ok_or_else(not_found)?;
        if r.repository_id
            .as_deref()
            .is_some_and(|id| id != receipt.repository)
        {
            return Err(not_found());
        }
        let handle = self.root.retention(&receipt.retention_handle)?;
        let mut value = serde_json::to_value(&receipt)?;
        value["retention_state"] = json!(handle.map(|h| h.state));
        Ok(value)
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
            ("aq-main-ref", "refs/heads/main"),
            ("aq-max-push-bytes", "1000"),
            ("aq-operation", "0b0f1c2e-3a4b-4c5d-8e9f-a0b1c2d3e4f5"),
        ];
        all.extend_from_slice(extra);
        all.into_iter()
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .collect()
    }
    #[test]
    fn grant_parses_with_and_without_lease() {
        let grant = Grant::from_headers(&headers(&[])).unwrap();
        assert_eq!(grant.lease, None);
        assert_eq!(grant.max_push_bytes, 1000);
        let lease = "b".repeat(40);
        let grant = Grant::from_headers(&headers(&[("aq-task-ref-lease", &lease)])).unwrap();
        assert_eq!(grant.lease.as_deref(), Some(lease.as_str()));
    }
    #[test]
    fn grant_rejects_malformed_or_duplicate_headers() {
        for extra in [
            ("aq-base", "abc"),
            ("aq-task-ref-lease", "XYZ"),
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
