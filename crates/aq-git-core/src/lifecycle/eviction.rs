use super::*;
use crate::repository::Binding;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evict {
    expected_storage_generation: u64,
    reconstructible: bool,
    reason: String,
}
impl RepositoryService {
    pub(super) fn evict_pool(&self, r: &Request, binding: &Binding) -> Result<Value> {
        let repo = &binding.repository_id;
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
        self.check_eviction_dependencies(repo, op)?;
        let eviction = PoolEviction::new(self, repo, op, p.expected_storage_generation);
        eviction.run()
    }

    fn check_eviction_dependencies(&self, repo: &str, op: &str) -> Result<()> {
        let mut blockers = self.matching_records("retention", |id| {
            Ok(self.root.retention(id)?.is_some_and(|handle| {
                handle.repository_id == repo && handle.state != RetentionState::Released
            }))
        })?;
        blockers.extend(
            self.matching_records(&format!("repositories/{repo}/workspaces"), |id| {
                Ok(self.workspace_record(repo, id)?.lifecycle != "retired")
            })?,
        );
        blockers.extend(self.matching_records("operations", |id| {
            if id == op {
                return Ok(false);
            }
            Ok(self.root.operation(id)?.is_some_and(|record| {
                record.status == OperationStatus::Pending && record.payload["repository_id"] == repo
            }))
        })?);
        if !blockers.is_empty() {
            return Err(
                Error::new("retention_in_use", "pool eviction has live dependencies")
                    .details(json!({"blockers":blockers})),
            );
        }
        Ok(())
    }
}

struct PoolEviction<'a> {
    service: &'a RepositoryService,
    repo: &'a str,
    op: &'a str,
    expected_generation: u64,
    file: String,
    path: PathBuf,
    trash: PathBuf,
}

impl<'a> PoolEviction<'a> {
    fn new(
        service: &'a RepositoryService,
        repo: &'a str,
        op: &'a str,
        expected_generation: u64,
    ) -> Self {
        Self {
            service,
            repo,
            op,
            expected_generation,
            file: format!("repositories/{repo}/pool.json"),
            path: service.repositories.pool_path(repo),
            trash: service
                .root
                .path()
                .join("trash")
                .join(format!("pool-{repo}-{expected_generation}-{op}")),
        }
    }

    fn run(&self) -> Result<Value> {
        let mut pool: Value = self
            .service
            .root
            .read_json(&self.file)?
            .ok_or_else(|| Error::new("object_unavailable", "pool metadata unavailable"))?;
        let current = pool_generation(&pool)?;
        let next = self
            .expected_generation
            .checked_add(1)
            .ok_or_else(|| Error::new("invalid_request", "storage generation exhausted"))?;
        let resuming = pool["operation_id"] == self.op;
        self.check_phase(&pool, current, next)?;
        if !resuming {
            self.record_intent(&mut pool, current)?;
        }
        if pool["lifecycle"] == "evicting" {
            self.move_to_trash(&mut pool, next)?;
        }
        self.clean_trash(&pool)?;
        Ok(pool)
    }

    fn check_phase(&self, pool: &Value, current: u64, next: u64) -> Result<()> {
        let phase = pool["lifecycle"].as_str().unwrap();
        let resuming = pool["operation_id"] == self.op;
        let expected = match (resuming, phase) {
            (true, "absent") => Some(next),
            (true, "present") => None,
            _ => Some(self.expected_generation),
        };
        if expected != Some(current) {
            return Err(Error::new(
                "binding_conflict",
                "storage generation or eviction phase changed",
            ));
        }
        if !resuming && phase != "present" {
            return Err(Error::new(
                "recovery_blocked",
                "pool is not available for a new eviction",
            ));
        }
        Ok(())
    }

    fn record_intent(&self, pool: &mut Value, current: u64) -> Result<()> {
        let repo = self.repo;
        let op = self.op;
        operations::checked_directory(&self.path).map_err(|_| {
            Error::new(
                "recovery_blocked",
                "managed pool missing without eviction intent",
            )
        })?;
        let marker = json!({"format_version":1,"repository_id":repo,"storage_generation":current,"operation_id":op});
        let marker_path = format!("repositories/{repo}/objects.git/aq-git-eviction.json");
        if let Some(existing) = self.service.root.read_json::<Value>(&marker_path)? {
            if existing != marker {
                return Err(Error::new(
                    "recovery_blocked",
                    "pool has another eviction ownership marker",
                ));
            }
        } else {
            self.service.root.write_json(&marker_path, &marker)?;
        }
        pool["eviction_marker"] = marker;
        if self.trash.symlink_metadata().is_ok() {
            return Err(Error::new(
                "recovery_blocked",
                "eviction trash already exists before intent",
            ));
        }
        use std::os::unix::fs::MetadataExt;
        let identity = fs::symlink_metadata(&self.path)?;
        pool["eviction_identity"] = json!({"device":identity.dev(),"inode":identity.ino()});
        pool["lifecycle"] = json!("evicting");
        pool["operation_id"] = json!(op);
        self.service.root.write_json(&self.file, pool)?;
        Ok(())
    }

    fn move_to_trash(&self, pool: &mut Value, next: u64) -> Result<()> {
        let repo = self.repo;
        self.service.repositories.invalidate(repo);
        self.service.root.create_dir("trash")?;
        let original = self.path.symlink_metadata().is_ok();
        let trashed = self.trash.symlink_metadata().is_ok();
        if original == trashed {
            return Err(Error::new(
                "recovery_blocked",
                "eviction requires exactly one original/trash directory",
            ));
        }
        let source = if original { &self.path } else { &self.trash };
        verify_eviction_directory(source, pool)?;
        if original {
            fs::rename(&self.path, &self.trash)?;
            operations::sync_dir(self.path.parent().unwrap())?;
            operations::sync_dir(self.trash.parent().unwrap())?;
        }
        pool["storage_generation"] = json!(next);
        pool["lifecycle"] = json!("absent");
        self.service.root.write_json(&self.file, pool)?;
        Ok(())
    }

    fn clean_trash(&self, pool: &Value) -> Result<()> {
        if self.path.symlink_metadata().is_ok() {
            return Err(Error::new(
                "recovery_blocked",
                "replacement pool exists during recorded eviction",
            ));
        }
        if self.trash.exists() {
            verify_eviction_directory(&self.trash, pool)?;
            fs::remove_dir_all(&self.trash)?;
            operations::sync_dir(self.trash.parent().unwrap())?;
        }
        Ok(())
    }
}

fn pool_generation(pool: &Value) -> Result<u64> {
    let current = pool["storage_generation"]
        .as_u64()
        .ok_or_else(|| Error::new("state_incompatible", "invalid pool generation"))?;
    let supported_format = pool["format_version"] == 1 && current > 0;
    let supported_phase = matches!(
        pool["lifecycle"].as_str(),
        Some("present" | "evicting" | "absent")
    );
    if !supported_format || !supported_phase {
        return Err(Error::new("state_incompatible", "unsupported pool record"));
    }
    Ok(current)
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
