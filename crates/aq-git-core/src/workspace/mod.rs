use crate::{
    Error, Result,
    operations::{self, CommitMetadata},
    service::RepositoryService,
    state::RetentionRef,
};
use aq_git_protocol::{ObjectId, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};

pub const OPERATIONS: &[&str] = &[
    "workspace.create",
    "workspace.inspect",
    "workspace.checkpoint",
    "workspace.retire",
];
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub format_version: u32,
    pub workspace_id: String,
    pub repository_id: String,
    pub binding_generation: u64,
    pub generation: u64,
    pub base: String,
    pub path: PathBuf,
    pub lifecycle: String,
    pub allocating_operation: String,
    pub last_checkpoint: Option<String>,
    pub retiring_operation: Option<String>,
    pub config_digest: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    base: ObjectId,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspect {
    workspace_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    workspace_id: String,
    expected_generation: u64,
    expected_head: ObjectId,
    quiescent: bool,
    #[serde(default = "head_mode")]
    mode: String,
    #[serde(default)]
    commit: Option<CommitMetadata>,
}
fn head_mode() -> String {
    "head".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retire {
    workspace_id: String,
    expected_generation: u64,
    expected_head: ObjectId,
    quiescent: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CheckpointRecord {
    format_version: u32,
    old_checkpoint: Option<String>,
    index_tree_before: String,
    source_tree: String,
    operation_id: String,
    workspace_id: String,
    old_head: String,
    source: String,
    workspace_generation: u64,
    completed: bool,
    result: Value,
}
impl RepositoryService {
    pub(crate) fn workspace_record(&self, repo: &str, workspace: &str) -> Result<WorkspaceRecord> {
        operations::id(workspace)?;
        let record: WorkspaceRecord = self
            .root
            .read_json(format!("repositories/{repo}/workspaces/{workspace}.json"))?
            .ok_or_else(|| Error::new("workspace_not_found", "workspace not found"))?;
        if record.format_version != 1 {
            return Err(Error::new(
                "state_incompatible",
                "unsupported workspace format",
            ));
        }
        if record.workspace_id != workspace
            || record.repository_id != repo
            || record.path != self.root.path().join("workspaces").join(workspace)
            || record.allocating_operation != workspace
            || (record.lifecycle != "retired"
                && record.binding_generation != self.repositories.binding(repo)?.binding_generation)
            || !matches!(
                record.lifecycle.as_str(),
                "allocating" | "active" | "retiring" | "retired"
            )
        {
            return Err(Error::new(
                "recovery_blocked",
                "workspace record identity/path/lifecycle differs from managed allocation",
            ));
        }
        Ok(record)
    }
    fn save_workspace(&self, w: &WorkspaceRecord) -> Result<()> {
        self.root.write_json(
            format!(
                "repositories/{}/workspaces/{}.json",
                w.repository_id, w.workspace_id
            ),
            w,
        )
    }
    fn workspace_integrity(&self, w: &WorkspaceRecord) -> Result<()> {
        self.workspace_integrity_at(w, true)
    }
    fn workspace_integrity_at(&self, w: &WorkspaceRecord, verify_borrow: bool) -> Result<()> {
        operations::checked_directory(&w.path)?;
        for name in [
            "commondir",
            "gitdir",
            "shallow",
            "config.worktree",
            "info/attributes",
        ] {
            if w.path.join(".git").join(name).symlink_metadata().is_ok() {
                return Err(Error::new(
                    "workspace_changed",
                    "unsupported workspace Git metadata",
                ));
            }
        }
        let relative = w
            .path
            .strip_prefix(self.root.path())
            .map_err(|_| Error::new("workspace_changed", "workspace outside managed root"))?;
        let owner: Value = self
            .root
            .read_json(relative.join(".git/aq-git-owner.json"))?
            .ok_or_else(|| Error::new("workspace_changed", "workspace ownership marker missing"))?;
        if owner
            != json!({"workspace_id":w.workspace_id,"repository_id":w.repository_id,"base":w.base})
        {
            return Err(Error::new(
                "workspace_changed",
                "workspace ownership marker differs",
            ));
        }
        if verify_borrow {
            if let Some(checkpoint) = &w.last_checkpoint {
                let latest = self.active_handle(&w.repository_id, checkpoint)?;
                if !latest.dependencies.contains(&w.workspace_id) {
                    return Err(Error::new(
                        "recovery_blocked",
                        "latest checkpoint lost its workspace dependency",
                    ));
                }
            }
            let borrow = self.active_handle(&w.repository_id, &w.workspace_id)?;
            if borrow.resource != "workspace"
                || borrow.refs
                    != vec![RetentionRef {
                        name: format!("refs/aq-git/workspaces/{}", w.workspace_id),
                        oid: w.base.clone(),
                    }]
            {
                return Err(Error::new(
                    "recovery_blocked",
                    "workspace borrow differs from allocation",
                ));
            }
        }
        operations::checked_directory(&w.path.join(".git"))?;
        operations::checked_directory(&w.path.join(".git/objects"))?;
        operations::checked_directory(&w.path.join(".git/objects/info"))?;
        operations::checked_directory(&w.path.join(".git/refs"))?;
        for path in [
            w.path.join(".git/HEAD"),
            w.path.join(".git/index"),
            w.path.join(".git/config"),
            w.path.join(".git/objects/info/alternates"),
        ] {
            if !fs::symlink_metadata(&path)?.is_file()
                || fs::symlink_metadata(&path)?.file_type().is_symlink()
            {
                return Err(Error::new(
                    "workspace_changed",
                    "workspace Git metadata was replaced",
                ));
            }
        }
        validate_private_refs(&w.path.join(".git/refs"))?;
        if self.workspace_config_digest(&w.path)? != w.config_digest {
            return Err(Error::new(
                "workspace_changed",
                "workspace managed config or alternates changed",
            ));
        }
        Ok(())
    }
    fn workspace_config_digest(&self, path: &std::path::Path) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(fs::read(path.join(".git/config"))?);
        hash.update([0]);
        hash.update(fs::read(path.join(".git/objects/info/alternates"))?);
        Ok(hex::encode(hash.finalize()))
    }
    fn workspace_head(&self, w: &WorkspaceRecord) -> Result<String> {
        let data = self.git_at(
            &w.path,
            vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
        )?;
        Ok(String::from_utf8_lossy(&data).trim().into())
    }
    fn workspace_status(&self, w: &WorkspaceRecord) -> Result<Vec<u8>> {
        self.git_at(
            &w.path,
            vec![
                "status".into(),
                "--porcelain=v2".into(),
                "-z".into(),
                "--untracked-files=all".into(),
            ],
        )
    }
    pub(crate) fn workspace_handle(&self, r: &Request) -> Result<Value> {
        let binding = self.repositories.checked_binding(r)?;
        let repo = &binding.repository_id;
        if matches!(
            r.operation.as_str(),
            "workspace.checkpoint" | "workspace.retire"
        ) && let Some(workspace) = r.params["workspace_id"].as_str()
        {
            for id in self.record_ids("operations")? {
                if Some(id.as_str()) == r.operation_id.as_deref() {
                    continue;
                }
                let Some(operation) = self.root.operation(&id)? else {
                    continue;
                };
                if operation.status == crate::state::OperationStatus::Pending
                    && operation.payload["repository_id"] == *repo
                    && ((operation.payload["operation"] == "workspace.create" && id == workspace)
                        || (matches!(
                            operation.payload["operation"].as_str(),
                            Some("workspace.checkpoint" | "workspace.retire")
                        ) && operation.payload["params"]["workspace_id"] == workspace))
                {
                    return Err(Error::new(
                        "retention_in_use",
                        "workspace has a pending mutation that must be replayed first",
                    )
                    .details(json!({"blockers":[id]})));
                }
            }
        }
        match r.operation.as_str() {
            "workspace.create" => {
                let p: Create = serde_json::from_value(r.params.clone())?;
                let base = operations::oid(&p.base)?;
                let wid = operations::opid(r)?;
                self.commit_exists(repo, base)?;
                self.validate_tree_features(repo, base)?;
                let final_path = self.root.path().join("workspaces").join(wid);
                let temporary = self.root.path().join("allocating").join(wid);
                let mut w = if let Some(w) = self.root.read_json::<WorkspaceRecord>(format!(
                    "repositories/{repo}/workspaces/{wid}.json"
                ))? {
                    if w.format_version != 1
                        || w.workspace_id != wid
                        || w.repository_id != *repo
                        || w.path != final_path
                        || w.base != base
                        || w.binding_generation != binding.binding_generation
                        || w.allocating_operation != wid
                    {
                        return Err(Error::new(
                            "recovery_blocked",
                            "allocation record differs from durable request",
                        ));
                    }
                    w
                } else {
                    let w = WorkspaceRecord {
                        format_version: 1,
                        workspace_id: wid.into(),
                        repository_id: repo.into(),
                        binding_generation: binding.binding_generation,
                        generation: 1,
                        base: base.into(),
                        path: final_path.clone(),
                        lifecycle: "allocating".into(),
                        allocating_operation: wid.into(),
                        last_checkpoint: None,
                        retiring_operation: None,
                        config_digest: String::new(),
                    };
                    self.save_workspace(&w)?;
                    w
                };
                if w.lifecycle == "active" {
                    self.workspace_integrity(&w)?;
                    return Ok(serde_json::to_value(w)?);
                }
                if w.lifecycle != "allocating" {
                    return Err(Error::new(
                        "recovery_blocked",
                        "workspace allocation has incompatible lifecycle",
                    ));
                }
                self.pin(
                    repo,
                    binding.binding_generation,
                    wid,
                    "workspace",
                    vec![RetentionRef {
                        name: format!("refs/aq-git/workspaces/{wid}"),
                        oid: base.into(),
                    }],
                )?;
                if !final_path.exists() {
                    self.root.create_dir(format!("allocating/{wid}"))?;
                    if temporary.join(".git").symlink_metadata().is_ok() {
                        operations::checked_directory(&temporary.join(".git"))?;
                    }
                    self.git_at(
                        &temporary,
                        vec!["init".into(), "--object-format=sha1".into(), ".".into()],
                    )?;
                    let alternate = self.repositories.pool_path(repo).join("objects");
                    // The allocation directory belongs only to this durable operation.
                    self.root.write_bytes(
                        format!("allocating/{wid}/.git/objects/info/alternates"),
                        format!("{}\n", alternate.display()).as_bytes(),
                    )?;
                    self.git_at(
                        &temporary,
                        vec!["config".into(), "core.hooksPath".into(), "/dev/null".into()],
                    )?;
                    self.git_at(
                        &temporary,
                        vec!["config".into(), "gc.auto".into(), "0".into()],
                    )?;
                    self.git_at(
                        &temporary,
                        vec!["checkout".into(), "--detach".into(), base.into()],
                    )?;
                    w.config_digest = self.workspace_config_digest(&temporary)?;
                    self.save_workspace(&w)?;
                    self.root.write_json(
                        format!("allocating/{wid}/.git/aq-git-owner.json"),
                        &json!({"workspace_id":wid,"repository_id":repo,"base":base}),
                    )?;
                    self.root.create_dir("workspaces")?;
                    fs::rename(&temporary, &final_path)?;
                    operations::sync_dir(final_path.parent().unwrap())?;
                    operations::sync_dir(temporary.parent().unwrap())?;
                }
                let owner: Value = self
                    .root
                    .read_json(format!("workspaces/{wid}/.git/aq-git-owner.json"))?
                    .ok_or_else(|| {
                        Error::new(
                            "workspace_changed",
                            "allocation has no matching ownership marker",
                        )
                    })?;
                if owner != json!({"workspace_id":wid,"repository_id":repo,"base":base}) {
                    return Err(Error::new(
                        "workspace_changed",
                        "allocation belongs to another workspace",
                    ));
                }
                if w.config_digest.is_empty()
                    || w.config_digest != self.workspace_config_digest(&final_path)?
                {
                    return Err(Error::new(
                        "workspace_changed",
                        "recovered allocation config differs from durable identity",
                    ));
                }
                if self.workspace_head(&w)? != base || !self.workspace_status(&w)?.is_empty() {
                    return Err(Error::new(
                        "workspace_changed",
                        "recovered allocation HEAD or files changed",
                    ));
                }
                w.lifecycle = "active".into();
                self.save_workspace(&w)?;
                Ok(serde_json::to_value(w)?)
            }
            "workspace.inspect" => {
                let p: Inspect = serde_json::from_value(r.params.clone())?;
                let w = self.workspace_record(repo, &p.workspace_id)?;
                if w.lifecycle != "active" {
                    return Ok(serde_json::to_value(w)?);
                }
                self.workspace_integrity(&w)?;
                let status = self.workspace_status(&w)?;
                Ok(
                    json!({"workspace":w,"head":operations::wire_oid(&self.workspace_head(&w)?),"clean":status.is_empty(),"status_encoding":"hex","status":hex::encode(status)}),
                )
            }
            "workspace.checkpoint" => {
                let p: Checkpoint = serde_json::from_value(r.params.clone())?;
                let expected = operations::oid(&p.expected_head)?;
                if !p.quiescent {
                    return Err(Error::new(
                        "workspace_changed",
                        "caller must stop all workspace writers",
                    ));
                }
                if p.mode != "head" && p.mode != "all" {
                    return Err(Error::new(
                        "invalid_request",
                        "checkpoint mode must be head or all",
                    ));
                }
                let op = operations::opid(r)?;
                let record_path = format!("repositories/{repo}/checkpoints/{op}.json");
                let mut w = self.workspace_record(repo, &p.workspace_id)?;
                if w.lifecycle != "active" {
                    return Err(Error::new("workspace_changed", "workspace is not active"));
                }
                self.workspace_integrity(&w)?;
                let mut checkpoint = if let Some(c) =
                    self.root.read_json::<CheckpointRecord>(&record_path)?
                {
                    c
                } else {
                    if w.generation != p.expected_generation || self.workspace_head(&w)? != expected
                    {
                        return Err(Error::new(
                            "workspace_changed",
                            "workspace generation or HEAD changed",
                        ));
                    }
                    if p.mode == "head" && !self.workspace_status(&w)?.is_empty() {
                        return Err(Error::new(
                            "workspace_dirty",
                            "head checkpoint requires a clean workspace",
                        ));
                    }
                    let index_tree_before =
                        String::from_utf8_lossy(&self.git_at(&w.path, vec!["write-tree".into()])?)
                            .trim()
                            .to_owned();
                    let mut source = expected.to_string();
                    if p.mode == "all" {
                        let metadata = p.commit.as_ref().ok_or_else(|| {
                            Error::new("invalid_request", "all checkpoint requires commit metadata")
                        })?;
                        metadata.validate()?;
                        self.root.create_dir("indexes")?;
                        let index = self.root.path().join("indexes").join(op);
                        let env = vec![(
                            "GIT_INDEX_FILE".into(),
                            index.to_string_lossy().into_owned(),
                        )];
                        self.git_with(&w.path, vec!["read-tree".into(), expected.into()], &env)?;
                        self.git_with(
                            &w.path,
                            vec!["add".into(), "--all".into(), "--".into(), ".".into()],
                            &env,
                        )?;
                        let tree = String::from_utf8_lossy(&self.git_with(
                            &w.path,
                            vec!["write-tree".into()],
                            &env,
                        )?)
                        .trim()
                        .to_string();
                        let prior = String::from_utf8_lossy(&self.git_at(
                            &w.path,
                            vec!["rev-parse".into(), format!("{expected}^{{tree}}")],
                        )?)
                        .trim()
                        .to_string();
                        if tree != prior {
                            source = self.create_commit(
                                &w.path,
                                &tree,
                                &[expected.to_string()],
                                metadata,
                                op,
                            )?;
                        }
                    }
                    if !self.ancestor_at(&w.path, &w.base, &source)? {
                        return Err(Error::new(
                            "invalid_request",
                            "assignment base is not ancestor of source",
                        ));
                    }
                    self.validate_tree_at(&w.path, &source)?;
                    let source_tree = String::from_utf8_lossy(&self.git_at(
                        &w.path,
                        vec!["rev-parse".into(), format!("{source}^{{tree}}")],
                    )?)
                    .trim()
                    .to_owned();
                    let c = CheckpointRecord {
                        index_tree_before,
                        source_tree,
                        format_version: 1,
                        old_checkpoint: w.last_checkpoint.clone(),
                        operation_id: op.into(),
                        workspace_id: w.workspace_id.clone(),
                        old_head: expected.into(),
                        source,
                        workspace_generation: w.generation,
                        completed: false,
                        result: Value::Null,
                    };
                    self.root.write_json(&record_path, &c)?;
                    c
                };
                if checkpoint.format_version != 1
                    || checkpoint.operation_id != op
                    || checkpoint.workspace_id != w.workspace_id
                    || checkpoint.old_head != expected
                    || checkpoint.workspace_generation != p.expected_generation
                {
                    return Err(Error::new(
                        "recovery_blocked",
                        "checkpoint record differs from durable request",
                    ));
                }
                if checkpoint.completed {
                    let temporary_index = self.root.path().join("indexes").join(op);
                    if temporary_index.exists() {
                        fs::remove_file(&temporary_index)?;
                        operations::sync_dir(temporary_index.parent().unwrap())?;
                    }
                    return Ok(checkpoint.result);
                }
                if !((w.generation == checkpoint.workspace_generation
                    && w.last_checkpoint == checkpoint.old_checkpoint)
                    || (w.generation == checkpoint.workspace_generation + 1
                        && w.last_checkpoint.as_deref() == Some(op)))
                {
                    return Err(Error::new(
                        "workspace_changed",
                        "workspace advanced beyond this pending checkpoint",
                    ));
                }
                let workspace_updated = w.generation == checkpoint.workspace_generation + 1;
                let head = self.workspace_head(&w)?;
                let current_index = if workspace_updated {
                    String::new()
                } else {
                    String::from_utf8_lossy(&self.git_at(&w.path, vec!["write-tree".into()])?)
                        .trim()
                        .to_owned()
                };
                if !workspace_updated
                    && ((head != checkpoint.old_head && head != checkpoint.source)
                        || (current_index != checkpoint.index_tree_before
                            && current_index != checkpoint.source_tree))
                {
                    return Err(Error::new(
                        "recovery_blocked",
                        "workspace HEAD or staged content changed after checkpoint intent",
                    ));
                }
                self.validate_tree_at(&w.path, &checkpoint.source)?;
                // Transfer the private source closure into the node pool before retention acknowledgement.
                self.repositories.git(
                    repo,
                    vec![
                        "fetch".into(),
                        "--no-tags".into(),
                        "--no-write-fetch-head".into(),
                        w.path.to_string_lossy().into_owned(),
                        checkpoint.source.clone(),
                    ],
                )?;
                self.validate_tree_features(repo, &checkpoint.source)?;
                self.pin(
                    repo,
                    binding.binding_generation,
                    op,
                    "checkpoint",
                    vec![RetentionRef {
                        name: format!("refs/aq-git/checkpoints/{op}"),
                        oid: checkpoint.source.clone(),
                    }],
                )?;
                self.root.acquire_dependency(op, &w.workspace_id)?;
                if !workspace_updated {
                    if head != checkpoint.source {
                        if head != checkpoint.old_head {
                            return Err(Error::new(
                                "workspace_changed",
                                "checkpoint recovery found an unexpected HEAD",
                            ));
                        }
                        self.git_at(
                            &w.path,
                            vec![
                                "update-ref".into(),
                                "HEAD".into(),
                                checkpoint.source.clone(),
                                checkpoint.old_head.clone(),
                            ],
                        )?;
                    }
                    if current_index != checkpoint.source_tree {
                        self.git_at(
                            &w.path,
                            vec![
                                "read-tree".into(),
                                "--reset".into(),
                                checkpoint.source.clone(),
                            ],
                        )?;
                    }
                    w.last_checkpoint = Some(op.into());
                    w.generation = checkpoint.workspace_generation + 1;
                    self.save_workspace(&w)?;
                }
                if let Some(previous) = &checkpoint.old_checkpoint
                    && previous != op
                {
                    self.root.release_dependency(previous, &w.workspace_id)?;
                }
                checkpoint.result = json!({"workspace_id":w.workspace_id,"workspace_generation":w.generation,"source":operations::wire_oid(&checkpoint.source),"retention_handle":op,"unchanged":checkpoint.source==checkpoint.old_head});
                checkpoint.completed = true;
                self.root.write_json(&record_path, &checkpoint)?;
                let temporary_index = self.root.path().join("indexes").join(op);
                if temporary_index.exists() {
                    fs::remove_file(&temporary_index)?;
                    operations::sync_dir(temporary_index.parent().unwrap())?;
                }
                Ok(checkpoint.result)
            }
            "workspace.retire" => {
                let p: Retire = serde_json::from_value(r.params.clone())?;
                let expected = operations::oid(&p.expected_head)?;
                let op = operations::opid(r)?;
                let mut w = self.workspace_record(repo, &p.workspace_id)?;
                if !p.quiescent {
                    return Err(Error::new(
                        "workspace_changed",
                        "caller must stop all workspace writers",
                    ));
                }
                if w.lifecycle == "active" {
                    self.workspace_integrity(&w)?;
                    if w.generation != p.expected_generation || self.workspace_head(&w)? != expected
                    {
                        return Err(Error::new(
                            "workspace_changed",
                            "workspace generation or HEAD changed",
                        ));
                    }
                    if !self.workspace_status(&w)?.is_empty() {
                        return Err(Error::new(
                            "workspace_dirty",
                            "retirement refuses dirty work",
                        ));
                    }
                    let handle = w.last_checkpoint.as_ref().ok_or_else(|| {
                        Error::new(
                            "retention_not_found",
                            "checkpoint required before retirement",
                        )
                    })?;
                    let pin = self.active_handle(repo, handle)?;
                    if !pin.refs.iter().any(|x| x.oid == expected) {
                        return Err(Error::new(
                            "workspace_changed",
                            "retained checkpoint differs from HEAD",
                        ));
                    }
                    w.lifecycle = "retiring".into();
                    w.retiring_operation = Some(op.into());
                    self.save_workspace(&w)?;
                } else if w.retiring_operation.as_deref() != Some(op) {
                    return Err(Error::new("workspace_changed", "workspace is not active"));
                }
                self.root.create_dir("trash")?;
                let trash = self.root.path().join("trash").join(&w.workspace_id);
                let original_exists = w.path.symlink_metadata().is_ok();
                let trash_exists = trash.symlink_metadata().is_ok();
                if original_exists && trash_exists {
                    return Err(Error::new(
                        "recovery_blocked",
                        "workspace and retirement trash both exist",
                    ));
                }
                if w.lifecycle == "retired" {
                    if original_exists {
                        return Err(Error::new(
                            "recovery_blocked",
                            "retired workspace path was replaced",
                        ));
                    }
                    if trash_exists {
                        let mut retired = w.clone();
                        retired.path = trash.clone();
                        self.workspace_integrity_at(&retired, false)?;
                        if self.workspace_head(&retired)? != expected
                            || !self.workspace_status(&retired)?.is_empty()
                        {
                            return Err(Error::new(
                                "workspace_changed",
                                "retirement trash differs from recorded source",
                            ));
                        }
                        fs::remove_dir_all(&trash)?;
                        operations::sync_dir(trash.parent().unwrap())?;
                    }
                    return Ok(serde_json::to_value(w)?);
                }
                if !original_exists && !trash_exists {
                    return Err(Error::new(
                        "recovery_blocked",
                        "retiring workspace and trash are both missing",
                    ));
                }
                if original_exists {
                    self.workspace_integrity(&w)?;
                    if self.workspace_head(&w)? != expected
                        || !self.workspace_status(&w)?.is_empty()
                    {
                        return Err(Error::new(
                            "workspace_changed",
                            "workspace changed after retirement intent",
                        ));
                    }
                    fs::rename(&w.path, &trash)?;
                    operations::sync_dir(w.path.parent().unwrap())?;
                    operations::sync_dir(trash.parent().unwrap())?;
                }
                let mut retired = w.clone();
                retired.path = trash.clone();
                self.workspace_integrity_at(&retired, false)?;
                if self.workspace_head(&retired)? != expected
                    || !self.workspace_status(&retired)?.is_empty()
                {
                    return Err(Error::new(
                        "workspace_changed",
                        "retirement trash differs from recorded source",
                    ));
                }
                if let Some(checkpoint) = &w.last_checkpoint {
                    self.root.release_dependency(checkpoint, &w.workspace_id)?;
                }
                let pin = self.root.retention(&w.workspace_id)?.ok_or_else(|| {
                    Error::new("retention_not_found", "workspace borrow pin missing")
                })?;
                self.release_handle(&w.workspace_id, pin.generation, op, "workspace retired")?;
                w.lifecycle = "retired".into();
                w.generation = p.expected_generation + 1;
                self.save_workspace(&w)?;
                if trash.exists() {
                    operations::checked_directory(&trash)?;
                    fs::remove_dir_all(&trash)?;
                    operations::sync_dir(trash.parent().unwrap())?;
                }
                Ok(serde_json::to_value(w)?)
            }
            _ => Err(Error::new(
                "unsupported_feature",
                "workspace operation unavailable",
            )),
        }
    }
    pub(crate) fn git_with(
        &self,
        path: &std::path::Path,
        args: Vec<String>,
        env: &[(String, String)],
    ) -> Result<Vec<u8>> {
        let output =
            self.repositories
                .runner
                .run_with(path, &args, &[], env, Duration::from_secs(120))?;
        if !output.status.success() {
            return Err(Error::new("git_failed", "Git operation failed")
                .details(json!({"exit_code":output.status.code()})));
        }
        Ok(output.stdout)
    }
    pub(crate) fn create_commit(
        &self,
        path: &std::path::Path,
        tree: &str,
        parents: &[String],
        metadata: &CommitMetadata,
        operation: &str,
    ) -> Result<String> {
        metadata.validate()?;
        let record = self
            .root
            .operation(operation)?
            .ok_or_else(|| Error::new("operation_not_found", "missing durable commit intent"))?;
        let date = format!("@{} +0000", record.created_at_ms / 1000);
        let env = vec![
            ("GIT_AUTHOR_NAME".into(), metadata.name.clone()),
            ("GIT_AUTHOR_EMAIL".into(), metadata.email.clone()),
            ("GIT_COMMITTER_NAME".into(), metadata.name.clone()),
            ("GIT_COMMITTER_EMAIL".into(), metadata.email.clone()),
            ("GIT_AUTHOR_DATE".into(), date.clone()),
            ("GIT_COMMITTER_DATE".into(), date),
        ];
        let mut args = vec!["commit-tree".into(), tree.into()];
        for parent in parents {
            args.extend(["-p".into(), parent.clone()]);
        }
        args.extend(["-m".into(), metadata.message.clone()]);
        Ok(String::from_utf8_lossy(&self.git_with(path, args, &env)?)
            .trim()
            .into())
    }
}

fn validate_private_refs(path: &std::path::Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(Error::new("workspace_changed", "symlink in workspace refs"));
        }
        if kind.is_dir() {
            validate_private_refs(&entry.path())?;
        } else if !kind.is_file() {
            return Err(Error::new("workspace_changed", "nonregular workspace ref"));
        }
    }
    Ok(())
}
