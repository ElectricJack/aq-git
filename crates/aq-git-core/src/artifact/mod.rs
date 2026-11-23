use crate::{
    Error, Result, git::runner::directory_bytes, operations, service::RepositoryService,
    state::RetentionRef,
};
use aq_git_protocol::{ObjectId, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const OPERATIONS: &[&str] = &["artifact.export", "artifact.import"];
pub const MAX_BUNDLE_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_EXPANDED_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLimits {
    pub max_bundle_bytes: u64,
    pub max_expanded_bytes: u64,
    pub disk_reserve_bytes: u64,
}
impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_bundle_bytes: MAX_BUNDLE_BYTES,
            max_expanded_bytes: MAX_EXPANDED_BYTES,
            disk_reserve_bytes: DISK_RESERVE_BYTES,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub repository_id: String,
    pub repository_identity: String,
    pub binding_generation: u64,
    pub transfer_id: String,
    pub object_format: String,
    pub base: ObjectId,
    pub source: ObjectId,
    pub prerequisites: Vec<ObjectId>,
    pub bundle_size: u64,
    pub bundle_sha256: Option<String>,
    pub producer_installation_id: String,
    pub reference_only: bool,
    #[serde(default)]
    pub caller_correlation: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    source: ObjectId,
    base: ObjectId,
    retention_handle: String,
    #[serde(default)]
    confirmed_prerequisites: Vec<ObjectId>,
    #[serde(default)]
    reference_only: bool,
    #[serde(default)]
    caller_correlation: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    artifact_handle: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferRecord {
    pub version: u32,
    pub transfer_id: String,
    pub manifest_digest: String,
    pub manifest: Manifest,
    pub receipt: Value,
}
mod bundle;
mod export;
mod import;
mod pool;
mod quarantine;
use crate::repository::Binding;
use bundle::{bundle_header, validate_manifest};
fn open_file(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(Error::new(
            "invalid_artifact",
            "artifact member must be a regular file",
        ));
    }
    Ok(f)
}
pub(crate) fn digest_file(path: &Path, limit: u64) -> Result<(u64, String)> {
    let mut file = open_file(path)?;
    if file.metadata()?.len() > limit {
        return Err(Error::new(
            "limit_exceeded",
            "artifact exceeds compressed byte limit",
        ));
    }
    let mut h = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > limit {
            return Err(Error::new(
                "limit_exceeded",
                "artifact grew beyond compressed byte limit",
            ));
        }
        h.update(&buffer[..n]);
    }
    Ok((count, hex::encode(h.finalize())))
}
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.0).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            let _ = fs::remove_dir_all(&self.0);
            if let Some(parent) = self.0.parent() {
                let _ = operations::sync_dir(parent);
            }
        }
    }
}
fn snapshot(from: &Path, to: &Path, size: u64, digest: &str) -> Result<()> {
    let mut input = open_file(from)?;
    discard_replaced_snapshot(to)?;
    let mut output = durable_bundle(to)?;
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buf = [0; 65536];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > size {
            return Err(Error::new(
                "artifact_digest_mismatch",
                "artifact changed while snapshotting",
            ));
        }
        hash.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    if total != size || hex::encode(hash.finalize()) != digest {
        return Err(Error::new(
            "artifact_digest_mismatch",
            "artifact changed while snapshotting",
        ));
    }
    output.sync_all()?;
    Ok(())
}
pub(crate) fn reserve(path: &Path, required: u64, reserve_bytes: u64) -> Result<()> {
    let raw = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::new("invalid_path", "NUL in path"))?;
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(raw.as_ptr(), &mut v) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let available = (v.f_bavail as u128) * (v.f_frsize as u128);
    if available < required as u128 + reserve_bytes as u128 {
        return Err(Error::new(
            "storage_limit",
            "insufficient free disk reserve",
        ));
    }
    Ok(())
}
fn discard_replaced_snapshot(to: &Path) -> Result<()> {
    if to.exists() {
        if fs::symlink_metadata(to)?.file_type().is_symlink() {
            return Err(Error::new("invalid_path", "quarantine snapshot replaced").details(json!({"origin":"artifact_snapshot","entry":"verified.bundle","entry_type":"symlink"})));
        }
        fs::remove_file(to)?;
    }
    Ok(())
}
pub(crate) fn durable_bundle(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}
impl RepositoryService {
    pub fn artifact_limits(&self) -> Result<ArtifactLimits> {
        let limits: ArtifactLimits = self
            .root
            .read_json("artifact_limits.json")?
            .unwrap_or_default();
        if !(1..=usize::MAX as u64).contains(&limits.max_bundle_bytes)
            || limits.max_expanded_bytes == 0
        {
            return Err(Error::new(
                "invalid_configuration",
                "artifact byte limits must be positive and addressable",
            ));
        }
        Ok(limits)
    }

    pub(crate) fn artifact_handle(&self, r: &Request) -> Result<Value> {
        self.check_artifact_admission(r)?;
        let binding = self.repositories.checked_binding(r)?;
        match r.operation.as_str() {
            "artifact.export" => self.export_artifact(r, &binding),
            "artifact.import" => self.import_artifact(r, &binding),
            _ => Err(Error::new(
                "unsupported_feature",
                "artifact operation unavailable",
            )),
        }
    }
}
