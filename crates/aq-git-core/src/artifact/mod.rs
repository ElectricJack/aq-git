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
fn digest_file(path: &Path, limit: u64) -> Result<(u64, String)> {
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
    if to.exists() {
        if fs::symlink_metadata(to)?.file_type().is_symlink() {
            return Err(Error::new("invalid_path", "quarantine snapshot replaced").details(json!({"origin":"artifact_snapshot","entry":"verified.bundle","entry_type":"symlink"})));
        }
        fs::remove_file(to)?;
    }
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
fn reserve(path: &Path, required: u64, reserve_bytes: u64) -> Result<()> {
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
fn validate_manifest(m: &Manifest, limits: &ArtifactLimits) -> Result<()> {
    if m.format_version != 1 {
        return Err(Error::new(
            "unsupported_feature",
            "unsupported artifact format",
        ));
    }
    operations::id(&m.repository_id)?;
    operations::id(&m.transfer_id)?;
    operations::id(&m.producer_installation_id)?;
    if m.object_format != "sha1" {
        return Err(Error::new(
            "unsupported_feature",
            "artifact object format unsupported",
        ));
    }
    operations::oid(&m.base)?;
    operations::oid(&m.source)?;
    if m.prerequisites.len() > 64 {
        return Err(Error::new(
            "limit_exceeded",
            "too many artifact prerequisites",
        ));
    }
    let mut seen = BTreeSet::new();
    for p in &m.prerequisites {
        operations::oid(p)?;
        if !seen.insert(p.hex.clone()) {
            return Err(Error::new("invalid_artifact", "duplicate prerequisite"));
        }
    }
    if m.bundle_size > limits.max_bundle_bytes {
        return Err(Error::new(
            "limit_exceeded",
            "artifact exceeds compressed byte limit",
        ));
    }
    if m.reference_only {
        if m.bundle_size != 0 || m.bundle_sha256.is_some() {
            return Err(Error::new(
                "invalid_artifact",
                "reference-only manifest contains bundle metadata",
            ));
        }
    } else if m.bundle_size == 0
        || m.bundle_sha256.as_ref().is_none_or(|s| {
            s.len() != 64
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    {
        return Err(Error::new("invalid_artifact", "invalid bundle size/digest"));
    }
    Ok(())
}
fn bundle_header(path: &Path) -> Result<(BTreeSet<String>, String, String)> {
    let mut reader = BufReader::new(open_file(path)?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line != "# v2 git bundle\n" {
        return Err(Error::new(
            "unsupported_feature",
            "only complete SHA-1 v2 bundles are accepted",
        ));
    }
    let mut prerequisites = BTreeSet::new();
    let mut refs = Vec::new();
    let mut bytes = line.len();
    loop {
        line.clear();
        let n = reader.by_ref().take(16385).read_line(&mut line)?;
        bytes += n;
        if n == 0 || n > 16384 || bytes > 1024 * 1024 {
            return Err(Error::new(
                "invalid_artifact",
                "invalid bounded bundle header",
            ));
        }
        if line == "\n" {
            break;
        }
        let line = line.trim_end_matches('\n');
        if let Some(p) = line.strip_prefix('-') {
            let oid = p.split_once(' ').map(|x| x.0).unwrap_or(p);
            operations::oid(&ObjectId {
                algorithm: "sha1".into(),
                hex: oid.into(),
            })?;
            if !prerequisites.insert(oid.into()) {
                return Err(Error::new(
                    "invalid_artifact",
                    "duplicate bundle prerequisite",
                ));
            }
        } else {
            let Some((oid, name)) = line.split_once(' ') else {
                return Err(Error::new("invalid_artifact", "malformed advertised ref"));
            };
            operations::oid(&ObjectId {
                algorithm: "sha1".into(),
                hex: oid.into(),
            })?;
            refs.push((oid.to_owned(), name.to_owned()));
        }
    }
    if refs.len() != 1 {
        return Err(Error::new(
            "invalid_artifact",
            "bundle must advertise exactly one transfer ref",
        ));
    }
    let (oid, name) = refs.pop().unwrap();
    Ok((prerequisites, oid, name))
}
fn durable_bundle(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}
impl RepositoryService {
    fn artifact_pool_record(&self, repository: &str) -> Result<Value> {
        let pool: Value = self
            .root
            .read_json(format!("repositories/{repository}/pool.json"))?
            .ok_or_else(|| {
                Error::new(
                    "recovery_blocked",
                    "registered repository has no pool metadata",
                )
            })?;
        if pool["format_version"] != 1
            || pool["storage_generation"]
                .as_u64()
                .is_none_or(|generation| generation == 0)
            || !matches!(
                pool["lifecycle"].as_str(),
                Some("present" | "evicting" | "absent")
            )
        {
            return Err(Error::new(
                "state_incompatible",
                "unsupported or malformed repository pool state",
            ));
        }
        if let Some(operation) = pool.get("operation_id")
            && operation
                .as_str()
                .is_none_or(|id| operations::id(id).is_err())
        {
            return Err(Error::new(
                "state_incompatible",
                "pool operation identity is malformed",
            ));
        }
        Ok(pool)
    }
    pub(crate) fn check_artifact_admission(&self, r: &Request) -> Result<()> {
        if r.operation != "artifact.import" {
            return Ok(());
        }
        let binding = self.repositories.checked_binding(r)?;
        let repository = &binding.repository_id;
        let pool = self.artifact_pool_record(repository)?;
        let path = self.repositories.pool_path(repository);
        if pool["lifecycle"] == "evicting" {
            return Err(Error::new("recovery_blocked","pool eviction must finish before importing objects").details(json!({"blocking_operation_id":pool["operation_id"],"storage_generation":pool["storage_generation"]})));
        }
        if pool["lifecycle"] == "present" {
            operations::checked_directory(&path).map_err(|_| {
                Error::new(
                    "recovery_blocked",
                    "present pool is missing or was replaced without an eviction intent",
                )
            })?;
            return Ok(());
        }
        let owner = pool["operation_id"].as_str().ok_or_else(|| {
            Error::new(
                "state_incompatible",
                "absent pool has no eviction operation",
            )
        })?;
        let eviction = self.root.operation(owner)?.ok_or_else(|| {
            Error::new(
                "recovery_blocked",
                "absent pool eviction receipt is missing",
            )
        })?;
        if eviction.status != crate::state::OperationStatus::Complete
            || eviction.payload["operation"] != "repo.cache-evict"
            || eviction.payload["repository_id"] != *repository
            || eviction.result.as_ref().is_none_or(|result| {
                result["lifecycle"] != "absent"
                    || result["storage_generation"] != pool["storage_generation"]
            })
        {
            return Err(Error::new(
                "recovery_blocked",
                "pool eviction acknowledgement must finish before rehydration",
            )
            .details(json!({"blocking_operation_id":owner})));
        }
        if path.symlink_metadata().is_ok() {
            let own = r
                .operation_id
                .as_deref()
                .map(|operation| self.root.operation(operation))
                .transpose()?
                .flatten();
            let next = pool["storage_generation"].as_u64().unwrap().checked_add(1);
            if !own.as_ref().is_some_and(|record| {
                record.status == crate::state::OperationStatus::Pending
                    && record.payload["operation"] == "artifact.import"
                    && record.payload["repository_id"] == *repository
                    && next.is_some()
                    && record.metadata["rehydrate_storage_generation"].as_u64() == next
            }) {
                return Err(Error::new(
                    "recovery_blocked",
                    "replacement pool is owned by another pending rehydration",
                ));
            }
            operations::checked_directory(&path)?;
        }
        Ok(())
    }
    pub fn artifact_limits(&self) -> Result<ArtifactLimits> {
        let limits: ArtifactLimits = self
            .root
            .read_json("artifact_limits.json")?
            .unwrap_or_default();
        if limits.max_bundle_bytes == 0
            || limits.max_expanded_bytes == 0
            || limits.max_bundle_bytes > usize::MAX as u64
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
    fn export_artifact(&self, r: &Request, b: &crate::repository::Binding) -> Result<Value> {
        let limits = self.artifact_limits()?;
        let p: Export = serde_json::from_value(r.params.clone())?;
        let source = operations::oid(&p.source)?;
        let base = operations::oid(&p.base)?;
        let op = operations::opid(r)?;
        let repo = &b.repository_id;
        let source_handle = self.active_handle(repo, &p.retention_handle)?;
        if !source_handle
            .refs
            .iter()
            .any(|reference| reference.oid == source)
        {
            return Err(Error::new(
                "retention_conflict",
                "source is not owned by supplied retention handle",
            ));
        }
        self.commit_exists(repo, source)?;
        self.commit_exists(repo, base)?;
        if !self.ancestor_at(&self.repositories.pool_path(repo), base, source)? {
            return Err(Error::new(
                "invalid_artifact",
                "base is not ancestor of source",
            ));
        }
        self.validate_tree_features(repo, source)?;
        let mut unique = BTreeSet::new();
        for prerequisite in &p.confirmed_prerequisites {
            let oid = operations::oid(prerequisite)?;
            if !unique.insert(oid.to_owned()) {
                return Err(Error::new(
                    "invalid_request",
                    "duplicate confirmed prerequisite",
                ));
            }
            self.commit_exists(repo, oid)?;
            if !self.ancestor_at(&self.repositories.pool_path(repo), oid, source)? {
                return Err(Error::new(
                    "invalid_request",
                    "prerequisite is not an ancestor of source",
                ));
            }
        }
        if p.confirmed_prerequisites.len() > 64 {
            return Err(Error::new("limit_exceeded", "too many prerequisites"));
        }
        if p.reference_only && !unique.contains(source) {
            return Err(Error::new(
                "prerequisite_missing",
                "reference-only export requires receiver-retained source confirmation",
            ));
        }
        if !p.reference_only && unique.contains(source) {
            return Err(Error::new(
                "invalid_request",
                "use reference-only for an already retained source",
            ));
        }
        self.root.acquire_dependency(&p.retention_handle, op)?;
        // A transfer owns its pin independently of the source handle before transport can start.
        let reference = format!("refs/aq-git/transfers/{op}");
        let pinned = self.pin(
            repo,
            b.binding_generation,
            op,
            "transfer",
            vec![RetentionRef {
                name: reference.clone(),
                oid: source.into(),
            }],
        );
        if let Err(error) = pinned {
            self.root.release_dependency(&p.retention_handle, op)?;
            return Err(error);
        }
        self.root.release_dependency(&p.retention_handle, op)?;
        let directory = self.root.create_dir(format!("artifacts/{op}"))?;
        reserve(
            &directory,
            limits.max_bundle_bytes,
            limits.disk_reserve_bytes,
        )?;
        let (bundle_size, bundle_sha256) = if p.reference_only {
            (0, None)
        } else {
            let temporary = directory.join("objects.bundle.partial");
            if temporary.exists() {
                if fs::symlink_metadata(&temporary)?.file_type().is_symlink() {
                    return Err(Error::new("invalid_path", "artifact temporary replaced"));
                }
                fs::remove_file(&temporary)?;
            }
            let mut args = vec![
                "bundle".into(),
                "create".into(),
                "-".into(),
                reference.clone(),
            ];
            for prerequisite in &p.confirmed_prerequisites {
                args.push(format!("^{}", prerequisite.hex));
            }
            let output = self.repositories.runner.run_to_file(
                &self.repositories.pool_path(repo),
                &args,
                durable_bundle(&temporary)?,
                limits.max_bundle_bytes as usize,
            )?;
            if !output.status.success() {
                return Err(Error::new("git_failed", "could not export bundle"));
            }
            let (size, digest) = digest_file(&temporary, limits.max_bundle_bytes)?;
            fs::rename(&temporary, directory.join("objects.bundle"))?;
            operations::sync_dir(&directory)?;
            (size, Some(digest))
        };
        let installation: Value = self
            .root
            .read_json("format.json")?
            .ok_or_else(|| Error::new("state_corrupt", "missing root format"))?;
        let actual_prerequisites = if p.reference_only {
            p.confirmed_prerequisites
        } else {
            let (actual, _, _) = bundle_header(&directory.join("objects.bundle"))?;
            if !actual.is_subset(&unique) {
                return Err(Error::new(
                    "invalid_artifact",
                    "Git emitted an unconfirmed prerequisite",
                ));
            }
            actual
                .into_iter()
                .map(|hex| ObjectId {
                    algorithm: "sha1".into(),
                    hex,
                })
                .collect()
        };
        let manifest = Manifest {
            format_version: 1,
            repository_id: repo.clone(),
            repository_identity: b.identity.clone(),
            binding_generation: b.binding_generation,
            transfer_id: op.into(),
            object_format: "sha1".into(),
            base: p.base,
            source: p.source,
            prerequisites: actual_prerequisites,
            bundle_size,
            bundle_sha256,
            producer_installation_id: installation["installation_id"]
                .as_str()
                .ok_or_else(|| Error::new("state_corrupt", "missing installation ID"))?
                .into(),
            reference_only: p.reference_only,
            caller_correlation: p.caller_correlation,
        };
        validate_manifest(&manifest, &limits)?;
        self.root
            .write_json(format!("artifacts/{op}/manifest.json"), &manifest)?;
        Ok(json!({"artifact_handle":op,"manifest":manifest,"retention_handle":op}))
    }
    fn import_artifact(&self, r: &Request, b: &crate::repository::Binding) -> Result<Value> {
        let limits = self.artifact_limits()?;
        let p: Import = serde_json::from_value(r.params.clone())?;
        uuid::Uuid::parse_str(&p.artifact_handle)
            .map_err(|_| Error::new("invalid_artifact", "artifact handle must be a UUID"))?;
        let op = operations::opid(r)?;
        let repo = &b.repository_id;
        let relative = format!("artifacts/incoming/{}", p.artifact_handle);
        let incoming = self.root.create_dir(&relative)?;
        operations::checked_directory(&incoming)?;
        let file = open_file(&incoming.join("manifest.json"))?;
        if file.metadata()?.len() > 1024 * 1024 {
            return Err(Error::new(
                "limit_exceeded",
                "artifact manifest exceeds byte limit",
            ));
        }
        let manifest: Manifest = serde_json::from_reader(file.take(1024 * 1024 + 1))?;
        validate_manifest(&manifest, &limits)?;
        if manifest.repository_id != *repo
            || manifest.repository_identity != b.identity
            || manifest.binding_generation != b.binding_generation
        {
            return Err(Error::new(
                "binding_conflict",
                "artifact repository identity/generation does not match binding",
            ));
        }
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&manifest)?));
        let intent = self
            .root
            .operation(op)?
            .ok_or_else(|| Error::new("operation_not_found", "import durable intent missing"))?;
        if let Some(recorded) = intent
            .metadata
            .get("manifest_digest")
            .and_then(Value::as_str)
        {
            if recorded != digest {
                return Err(Error::new(
                    "idempotency_conflict",
                    "staged artifact changed after import intent",
                ));
            }
        } else {
            self.root.advance_operation(
                op,
                intent.generation,
                "artifact_validated",
                json!({"manifest_digest":digest,"transfer_id":manifest.transfer_id}),
            )?;
        }

        let transfer_path = format!(
            "repositories/{repo}/transfers/{}.json",
            manifest.transfer_id
        );
        if let Some(previous) = self.root.read_json::<TransferRecord>(&transfer_path)? {
            if previous.version != 1 {
                return Err(Error::new(
                    "state_incompatible",
                    "unsupported transfer format",
                ));
            }
            if previous.manifest_digest != digest {
                return Err(Error::new(
                    "idempotency_conflict",
                    "transfer ID already has another manifest",
                ));
            }
            let mut receipt = previous.receipt;
            receipt["historical"] = json!(true);
            receipt["retention_state"] = serde_json::to_value(
                self.root
                    .retention(&manifest.transfer_id)?
                    .map(|record| record.state),
            )?;
            return Ok(receipt);
        }
        if let Some(handle) = self.root.retention(&manifest.transfer_id)?
            && (handle.repository_id != *repo
                || handle.binding_generation != b.binding_generation
                || handle.resource != "transfer"
                || handle.state != crate::state::RetentionState::Active
                || handle.refs
                    != vec![RetentionRef {
                        name: format!("refs/aq-git/transfers/{}", manifest.transfer_id),
                        oid: manifest.source.hex.clone(),
                    }])
        {
            return Err(Error::new(
                "idempotency_conflict",
                "transfer handle conflicts with existing resource",
            ));
        }
        let reference = format!("refs/aq-git/transfers/{}", manifest.transfer_id);
        if !manifest.reference_only {
            let bundle = incoming.join("objects.bundle");
            let (size, hash) = digest_file(&bundle, limits.max_bundle_bytes)?;
            if size != manifest.bundle_size || Some(hash) != manifest.bundle_sha256 {
                return Err(Error::new(
                    "artifact_digest_mismatch",
                    "bundle size/digest differs from manifest",
                ));
            }
            let (prerequisites, head, advertised) = bundle_header(&bundle)?;
            let declared: BTreeSet<_> = manifest
                .prerequisites
                .iter()
                .map(|oid| oid.hex.clone())
                .collect();
            if prerequisites != declared || head != manifest.source.hex || advertised != reference {
                return Err(Error::new(
                    "invalid_artifact",
                    "bundle prerequisites/source/advertised ref differ from manifest",
                ));
            }
        }
        let pool = self.repositories.pool_path(repo);
        let mut pool_record = self.artifact_pool_record(repo)?;
        if pool_record["lifecycle"] == "absent" {
            let lifecycle = self.repositories.repository_lifecycle(repo);
            let _rehydration = crate::git::write_with_deadline(&lifecycle)?;
            self.check_artifact_admission(r)?;
            if manifest.reference_only || !manifest.prerequisites.is_empty() {
                return Err(Error::new(
                    "prerequisite_missing",
                    "evicted pool requires a complete bundle",
                ));
            }
            let record = self
                .root
                .operation(op)?
                .ok_or_else(|| Error::new("operation_not_found", "import intent missing"))?;
            let generation = if let Some(generation) =
                record.metadata["rehydrate_storage_generation"].as_u64()
            {
                generation
            } else {
                let generation = pool_record["storage_generation"]
                    .as_u64()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| Error::new("state_corrupt", "storage generation exhausted"))?;
                let mut metadata = record.metadata;
                metadata["rehydrate_storage_generation"] = json!(generation);
                self.root
                    .advance_operation(op, record.generation, "rehydrating", metadata)?;
                generation
            };
            self.root
                .create_dir(format!("repositories/{repo}/objects.git"))?;
            self.git_at(
                &pool,
                vec![
                    "init".into(),
                    "--bare".into(),
                    "--object-format=sha1".into(),
                    ".".into(),
                ],
            )?;
            pool_record["storage_generation"] = json!(generation);
            pool_record["lifecycle"] = json!("present");
            pool_record["operation_id"] = json!(op);
            self.repositories.invalidate(repo);
            self.root
                .write_json(format!("repositories/{repo}/pool.json"), &pool_record)?;
        }
        for prerequisite in &manifest.prerequisites {
            if self.commit_exists(repo, &prerequisite.hex).is_err() {
                return Err(Error::new(
                    "prerequisite_missing",
                    "receiver lacks retained prerequisite history",
                )
                .details(json!({"prerequisite":prerequisite})));
            }
        }
        if manifest.reference_only {
            if incoming.join("objects.bundle").exists() {
                return Err(Error::new(
                    "invalid_artifact",
                    "reference-only artifact includes a bundle",
                ));
            }
            if !self.artifact_retained(repo, &manifest.source.hex)? {
                return Err(Error::new(
                    "prerequisite_missing",
                    "reference-only source is not actively retained",
                ));
            }
            self.commit_exists(repo, &manifest.source.hex)?;
            self.commit_exists(repo, &manifest.base.hex)?;
            if !self.ancestor_at(&pool, &manifest.base.hex, &manifest.source.hex)? {
                return Err(Error::new(
                    "invalid_artifact",
                    "base is not ancestor of source",
                ));
            }
            self.validate_tree_features(repo, &manifest.source.hex)?;
        } else {
            let bundle = incoming.join("objects.bundle");
            let quarantine = self.root.create_dir(format!("artifacts/quarantine/{op}"))?;
            let _cleanup = Cleanup(quarantine.clone());
            reserve(
                &quarantine,
                limits.max_expanded_bytes,
                limits.disk_reserve_bytes,
            )?;
            let verified_bundle = quarantine.join("verified.bundle");
            snapshot(
                &bundle,
                &verified_bundle,
                manifest.bundle_size,
                manifest.bundle_sha256.as_deref().unwrap(),
            )?;

            let initialized = self.repositories.runner.run_with_disk_budget(
                &quarantine,
                &[
                    "init".into(),
                    "--bare".into(),
                    "--object-format=sha1".into(),
                    ".".into(),
                ],
                &quarantine,
                limits.max_expanded_bytes,
            )?;
            if !initialized.status.success() {
                return Err(Error::new(
                    "storage_limit",
                    "quarantine initialization exceeded managed budget",
                ));
            }
            self.root.write_bytes(
                format!("artifacts/quarantine/{op}/objects/info/alternates"),
                format!("{}\n", pool.join("objects").display()).as_bytes(),
            )?;
            self.git_at(
                &quarantine,
                vec![
                    "bundle".into(),
                    "verify".into(),
                    verified_bundle.to_string_lossy().into_owned(),
                ],
            )?;
            let fetch = vec![
                "-c".into(),
                "fetch.unpackLimit=1".into(),
                "fetch".into(),
                "--no-tags".into(),
                "--no-write-fetch-head".into(),
                verified_bundle.to_string_lossy().into_owned(),
                format!("{reference}:{reference}"),
            ];
            let output = self.repositories.runner.run_with_disk_budget(
                &quarantine,
                &fetch,
                &quarantine,
                limits.max_expanded_bytes,
            )?;
            if !output.status.success() {
                return Err(Error::new(
                    "invalid_artifact",
                    "quarantined bundle import failed",
                ));
            }
            self.git_at(
                &quarantine,
                vec![
                    "fsck".into(),
                    "--full".into(),
                    "--strict".into(),
                    "--no-reflogs".into(),
                ],
            )?;
            let kind = self.git_at(
                &quarantine,
                vec!["cat-file".into(), "-t".into(), manifest.source.hex.clone()],
            )?;
            if kind != b"commit\n" {
                return Err(Error::new(
                    "invalid_artifact",
                    "artifact source must be a commit",
                ));
            }
            if !self.ancestor_at(&quarantine, &manifest.base.hex, &manifest.source.hex)? {
                return Err(Error::new(
                    "invalid_artifact",
                    "base is not ancestor of source",
                ));
            }
            self.artifact_features(&quarantine, &manifest.source.hex)?;
            if directory_bytes(&quarantine, limits.max_expanded_bytes)? > limits.max_expanded_bytes
            {
                return Err(Error::new(
                    "storage_limit",
                    "quarantine exceeded storage quota",
                ));
            }
            let before = directory_bytes(&pool, u64::MAX)?;
            // Reuse the exact validated snapshot. A local repository fetch would
            // invoke upload-pack and repack the same closure under server-side defaults.
            let import = vec![
                "bundle".into(),
                "unbundle".into(),
                verified_bundle.to_string_lossy().into_owned(),
            ];
            let output = self.repositories.runner.run_with_disk_budget(
                &pool,
                &import,
                &pool,
                before.saturating_add(limits.max_expanded_bytes),
            )?;
            if !output.status.success() {
                return Err(Error::new(
                    "git_failed",
                    "validated bundle could not enter managed pool",
                ));
            }
            if output.stdout != format!("{} {reference}\n", manifest.source.hex).as_bytes() {
                return Err(Error::new(
                    "git_failed",
                    "validated bundle import advertised an unexpected source or ref",
                ));
            }
        }
        self.repositories.invalidate(repo);
        self.pin(
            repo,
            b.binding_generation,
            &manifest.transfer_id,
            "transfer",
            vec![RetentionRef {
                name: reference,
                oid: manifest.source.hex.clone(),
            }],
        )?;
        let installation: Value = self
            .root
            .read_json("format.json")?
            .ok_or_else(|| Error::new("state_corrupt", "missing root format"))?;
        let receipt = json!({"receiver_installation_id":installation["installation_id"],"repository_id":repo,"binding_generation":b.binding_generation,"transfer_id":manifest.transfer_id,"source":manifest.source,"retention_handle":manifest.transfer_id,"manifest_digest":digest,"accepted_at":self.root.operation(op)?.ok_or_else(||Error::new("operation_not_found","import intent missing"))?.created_at,"retention_state":"active","historical":false});
        self.root.write_json(
            &transfer_path,
            &TransferRecord {
                version: 1,
                transfer_id: manifest.transfer_id.clone(),
                manifest_digest: digest,
                manifest,
                receipt: receipt.clone(),
            },
        )?;
        Ok(receipt)
    }
    fn artifact_retained(&self, repo: &str, oid: &str) -> Result<bool> {
        let path = self.root.path().join("retention");
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some(id) = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if let Some(handle) = self.root.retention(&id)?
                && handle.repository_id == repo
                && handle.state == crate::state::RetentionState::Active
            {
                for reference in handle.refs {
                    if reference.oid == oid
                        || self.ancestor_at(
                            &self.repositories.pool_path(repo),
                            oid,
                            &reference.oid,
                        )?
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
    fn artifact_features(&self, path: &Path, source: &str) -> Result<()> {
        let entries = self.git_at(path, vec!["ls-tree".into(), "-rz".into(), source.into()])?;
        for entry in entries.split(|b| *b == 0).filter(|x| !x.is_empty()) {
            if entry.starts_with(b"160000 ") {
                return Err(Error::new(
                    "unsupported_feature",
                    "submodules cannot be imported",
                ));
            }
            if let Some(tab) = entry.iter().position(|b| *b == b'\t')
                && entry[tab + 1..].rsplit(|b| *b == b'/').next()
                    == Some(b".gitattributes".as_slice())
            {
                return Err(Error::new(
                    "unsupported_feature",
                    "attribute-controlled checkout/merge is unsupported",
                ));
            }
        }
        Ok(())
    }
}
