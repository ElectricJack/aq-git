use super::*;
use crate::state::{OperationRecord, OperationStatus};

pub(crate) struct Pin<'a> {
    pub repo: &'a str,
    pub generation: u64,
    pub handle: &'a str,
    pub resource: &'a str,
    pub refs: Vec<RetentionRef>,
}

impl Pin<'_> {
    fn matches(&self, existing: &RetentionRecord) -> bool {
        let identity = (self.repo, self.generation, self.resource);
        let existing_identity = (
            existing.repository_id.as_str(),
            existing.binding_generation,
            existing.resource.as_str(),
        );
        let active = existing.state == RetentionState::Active && existing.refs == self.refs;
        active && identity == existing_identity
    }
}

pub(crate) struct HandleRelease<'a> {
    pub handle: &'a str,
    pub generation: u64,
    pub operation: &'a str,
    pub reason: &'a str,
}

fn local_creator(record: &RetentionRecord, creation: &OperationRecord) -> bool {
    let pending = creation.status == OperationStatus::Pending
        && creation.payload["repository_id"] == record.repository_id;
    let creating = matches!(
        (
            record.resource.as_str(),
            creation.payload["operation"].as_str()
        ),
        ("candidate", Some("candidate.create"))
            | ("checkpoint", Some("workspace.checkpoint"))
            | ("transfer", Some("artifact.export"))
    );
    pending && creating
}

fn pending_import(record: &RetentionRecord, handle: &str, creation: &OperationRecord) -> bool {
    let importing = creation.status == OperationStatus::Pending
        && creation.payload["operation"] == "artifact.import";
    let matches = creation.payload["repository_id"] == record.repository_id
        && creation.metadata["transfer_id"] == handle;
    importing && matches
}

impl RepositoryService {
    pub(crate) fn pin(&self, pin: Pin<'_>) -> Result<()> {
        let Pin {
            repo,
            generation,
            handle,
            resource,
            ref refs,
        } = pin;
        if let Some(existing) = self.root.retention(handle)? {
            if !pin.matches(&existing) {
                return Err(Error::new(
                    "recovery_blocked",
                    "existing handle does not match durable intent",
                ));
            }
            self.verify_retention_refs(&existing)?;
            return Ok(());
        }
        self.harden_retained_objects(repo, refs)?;
        for reference in refs {
            self.pin_ref(repo, reference)?;
        }
        self.root.create_retention(&RetentionRecord {
            version: 1,
            id: handle.into(),
            repository_id: repo.into(),
            binding_generation: generation,
            generation: 1,
            resource: resource.into(),
            refs: pin.refs,
            state: RetentionState::Active,
            dependencies: BTreeSet::new(),
            release_operation: None,
            release_reason: None,
        })
    }
    fn harden_retained_objects(&self, repo: &str, refs: &[RetentionRef]) -> Result<()> {
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
        Ok(())
    }
    fn read_retained_ref(
        &self,
        repo: &str,
        reference: &RetentionRef,
    ) -> Result<crate::git::GitOutput> {
        self.repositories.runner.run(
            &self.repositories.pool_path(repo),
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                reference.name.clone(),
            ],
        )
    }
    fn pin_ref(&self, repo: &str, reference: &RetentionRef) -> Result<()> {
        let found = self.read_retained_ref(repo, reference)?;
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
        Ok(())
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
    pub(crate) fn release_handle(&self, release: HandleRelease<'_>) -> Result<RetentionRecord> {
        let HandleRelease {
            handle,
            generation,
            operation,
            reason,
        } = release;
        let current = self
            .root
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "retention handle not found"))?;
        let owns_release = current.release_operation.as_deref() == Some(operation);
        let record = match current.state {
            RetentionState::Released if owns_release => return Ok(current),
            RetentionState::Released => {
                return Err(Error::new(
                    "retention_released",
                    "resource was released by another operation",
                ));
            }
            RetentionState::Releasing if owns_release => current,
            RetentionState::Releasing => {
                return Err(Error::new(
                    "retention_conflict",
                    "another release owns the recorded intent",
                ));
            }
            RetentionState::Active => {
                if current.generation != generation {
                    return Err(Error::new(
                        "generation_conflict",
                        "retention generation changed",
                    ));
                }
                self.verify_retention_refs(&current)?;
                self.root.begin_release_with_reason(
                    handle,
                    generation,
                    Some(operation.into()),
                    Some(reason.into()),
                )?
            }
        };
        self.cancel_local_creators(&record, &release)?;
        for reference in &record.refs {
            self.remove_released_ref(&record.repository_id, reference)?;
        }
        self.root.finish_release(handle, record.generation)
    }
    fn remove_released_ref(&self, repo: &str, reference: &RetentionRef) -> Result<()> {
        let output = self.read_retained_ref(repo, reference)?;
        if output.status.success() {
            if String::from_utf8_lossy(&output.stdout).trim() != reference.oid {
                return Err(Error::new(
                    "recovery_blocked",
                    "release found a changed retention ref",
                ));
            }
            self.repositories.git(
                repo,
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
        Ok(())
    }
    fn cancel_local_creators(
        &self,
        record: &RetentionRecord,
        release: &HandleRelease<'_>,
    ) -> Result<()> {
        let HandleRelease {
            handle,
            operation,
            reason,
            ..
        } = *release;
        // Explicit local release cancels a still-pending creator before pins can disappear.
        // Remote publication uncertainty is never a cancellable creator and its dependency
        // must continue to block begin_release.
        let mut creators: Vec<_> = self
            .root
            .operation(handle)?
            .filter(|creation| local_creator(record, creation))
            .into_iter()
            .collect();
        if record.resource == "transfer" {
            creators.extend(self.pending_imports(record, release)?);
        }
        for creation in creators {
            self.root.fail_operation(&creation.id,creation.generation,serde_json::to_value(Error::new("retention_released","local resource creation cancelled by explicit release").details(json!({"reason":reason,"release_operation_id":operation,"retention_handle":handle})))?)?;
        }
        Ok(())
    }
    fn pending_imports(
        &self,
        record: &RetentionRecord,
        release: &HandleRelease<'_>,
    ) -> Result<Vec<OperationRecord>> {
        let mut creators = Vec::new();
        for id in self.record_ids("operations")? {
            if id == release.operation || id == release.handle {
                continue;
            }
            creators.extend(
                self.root
                    .operation(&id)?
                    .filter(|creation| pending_import(record, release.handle, creation)),
            );
        }
        Ok(creators)
    }
}
