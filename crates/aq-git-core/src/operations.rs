mod retention;
pub(crate) use retention::{HandleRelease, Pin};

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
        let valid_message = self.message.len() <= 65536 && !self.message.contains('\0');
        if !valid_commit_identity(&self.name, &self.email) || !valid_message {
            return Err(Error::new(
                "invalid_request",
                "invalid explicit commit identity/message",
            ));
        }
        Ok(())
    }
}
pub(crate) fn valid_commit_identity(name: &str, email: &str) -> bool {
    [name, email]
        .iter()
        .all(|value| !value.is_empty() && !value.contains(['\n', '\r', '\0', '<', '>']))
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
    pub(crate) fn validate_tree_features(&self, repo: &str, commit: &str) -> Result<()> {
        self.validate_tree_at(&self.repositories.pool_path(repo), commit)
    }
    pub(crate) fn validate_tree_at(&self, path: &Path, commit: &str) -> Result<()> {
        validate_tree_with(&mut |args| self.git_at(path, args), commit)
    }
}

/// Feature-profile check of one commit's tree (submodules, attributes metadata).
/// `git` runs one Git command in the repository under inspection, so the same
/// rules apply to the pool, an import quarantine and a push quarantine.
pub(crate) fn validate_tree_with(
    git: &mut dyn FnMut(Vec<String>) -> Result<Vec<u8>>,
    commit: &str,
) -> Result<()> {
    let entries = git(vec!["ls-tree".into(), "-rz".into(), commit.into()])?;
    let mut attribute_files = 0;
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
        let entry_path = &entry[tab + 1..];
        if entry_path.rsplit(|x| *x == b'/').next() == Some(b".gitattributes".as_slice()) {
            attribute_files += 1;
            validate_attribute_file(git, &entry[..tab], attribute_files)?;
        }
    }
    Ok(())
}

fn validate_attribute_file(
    git: &mut dyn FnMut(Vec<String>) -> Result<Vec<u8>>,
    header: &[u8],
    attribute_files: usize,
) -> Result<()> {
    let header = std::str::from_utf8(header)
        .map_err(|_| Error::new("git_failed", "malformed tree entry"))?;
    let fields: Vec<_> = header.split_ascii_whitespace().collect();
    let blob = matches!(fields.as_slice(), ["100644" | "100755", "blob", _]);
    if attribute_files > 32 || !blob {
        return Err(Error::new(
            "unsupported_feature",
            "unsupported attributes metadata",
        ));
    }
    let size = git(vec!["cat-file".into(), "-s".into(), fields[2].into()])?;
    if std::str::from_utf8(&size)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .is_none_or(|n| n > 65_536)
    {
        return Err(Error::new(
            "unsupported_feature",
            "attributes metadata too large",
        ));
    }
    let body = git(vec!["cat-file".into(), "blob".into(), fields[2].into()])?;
    if !crate::metadata_attributes::whitespace_only(&body) {
        return Err(Error::new(
            "unsupported_feature",
            "only whitespace-diagnostic attributes are enabled; checkout/merge attributes are unsupported",
        ));
    }
    Ok(())
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
