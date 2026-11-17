//! `workspace.snapshot`: capture a managed workspace's working tree, including
//! dirty tracked files and untracked non-ignored files, as a Git bundle with a
//! sha256 digest. The workspace HEAD, index and refs are left untouched.
//!
//! The snapshot commit uses a fixed identity, date and message, and the bundle
//! is written with a fixed ref name and single-threaded packing. Two snapshots
//! of an unchanged tree on unchanged object storage therefore produce
//! byte-identical bundles and the same digest.
use super::WorkspaceRecord;
use crate::{
    Error, Result,
    artifact::{self, ArtifactLimits},
    operations,
    service::RepositoryService,
};
use aq_git_protocol::{ObjectId, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, path::Path};

/// Ref name recorded inside every snapshot bundle.
pub const SNAPSHOT_REF: &str = "refs/aq-git/snapshot";
/// File name of the bundle under `ROOT/artifacts/<artifact_handle>/`.
pub const SNAPSHOT_BUNDLE: &str = "snapshot.bundle";
const IDENTITY_NAME: &str = "aq-git snapshot";
const IDENTITY_EMAIL: &str = "snapshot@aq-git.invalid";
const DATE: &str = "@0 +0000";
const MESSAGE: &str = "aq-git working-tree snapshot";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    workspace_id: String,
    expected_generation: u64,
    expected_head: ObjectId,
    quiescent: bool,
}

impl RepositoryService {
    pub(super) fn snapshot_workspace(&self, r: &Request, repo: &str) -> Result<Value> {
        let p: Snapshot = serde_json::from_value(r.params.clone())?;
        let expected = operations::oid(&p.expected_head)?;
        if !p.quiescent {
            return Err(Error::new(
                "workspace_changed",
                "caller must stop all workspace writers",
            ));
        }
        let op = operations::opid(r)?;
        let w = self.workspace_record(repo, &p.workspace_id)?;
        if w.lifecycle != "active" {
            return Err(Error::new("workspace_changed", "workspace is not active"));
        }
        self.workspace_integrity(&w)?;
        if w.generation != p.expected_generation || self.workspace_head(&w)? != expected {
            return Err(Error::new(
                "workspace_changed",
                "workspace generation or HEAD changed",
            ));
        }
        let limits = self.artifact_limits()?;
        self.root.create_dir("indexes")?;
        let index = self.root.path().join("indexes").join(op);
        let scratch = format!("snapshots/{op}");
        let outcome = self.write_snapshot(&w, expected, op, &index, &scratch, &limits);
        // Scratch state is reconstructible; a cleanup failure must not mask the outcome.
        let _ = fs::remove_file(&index);
        let _ = remove_scratch(&self.root.path().join(&scratch));
        outcome
    }

    fn write_snapshot(
        &self,
        w: &WorkspaceRecord,
        head: &str,
        op: &str,
        index: &Path,
        scratch: &str,
        limits: &ArtifactLimits,
    ) -> Result<Value> {
        if !self.ancestor_at(&w.path, &w.base, head)? {
            return Err(Error::new(
                "invalid_request",
                "assignment base is not ancestor of workspace HEAD",
            ));
        }
        // A private index keeps the workspace index and HEAD untouched.
        let env = vec![(
            "GIT_INDEX_FILE".into(),
            index.to_string_lossy().into_owned(),
        )];
        self.git_with(&w.path, vec!["read-tree".into(), head.into()], &env)?;
        self.git_with(
            &w.path,
            vec!["add".into(), "--all".into(), "--".into(), ".".into()],
            &env,
        )?;
        let tree = trimmed(self.git_with(&w.path, vec!["write-tree".into()], &env)?);
        let identity = [
            ("GIT_AUTHOR_NAME", IDENTITY_NAME),
            ("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL),
            ("GIT_AUTHOR_DATE", DATE),
            ("GIT_COMMITTER_NAME", IDENTITY_NAME),
            ("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL),
            ("GIT_COMMITTER_DATE", DATE),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string()));
        // Always create a commit so the bundle is never empty, even when the
        // working tree equals HEAD and HEAD equals the base.
        let snapshot = trimmed(self.git_with(
            &w.path,
            vec![
                "commit-tree".into(),
                tree.clone(),
                "-p".into(),
                head.into(),
                "-m".into(),
                MESSAGE.into(),
            ],
            &identity,
        )?);
        self.validate_tree_at(&w.path, &snapshot)?;

        // Bundles need a named ref. A scratch bare repository borrowing the
        // workspace objects carries it, so workspace refs are never written.
        let scratch_path = self.root.path().join(scratch);
        remove_scratch(&scratch_path)?;
        self.root.create_dir(scratch)?;
        self.git_at(
            &scratch_path,
            vec![
                "init".into(),
                "--bare".into(),
                "--object-format=sha1".into(),
                ".".into(),
            ],
        )?;
        self.root.write_bytes(
            format!("{scratch}/objects/info/alternates"),
            format!("{}\n", w.path.join(".git/objects").display()).as_bytes(),
        )?;
        self.git_at(
            &scratch_path,
            vec!["update-ref".into(), SNAPSHOT_REF.into(), snapshot.clone()],
        )?;

        let directory = self.root.create_dir(format!("artifacts/{op}"))?;
        artifact::reserve(
            &directory,
            limits.max_bundle_bytes,
            limits.disk_reserve_bytes,
        )?;
        let temporary = directory.join(format!("{SNAPSHOT_BUNDLE}.partial"));
        if let Ok(meta) = fs::symlink_metadata(&temporary) {
            if meta.file_type().is_symlink() {
                return Err(Error::new("invalid_path", "artifact temporary replaced"));
            }
            fs::remove_file(&temporary)?;
        }
        let args = vec![
            "-c".into(),
            "pack.threads=1".into(),
            "bundle".into(),
            "create".into(),
            "-".into(),
            SNAPSHOT_REF.into(),
            format!("^{}", w.base),
        ];
        let output = self.repositories.runner.run_to_file(
            &scratch_path,
            &args,
            artifact::durable_bundle(&temporary)?,
            limits.max_bundle_bytes as usize,
        )?;
        if !output.status.success() {
            return Err(Error::new("git_failed", "could not create snapshot bundle"));
        }
        let (bundle_size, bundle_sha256) =
            artifact::digest_file(&temporary, limits.max_bundle_bytes)?;
        fs::rename(&temporary, directory.join(SNAPSHOT_BUNDLE))?;
        operations::sync_dir(&directory)?;
        let result = json!({
            "artifact_handle": op,
            "bundle": format!("artifacts/{op}/{SNAPSHOT_BUNDLE}"),
            "workspace_id": w.workspace_id,
            "workspace_generation": w.generation,
            "base": operations::wire_oid(&w.base),
            "head": operations::wire_oid(head),
            "snapshot": operations::wire_oid(&snapshot),
            "tree": operations::wire_oid(&tree),
            "ref": SNAPSHOT_REF,
            "prerequisites": [operations::wire_oid(&w.base)],
            "bundle_size": bundle_size,
            "bundle_sha256": bundle_sha256,
        });
        self.root
            .write_json(format!("artifacts/{op}/snapshot.json"), &result)?;
        Ok(result)
    }
}

fn trimmed(output: Vec<u8>) -> String {
    String::from_utf8_lossy(&output).trim().to_owned()
}

fn remove_scratch(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        operations::checked_directory(path)?;
        fs::remove_dir_all(path)?;
    }
    Ok(())
}
