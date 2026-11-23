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

mod allocation;
mod checkpoint;
mod retirement;
mod snapshot;
pub use snapshot::{SNAPSHOT_BUNDLE, SNAPSHOT_REF};

pub const OPERATIONS: &[&str] = &[
    "workspace.create",
    "workspace.inspect",
    "workspace.checkpoint",
    "workspace.retire",
    aq_git_protocol::WORKSPACE_SNAPSHOT,
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
    commit: Option<CheckpointCommit>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCommit {
    name: String,
    email: String,
    message: String,
    author: Option<CommitAuthor>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitAuthor {
    name: String,
    email: String,
}
pub(crate) struct CommitTree<'a> {
    pub tree: &'a str,
    pub parents: &'a [String],
    pub metadata: &'a CommitMetadata,
    pub author: Option<&'a CommitAuthor>,
    pub operation: &'a str,
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
        let allocated = (
            record.workspace_id.as_str(),
            record.repository_id.as_str(),
            record.allocating_operation.as_str(),
        ) == (workspace, repo, workspace);
        let expected_path = self.root.path().join("workspaces").join(workspace);
        let consistent = if allocated && record.path == expected_path {
            record.lifecycle == "retired"
                || record.binding_generation == self.repositories.binding(repo)?.binding_generation
        } else {
            false
        };
        let known_phase = matches!(
            record.lifecycle.as_str(),
            "allocating" | "active" | "retiring" | "retired"
        );
        if !consistent || !known_phase {
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
        self.check_workspace_owner(w)?;
        if verify_borrow {
            self.check_workspace_borrow(w)?;
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
    fn check_workspace_owner(&self, w: &WorkspaceRecord) -> Result<()> {
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
        Ok(())
    }
    fn check_workspace_borrow(&self, w: &WorkspaceRecord) -> Result<()> {
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
            "workspace.checkpoint" | "workspace.retire" | "workspace.snapshot"
        ) && let Some(workspace) = r.params["workspace_id"].as_str()
        {
            self.check_workspace_admission(repo, workspace, r.operation_id.as_deref())?;
        }
        match r.operation.as_str() {
            "workspace.create" => self.create_workspace(r, &binding),
            "workspace.inspect" => self.inspect_workspace(r, repo),
            "workspace.checkpoint" => self.checkpoint_workspace(r, &binding),
            "workspace.retire" => self.retire_workspace(r, repo),
            aq_git_protocol::WORKSPACE_SNAPSHOT => self.snapshot_workspace(r, repo),
            _ => Err(Error::new(
                "unsupported_feature",
                "workspace operation unavailable",
            )),
        }
    }
    fn check_workspace_admission(
        &self,
        repo: &str,
        workspace: &str,
        operation_id: Option<&str>,
    ) -> Result<()> {
        for id in self.record_ids("operations")? {
            if Some(id.as_str()) == operation_id {
                continue;
            }
            let Some(operation) = self.root.operation(&id)? else {
                continue;
            };
            let matches_workspace = match operation.payload["operation"].as_str() {
                Some("workspace.create") => id == workspace,
                Some("workspace.checkpoint" | "workspace.retire") => {
                    operation.payload["params"]["workspace_id"] == workspace
                }
                _ => false,
            };
            let pending = operation.status == crate::state::OperationStatus::Pending
                && operation.payload["repository_id"] == repo;
            if pending && matches_workspace {
                return Err(Error::new(
                    "retention_in_use",
                    "workspace has a pending mutation that must be replayed first",
                )
                .details(json!({"blockers":[id]})));
            }
        }
        Ok(())
    }
    fn inspect_workspace(&self, r: &Request, repo: &str) -> Result<Value> {
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
        commit: CommitTree<'_>,
    ) -> Result<String> {
        let CommitTree {
            tree,
            parents,
            metadata,
            author,
            operation,
        } = commit;
        metadata.validate()?;
        let record = self
            .root
            .operation(operation)?
            .ok_or_else(|| Error::new("operation_not_found", "missing durable commit intent"))?;
        let date = format!("@{} +0000", record.created_at_ms / 1000);
        let (author_name, author_email) = author
            .map(|identity| (&identity.name, &identity.email))
            .unwrap_or((&metadata.name, &metadata.email));
        let env = vec![
            ("GIT_AUTHOR_NAME".into(), author_name.clone()),
            ("GIT_AUTHOR_EMAIL".into(), author_email.clone()),
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
