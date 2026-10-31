use crate::{
    Error, Result,
    service::RepositoryService,
    state::{RetentionRecord, RetentionRef, RetentionState},
};
use aq_git_protocol::{ObjectId, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::Path,
};

static HARDEN_FILES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HARDEN_DIRECTORIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HARDEN_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CONNECTIVITY_CHECKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CONNECTIVITY_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct Measure<'a>(&'a std::sync::atomic::AtomicU64, std::time::Instant);
impl Drop for Measure<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(
            self.1.elapsed().as_micros().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}
pub fn durability_metrics() -> Value {
    use std::sync::atomic::Ordering::Relaxed;
    json!({"scope":"process","connectivity_checks":CONNECTIVITY_CHECKS.load(Relaxed),"connectivity_micros":CONNECTIVITY_MICROS.load(Relaxed),"hardened_files":HARDEN_FILES.load(Relaxed),"hardened_directories":HARDEN_DIRECTORIES.load(Relaxed),"hardening_micros":HARDEN_MICROS.load(Relaxed),"adapter":"conservative_pool_scan"})
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitMetadata {
    pub name: String,
    pub email: String,
    pub message: String,
}
impl CommitMetadata {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.email.is_empty()
            || self.message.len() > 65536
            || self.name.contains(['\n', '\r', '\0', '<', '>'])
            || self.email.contains(['\n', '\r', '\0', '<', '>'])
            || self.message.contains('\0')
        {
            return Err(Error::new(
                "invalid_request",
                "invalid explicit commit identity/message",
            ));
        }
        Ok(())
    }
}
pub fn oid(value: &ObjectId) -> Result<&str> {
    value
        .validate()
        .map_err(|e| Error::new(e.code, e.message))?;
    Ok(&value.hex)
}
pub fn id(value: &str) -> Result<&str> {
    aq_git_protocol::validate_id(value).map_err(|e| Error::new(e.code, e.message))?;
    Ok(value)
}
pub fn wire_oid(value: &str) -> Value {
    json!({"algorithm":"sha1","hex":value})
}
pub fn opid(r: &Request) -> Result<&str> {
    r.operation_id
        .as_deref()
        .ok_or_else(|| Error::new("invalid_request", "operation_id required"))
}
pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub fn checked_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        return Err(Error::new("invalid_path", "managed directory was replaced").details(json!({"origin":"checked_directory","entry_encoding":"unix_bytes_hex","entry_hex":hex::encode(path.file_name().unwrap_or_default().as_bytes()),"entry_type":if meta.file_type().is_symlink(){"symlink"}else if meta.is_file(){"regular_file"}else{"other"}})));
    }
    Ok(())
}
impl RepositoryService {
    pub(crate) fn git_at(&self, path: &Path, args: Vec<String>) -> Result<Vec<u8>> {
        let output = self.repositories.runner.run(path, &args)?;
        if !output.status.success() {
            return Err(Error::new("git_failed", "Git operation failed")
                .details(json!({"exit_code":output.status.code()})));
        }
        Ok(output.stdout)
    }
    pub(crate) fn commit_exists(&self, repo: &str, commit: &str) -> Result<()> {
        let out = self.repositories.runner.run(
            &self.repositories.pool_path(repo),
            &["cat-file".into(), "-t".into(), commit.into()],
        )?;
        if !out.status.success() {
            return Err(Error::new(
                "object_unavailable",
                "commit is unavailable locally",
            ));
        }
        if out.stdout != b"commit\n" {
            return Err(Error::new("invalid_request", "expected commit object"));
        }
        Ok(())
    }
    pub(crate) fn ancestor_at(&self, path: &Path, a: &str, b: &str) -> Result<bool> {
        let out = self.repositories.runner.run(
            path,
            &[
                "merge-base".into(),
                "--is-ancestor".into(),
                a.into(),
                b.into(),
            ],
        )?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::new(
                "object_unavailable",
                "ancestry could not be established",
            )),
        }
    }
    pub(crate) fn pin(
        &self,
        repo: &str,
        generation: u64,
        handle: &str,
        resource: &str,
        refs: Vec<RetentionRef>,
    ) -> Result<()> {
        if let Some(existing) = self.root.retention(handle)? {
            if existing.state != RetentionState::Active
                || existing.repository_id != repo
                || existing.binding_generation != generation
                || existing.resource != resource
                || existing.refs != refs
            {
                return Err(Error::new(
                    "recovery_blocked",
                    "existing handle does not match durable intent",
                ));
            }
            self.verify_retention_refs(&existing)?;
            return Ok(());
        }
        let pool = self.repositories.pool_path(repo);
        if pool.join("shallow").symlink_metadata().is_ok()
            || pool
                .join("objects/info/alternates")
                .symlink_metadata()
                .is_ok()
        {
            return Err(Error::new(
                "unsupported_feature",
                "managed retained pools must have complete independent objects",
            ));
        }
        CONNECTIVITY_CHECKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let connectivity_timer = Measure(&CONNECTIVITY_MICROS, std::time::Instant::now());
        self.git_at(
            &pool,
            std::iter::once("fsck".into())
                .chain([
                    "--connectivity-only".into(),
                    "--no-dangling".into(),
                    "--no-reflogs".into(),
                ])
                .chain(refs.iter().map(|r| r.oid.clone()))
                .collect(),
        )?;
        drop(connectivity_timer);
        harden_objects(&pool.join("objects"))?;
        for reference in &refs {
            let found = self.repositories.runner.run(
                &self.repositories.pool_path(repo),
                &[
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    reference.name.clone(),
                ],
            )?;
            if found.status.success() {
                if String::from_utf8_lossy(&found.stdout).trim() != reference.oid {
                    return Err(Error::new(
                        "recovery_blocked",
                        "retention ref has unexpected contents",
                    ));
                }
            } else if found.status.code() == Some(1) {
                self.repositories.git(
                    repo,
                    vec![
                        "update-ref".into(),
                        reference.name.clone(),
                        reference.oid.clone(),
                        "0000000000000000000000000000000000000000".into(),
                    ],
                )?;
            } else {
                return Err(Error::new(
                    "recovery_blocked",
                    "cannot establish retention ref absence",
                ));
            }
        }
        self.root.create_retention(&RetentionRecord {
            version: 1,
            id: handle.into(),
            repository_id: repo.into(),
            binding_generation: generation,
            generation: 1,
            resource: resource.into(),
            refs,
            state: RetentionState::Active,
            dependencies: BTreeSet::new(),
            release_operation: None,
            release_reason: None,
        })
    }
    pub(crate) fn active_handle(&self, repo: &str, handle: &str) -> Result<RetentionRecord> {
        let record = self
            .root
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "retention handle not found"))?;
        if record.repository_id != repo || record.state != RetentionState::Active {
            return Err(Error::new(
                "retention_released",
                "resource has no active retention in this repository",
            ));
        }
        if record.binding_generation != self.repositories.binding(repo)?.binding_generation {
            return Err(Error::new(
                "binding_conflict",
                "retention belongs to another binding generation",
            ));
        }
        self.verify_retention_refs(&record)?;
        Ok(record)
    }
    pub(crate) fn verify_retention_refs(&self, record: &RetentionRecord) -> Result<()> {
        for reference in &record.refs {
            let out = self.repositories.runner.run(
                &self.repositories.pool_path(&record.repository_id),
                &[
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    reference.name.clone(),
                ],
            )?;
            if !out.status.success() || out.stdout != format!("{}\n", reference.oid).as_bytes() {
                return Err(Error::new(
                    "recovery_blocked",
                    "retention ref is missing or differs from recorded OID",
                )
                .details(json!({"handle_id":record.id,"ref":reference.name})));
            }
            self.commit_exists(&record.repository_id, &reference.oid)?;
        }
        Ok(())
    }
    pub(crate) fn release_handle(
        &self,
        handle: &str,
        generation: u64,
        operation: &str,
        reason: &str,
    ) -> Result<RetentionRecord> {
        let current = self
            .root
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "retention handle not found"))?;
        if current.state == RetentionState::Released
            && current.release_operation.as_deref() == Some(operation)
        {
            return Ok(current);
        }
        if current.state == RetentionState::Active {
            if current.generation != generation {
                return Err(Error::new(
                    "generation_conflict",
                    "retention generation changed",
                ));
            }
            self.verify_retention_refs(&current)?;
        }
        if current.state == RetentionState::Released {
            return Err(Error::new(
                "retention_released",
                "resource was released by another operation",
            ));
        }
        if current.state == RetentionState::Releasing
            && current.release_operation.as_deref() != Some(operation)
        {
            return Err(Error::new(
                "retention_conflict",
                "another release owns the recorded intent",
            ));
        }
        let record = if current.state == RetentionState::Releasing
            && current.release_operation.as_deref() == Some(operation)
        {
            current
        } else {
            self.root.begin_release_with_reason(
                handle,
                generation,
                Some(operation.into()),
                Some(reason.into()),
            )?
        };
        // Explicit local release cancels a still-pending creator before pins can disappear.
        // Remote publication uncertainty is never a cancellable creator and its dependency
        // must continue to block begin_release.
        let mut creators = Vec::new();
        if let Some(creation) = self.root.operation(handle)? {
            let kind = creation.payload["operation"].as_str();
            if creation.status == crate::state::OperationStatus::Pending
                && creation.payload["repository_id"] == record.repository_id
                && matches!(
                    (record.resource.as_str(), kind),
                    ("candidate", Some("candidate.create"))
                        | ("checkpoint", Some("workspace.checkpoint"))
                        | ("transfer", Some("artifact.export"))
                )
            {
                creators.push(creation);
            }
        }
        if record.resource == "transfer" {
            for id in self.record_ids("operations")? {
                if id == operation || id == handle {
                    continue;
                }
                if let Some(creation) = self.root.operation(&id)?
                    && creation.status == crate::state::OperationStatus::Pending
                    && creation.payload["operation"] == "artifact.import"
                    && creation.payload["repository_id"] == record.repository_id
                    && creation.metadata["transfer_id"] == handle
                {
                    creators.push(creation);
                }
            }
        }
        for creation in creators {
            self.root.fail_operation(&creation.id,creation.generation,serde_json::to_value(Error::new("retention_released","local resource creation cancelled by explicit release").details(json!({"reason":reason,"release_operation_id":operation,"retention_handle":handle})))?)?;
        }
        for reference in &record.refs {
            let output = self.repositories.runner.run(
                &self.repositories.pool_path(&record.repository_id),
                &[
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    reference.name.clone(),
                ],
            )?;
            if output.status.success() {
                if String::from_utf8_lossy(&output.stdout).trim() != reference.oid {
                    return Err(Error::new(
                        "recovery_blocked",
                        "release found a changed retention ref",
                    ));
                }
                self.repositories.git(
                    &record.repository_id,
                    vec![
                        "update-ref".into(),
                        "-d".into(),
                        reference.name.clone(),
                        reference.oid.clone(),
                    ],
                )?;
            } else if output.status.code() != Some(1) {
                return Err(Error::new("git_failed", "could not inspect retention ref"));
            }
        }
        self.root.finish_release(handle, record.generation)
    }
    pub(crate) fn validate_tree_features(&self, repo: &str, commit: &str) -> Result<()> {
        self.validate_tree_at(&self.repositories.pool_path(repo), commit)
    }
    pub(crate) fn validate_tree_at(&self, path: &Path, commit: &str) -> Result<()> {
        let entries = self.git_at(path, vec!["ls-tree".into(), "-rz".into(), commit.into()])?;
        for entry in entries.split(|x| *x == 0).filter(|x| !x.is_empty()) {
            if entry.starts_with(b"160000 ") {
                return Err(Error::new(
                    "unsupported_feature",
                    "submodules are not supported",
                ));
            }
            let Some(tab) = entry.iter().position(|x| *x == b'\t') else {
                return Err(Error::new("git_failed", "malformed tree output"));
            };
            let path = &entry[tab + 1..];
            if path.rsplit(|x| *x == b'/').next() == Some(b".gitattributes".as_slice()) {
                // The initial advertised profile refuses attributes until conformance fixtures land.
                return Err(Error::new(
                    "unsupported_feature",
                    "attribute-controlled checkout/merge is not enabled",
                ));
            }
        }
        Ok(())
    }
}

// Harden all existing pool object files as well as new writes. Borrowed/local objects
// may have originated in ordinary Git, whose original write need not have been synced.
fn harden_objects(path: &Path) -> Result<()> {
    let _timer = Measure(&HARDEN_MICROS, std::time::Instant::now());
    harden_objects_inner(path)
}
fn harden_objects_inner(path: &Path) -> Result<()> {
    checked_directory(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if crate::git::remaining_deadline().is_some_and(|d| d.is_zero()) {
            return Err(Error::new(
                "deadline_exceeded",
                "object hardening deadline expired",
            ));
        }
        let meta = entry.file_type()?;
        if meta.is_symlink() {
            return Err(Error::new(
                "recovery_blocked",
                "symlink in managed object store",
            ));
        }
        if meta.is_dir() {
            harden_objects_inner(&entry.path())?;
        } else if meta.is_file() {
            File::open(entry.path())?.sync_all()?;
            HARDEN_FILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            return Err(Error::new(
                "recovery_blocked",
                "nonregular entry in managed object store",
            ));
        }
    }
    sync_dir(path)?;
    HARDEN_DIRECTORIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}
