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
struct Evict {
    expected_storage_generation: u64,
    reconstructible: bool,
    reason: String,
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
    pub(crate) fn lifecycle_handle(&self, r: &Request) -> Result<Value> {
        if r.operation == "doctor" {
            let _: std::collections::BTreeMap<String, Value> =
                serde_json::from_value(r.params.clone())?;
            if !r.params.as_object().is_some_and(|p| p.is_empty()) {
                return Err(Error::new("invalid_request", "doctor takes no params"));
            }
            let mut findings = Vec::new();
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
            for id in self.record_ids("retention")?.into_iter().take(1000) {
                let Some(record) = self.root.retention(&id)? else {
                    continue;
                };
                if record.state == RetentionState::Active {
                    if !self.repositories.pool_path(&record.repository_id).exists() {
                        findings.push(json!({"kind":"retained_pool_unavailable","handle_id":id}));
                    } else if let Err(error) = self.verify_retention_refs(&record) {
                        findings.push(
                            json!({"kind":"retention_ref_mismatch","handle_id":id,"error":error}),
                        );
                    }
                }
            }
            return Ok(
                json!({"state_format":1,"inspected_operations":count,"inspection_limit":1000,"findings":findings,"process_incarnation":self.root.active_incarnation()?}),
            );
        }
        let binding = self.repositories.checked_binding(r)?;
        let repo = &binding.repository_id;
        match r.operation.as_str() {
            "retention.inspect" => {
                let p: Inspect = serde_json::from_value(r.params.clone())?;
                if p.limit == 0 || p.limit > 1000 {
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
            "retention.release" => {
                let p: Release = serde_json::from_value(r.params.clone())?;
                operations::id(&p.handle_id)?;
                let expected_refs: Vec<RetentionRef> = p
                    .refs
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

                if p.reason.trim().is_empty() {
                    return Err(Error::new("invalid_request", "release requires a reason"));
                }
                let record = self
                    .root
                    .retention(&p.handle_id)?
                    .ok_or_else(|| Error::new("retention_not_found", "handle not found"))?;
                if record.repository_id != *repo
                    || record.binding_generation != binding.binding_generation
                    || record.refs != expected_refs
                {
                    return Err(Error::new(
                        "retention_conflict",
                        "release preconditions do not match exact retained resource",
                    ));
                }
                if record.resource == "checkpoint" {
                    let mut blockers = Vec::new();
                    for id in self.record_ids(&format!("repositories/{repo}/workspaces"))? {
                        let workspace = self.workspace_record(repo, &id)?;
                        if workspace.lifecycle != "retired"
                            && workspace.last_checkpoint.as_deref() == Some(p.handle_id.as_str())
                        {
                            blockers.push(id);
                        }
                    }
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
                Ok(serde_json::to_value(self.release_handle(
                    &p.handle_id,
                    p.expected_generation,
                    operations::opid(r)?,
                    &p.reason,
                )?)?)
            }
            "repo.cache-evict" => {
                let p: Evict = serde_json::from_value(r.params.clone())?;
                let op = operations::opid(r)?;
                if binding.mode != "worker" {
                    return Err(Error::new(
                        "unsupported_feature",
                        "central retained pools cannot be cache-evicted",
                    ));
                }
                if !p.reconstructible || p.reason.trim().is_empty() {
                    return Err(Error::new(
                        "invalid_request",
                        "eviction requires reconstructible designation and reason",
                    ));
                }
                let lifecycle = self.repositories.repository_lifecycle(repo);
                let _drain = crate::git::write_with_deadline(&lifecycle)?;
                let mut blockers = Vec::new();
                for id in self.record_ids("retention")? {
                    if let Some(handle) = self.root.retention(&id)?
                        && handle.repository_id == *repo
                        && handle.state != RetentionState::Released
                    {
                        blockers.push(id)
                    }
                }
                for id in self.record_ids(&format!("repositories/{repo}/workspaces"))? {
                    let workspace = self.workspace_record(repo, &id)?;
                    if workspace.lifecycle != "retired" {
                        blockers.push(id);
                    }
                }
                for id in self.record_ids("operations")? {
                    if id == op {
                        continue;
                    }
                    if let Some(record) = self.root.operation(&id)?
                        && record.status == OperationStatus::Pending
                        && record.payload["repository_id"] == *repo
                    {
                        blockers.push(id)
                    }
                }
                if !blockers.is_empty() {
                    return Err(Error::new(
                        "retention_in_use",
                        "pool eviction has live dependencies",
                    )
                    .details(json!({"blockers":blockers})));
                }
                let file = format!("repositories/{repo}/pool.json");
                let mut pool: Value = self
                    .root
                    .read_json(&file)?
                    .ok_or_else(|| Error::new("object_unavailable", "pool metadata unavailable"))?;
                let current = pool["storage_generation"]
                    .as_u64()
                    .ok_or_else(|| Error::new("state_incompatible", "invalid pool generation"))?;
                if pool["format_version"] != 1
                    || current == 0
                    || !matches!(
                        pool["lifecycle"].as_str(),
                        Some("present" | "evicting" | "absent")
                    )
                {
                    return Err(Error::new("state_incompatible", "unsupported pool record"));
                }
                let next = p
                    .expected_storage_generation
                    .checked_add(1)
                    .ok_or_else(|| Error::new("invalid_request", "storage generation exhausted"))?;
                let resuming = pool["operation_id"] == op;
                if (!resuming && current != p.expected_storage_generation)
                    || (resuming
                        && ((pool["lifecycle"] == "evicting"
                            && current != p.expected_storage_generation)
                            || (pool["lifecycle"] == "absent" && current != next)
                            || pool["lifecycle"] == "present"))
                {
                    return Err(Error::new(
                        "binding_conflict",
                        "storage generation or eviction phase changed",
                    ));
                }
                if !resuming && pool["lifecycle"] != "present" {
                    return Err(Error::new(
                        "recovery_blocked",
                        "pool is not available for a new eviction",
                    ));
                }
                let trash = self.root.path().join("trash").join(format!(
                    "pool-{repo}-{}-{op}",
                    p.expected_storage_generation
                ));
                let path = self.repositories.pool_path(repo);
                if !resuming {
                    operations::checked_directory(&path).map_err(|_| {
                        Error::new(
                            "recovery_blocked",
                            "managed pool missing without eviction intent",
                        )
                    })?;
                    let marker = json!({"format_version":1,"repository_id":repo,"storage_generation":current,"operation_id":op});
                    let marker_path =
                        format!("repositories/{repo}/objects.git/aq-git-eviction.json");
                    if let Some(existing) = self.root.read_json::<Value>(&marker_path)? {
                        if existing != marker {
                            return Err(Error::new(
                                "recovery_blocked",
                                "pool has another eviction ownership marker",
                            ));
                        }
                    } else {
                        self.root.write_json(&marker_path, &marker)?;
                    }
                    pool["eviction_marker"] = marker;
                    if trash.symlink_metadata().is_ok() {
                        return Err(Error::new(
                            "recovery_blocked",
                            "eviction trash already exists before intent",
                        ));
                    }
                    use std::os::unix::fs::MetadataExt;
                    let identity = fs::symlink_metadata(&path)?;
                    pool["eviction_identity"] =
                        json!({"device":identity.dev(),"inode":identity.ino()});
                    pool["lifecycle"] = json!("evicting");
                    pool["operation_id"] = json!(op);
                    self.root.write_json(&file, &pool)?;
                }
                if pool["lifecycle"] == "evicting" {
                    self.repositories.invalidate(repo);
                    self.root.create_dir("trash")?;
                    let original = path.symlink_metadata().is_ok();
                    let trashed = trash.symlink_metadata().is_ok();
                    if original == trashed {
                        return Err(Error::new(
                            "recovery_blocked",
                            "eviction requires exactly one original/trash directory",
                        ));
                    }
                    let source = if original { &path } else { &trash };
                    verify_eviction_directory(source, &pool)?;
                    if original {
                        fs::rename(&path, &trash)?;
                        operations::sync_dir(path.parent().unwrap())?;
                        operations::sync_dir(trash.parent().unwrap())?;
                    }
                    pool["storage_generation"] = json!(next);
                    pool["lifecycle"] = json!("absent");
                    self.root.write_json(&file, &pool)?;
                }
                if path.symlink_metadata().is_ok() {
                    return Err(Error::new(
                        "recovery_blocked",
                        "replacement pool exists during recorded eviction",
                    ));
                }
                if trash.exists() {
                    verify_eviction_directory(&trash, &pool)?;
                    fs::remove_dir_all(&trash)?;
                    operations::sync_dir(trash.parent().unwrap())?;
                }
                Ok(pool)
            }
            "maintenance" => {
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
            _ => Err(Error::new(
                "unsupported_feature",
                "lifecycle operation unavailable",
            )),
        }
    }
}

fn verify_eviction_directory(path: &std::path::Path, pool: &Value) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    operations::checked_directory(path)?;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let marker = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("aq-git-eviction.json"))?;
    if !marker.metadata()?.is_file() {
        return Err(Error::new(
            "recovery_blocked",
            "eviction ownership marker is not regular",
        ));
    }
    let marker: Value = serde_json::from_reader(marker.take(4097))?;
    if marker != pool["eviction_marker"] {
        return Err(Error::new(
            "recovery_blocked",
            "eviction ownership marker differs from recorded intent",
        ));
    }
    let meta = fs::symlink_metadata(path)?;
    if pool["eviction_identity"]["device"].as_u64() != Some(meta.dev())
        || pool["eviction_identity"]["inode"].as_u64() != Some(meta.ino())
    {
        return Err(Error::new(
            "recovery_blocked",
            "eviction directory differs from recorded device/inode",
        ));
    }
    Ok(())
}
