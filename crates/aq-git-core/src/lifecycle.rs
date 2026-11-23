mod eviction;

use crate::{
    Error, Result, operations,
    service::RepositoryService,
    state::{OperationStatus, RetentionRecord, RetentionRef, RetentionState},
};
use aq_git_protocol::Request;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
pub const OPERATIONS: &[&str] = &[
    "retention.inspect",
    "retention.release",
    "repo.cache-evict",
    "doctor",
    "maintenance",
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspect {
    #[serde(default)]
    handle_id: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default = "page_size")]
    limit: usize,
}
fn page_size() -> usize {
    100
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    handle_id: String,
    expected_generation: u64,
    refs: Vec<ExpectedRef>,
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedRef {
    name: String,
    oid: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Maintenance {
    #[serde(default)]
    kind: Option<String>,
}
impl RepositoryService {
    pub(crate) fn record_ids(&self, directory: &str) -> Result<Vec<String>> {
        let path = self.root.path().join(directory);
        if !path.exists() {
            return Ok(Vec::new());
        }
        operations::checked_directory(&path)?;
        let mut ids = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(Error::new("invalid_path", "symlink in private metadata"));
            }
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json")
                && let Some(id) = path.file_stem().and_then(|s| s.to_str())
            {
                ids.push(id.into())
            }
        }
        ids.sort();
        Ok(ids)
    }
    fn matching_records(
        &self,
        directory: &str,
        matches: impl Fn(&str) -> Result<bool>,
    ) -> Result<Vec<String>> {
        let mut matched = Vec::new();
        for id in self.record_ids(directory)? {
            if matches(&id)? {
                matched.push(id);
            }
        }
        Ok(matched)
    }
    pub(crate) fn lifecycle_handle(&self, r: &Request) -> Result<Value> {
        if r.operation == "doctor" {
            return self.doctor(r);
        }
        let binding = self.repositories.checked_binding(r)?;
        let repo = &binding.repository_id;
        match r.operation.as_str() {
            "retention.inspect" => self.inspect_retention(r, repo),
            "retention.release" => self.release_retention(r, &binding),
            "repo.cache-evict" => self.evict_pool(r, &binding),
            "maintenance" => self.maintain_pool(r, repo),
            _ => Err(Error::new(
                "unsupported_feature",
                "lifecycle operation unavailable",
            )),
        }
    }
    fn doctor(&self, r: &Request) -> Result<Value> {
        let _: std::collections::BTreeMap<String, Value> =
            serde_json::from_value(r.params.clone())?;
        if !r.params.as_object().is_some_and(|p| p.is_empty()) {
            return Err(Error::new("invalid_request", "doctor takes no params"));
        }
        let mut findings = Vec::new();
        let count = self.diagnose_operations(&mut findings)?;
        for id in self.record_ids("retention")?.into_iter().take(1000) {
            if let Some(record) = self.root.retention(&id)? {
                findings.extend(self.diagnose_retention(&id, &record));
            }
        }
        Ok(
            json!({"state_format":1,"inspected_operations":count,"inspection_limit":1000,"findings":findings,"process_incarnation":self.root.active_incarnation()?}),
        )
    }
    fn diagnose_operations(&self, findings: &mut Vec<Value>) -> Result<usize> {
        let mut count = 0;
        for id in self.record_ids("operations")?.into_iter().take(1000) {
            let Some(record) = self.root.operation(&id)? else {
                continue;
            };
            count += 1;
            if record.status == OperationStatus::Pending {
                findings.push(
                    json!({"kind":"pending_operation","operation_id":id,"phase":record.phase}),
                );
            }
        }
        Ok(count)
    }
    fn diagnose_retention(&self, id: &str, record: &RetentionRecord) -> Option<Value> {
        if record.state != RetentionState::Active {
            return None;
        }
        if !self.repositories.pool_path(&record.repository_id).exists() {
            return Some(json!({"kind":"retained_pool_unavailable","handle_id":id}));
        }
        self.verify_retention_refs(record)
            .err()
            .map(|error| json!({"kind":"retention_ref_mismatch","handle_id":id,"error":error}))
    }
    fn inspect_retention(&self, r: &Request, repo: &str) -> Result<Value> {
        let p: Inspect = serde_json::from_value(r.params.clone())?;
        if !(1..=1000).contains(&p.limit) {
            return Err(Error::new(
                "limit_exceeded",
                "retention page limit must be 1..1000",
            ));
        }
        if let Some(handle) = p.handle_id {
            operations::id(&handle)?;
            let record = self
                .root
                .retention(&handle)?
                .ok_or_else(|| Error::new("retention_not_found", "handle not found"))?;
            if record.repository_id != *repo {
                return Err(Error::new(
                    "retention_not_found",
                    "handle belongs to another repository",
                ));
            }
            return Ok(serde_json::to_value(record)?);
        }
        self.retention_page(repo, p)
    }

    fn retention_page(&self, repo: &str, p: Inspect) -> Result<Value> {
        let mut rows = Vec::new();
        let mut next = None;
        for id in self.record_ids("retention")? {
            if p.after.as_ref().is_some_and(|after| id <= *after) {
                continue;
            }
            let Some(record) = self.root.retention(&id)? else {
                continue;
            };
            if record.repository_id != *repo {
                continue;
            }
            if rows.len() == p.limit {
                next = rows.last().map(|x: &RetentionRecord| x.id.clone());
                break;
            }
            rows.push(record);
        }
        Ok(json!({"handles":rows,"next_after":next}))
    }
    fn release_retention(
        &self,
        r: &Request,
        binding: &crate::repository::Binding,
    ) -> Result<Value> {
        let repo = &binding.repository_id;
        let p: Release = serde_json::from_value(r.params.clone())?;
        operations::id(&p.handle_id)?;
        let expected_refs = checked_refs(p.refs)?;
        if p.reason.trim().is_empty() {
            return Err(Error::new("invalid_request", "release requires a reason"));
        }
        let record = self
            .root
            .retention(&p.handle_id)?
            .ok_or_else(|| Error::new("retention_not_found", "handle not found"))?;
        let same_binding = record.repository_id == *repo
            && record.binding_generation == binding.binding_generation;
        if !same_binding || record.refs != expected_refs {
            return Err(Error::new(
                "retention_conflict",
                "release preconditions do not match exact retained resource",
            ));
        }
        self.check_release_dependencies(&record, &p.handle_id)?;
        Ok(serde_json::to_value(self.release_handle(
            operations::HandleRelease {
                handle: &p.handle_id,
                generation: p.expected_generation,
                operation: operations::opid(r)?,
                reason: &p.reason,
            },
        )?)?)
    }
    fn check_release_dependencies(&self, record: &RetentionRecord, handle_id: &str) -> Result<()> {
        let repo = &record.repository_id;
        if record.resource == "checkpoint" {
            let blockers =
                self.matching_records(&format!("repositories/{repo}/workspaces"), |id| {
                    let workspace = self.workspace_record(repo, id)?;
                    Ok(workspace.lifecycle != "retired"
                        && workspace.last_checkpoint.as_deref() == Some(handle_id))
                })?;
            if !blockers.is_empty() {
                return Err(Error::new(
                    "retention_in_use",
                    "live workspace still uses this checkpoint",
                )
                .details(json!({"blockers":blockers})));
            }
        }
        if record.resource == "workspace" {
            return Err(Error::new(
                "retention_in_use",
                "workspace borrow handles are released by safe workspace retirement",
            ));
        }
        Ok(())
    }
    fn maintain_pool(&self, r: &Request, repo: &str) -> Result<Value> {
        let p: Maintenance = serde_json::from_value(r.params.clone())?;
        let kind = p.kind.as_deref().unwrap_or("indexes");
        if !["indexes", "repack"].contains(&kind) {
            return Err(Error::new(
                "unsupported_feature",
                "only non-pruning index/repack maintenance is enabled",
            ));
        }
        let lifecycle = self.repositories.repository_lifecycle(repo);
        let _drain = crate::git::write_with_deadline(&lifecycle)?;
        self.repositories.invalidate(repo);
        if kind == "repack" {
            self.repositories
                .git(repo, vec!["repack".into(), "-a".into()])?;
        }
        self.repositories.git(
            repo,
            vec!["commit-graph".into(), "write".into(), "--reachable".into()],
        )?;
        Ok(json!({"kind":kind,"pruned":false}))
    }
}

fn checked_refs(refs: Vec<ExpectedRef>) -> Result<Vec<RetentionRef>> {
    let expected_refs: Vec<RetentionRef> = refs
        .into_iter()
        .map(|r| RetentionRef {
            name: r.name,
            oid: r.oid,
        })
        .collect();
    for reference in &expected_refs {
        aq_git_protocol::validate_ref(&reference.name)
            .map_err(|e| Error::new(e.code, e.message))?;
        operations::oid(&aq_git_protocol::ObjectId {
            algorithm: "sha1".into(),
            hex: reference.oid.clone(),
        })?;
    }

    Ok(expected_refs)
}
