use super::*;
impl RepositoryService {
    pub(crate) fn artifact_pool_record(&self, repository: &str) -> Result<Value> {
        let pool: Value = self
            .root
            .read_json(format!("repositories/{repository}/pool.json"))?
            .ok_or_else(|| {
                Error::new(
                    "recovery_blocked",
                    "registered repository has no pool metadata",
                )
            })?;
        validate_pool_state(&pool)?;
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
        self.require_eviction_receipt(&pool, repository)?;
        if path.symlink_metadata().is_ok() {
            self.require_rehydration_owner(r, &pool, repository)?;
            operations::checked_directory(&path)?;
        }
        Ok(())
    }
    fn require_eviction_receipt(&self, pool: &Value, repository: &str) -> Result<()> {
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
        let receipt = eviction.result.as_ref().is_some_and(|result| {
            result["lifecycle"] == "absent"
                && result["storage_generation"] == pool["storage_generation"]
        });
        let owns = eviction.payload["operation"] == "repo.cache-evict"
            && eviction.payload["repository_id"] == repository;
        let acknowledged = eviction.status == crate::state::OperationStatus::Complete && receipt;
        if !acknowledged || !owns {
            return Err(Error::new(
                "recovery_blocked",
                "pool eviction acknowledgement must finish before rehydration",
            )
            .details(json!({"blocking_operation_id":owner})));
        }
        Ok(())
    }
    fn require_rehydration_owner(&self, r: &Request, pool: &Value, repository: &str) -> Result<()> {
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
                && record.payload["repository_id"] == repository
                && next.is_some()
                && record.metadata["rehydrate_storage_generation"].as_u64() == next
        }) {
            return Err(Error::new(
                "recovery_blocked",
                "replacement pool is owned by another pending rehydration",
            ));
        }
        Ok(())
    }
}
fn validate_pool_state(pool: &Value) -> Result<()> {
    let known = matches!(
        pool["lifecycle"].as_str(),
        Some("present" | "evicting" | "absent")
    );
    let generation = pool["storage_generation"].as_u64().unwrap_or(0);
    if (pool["format_version"].as_u64(), known) != (Some(1), true) || generation == 0 {
        return Err(Error::new(
            "state_incompatible",
            "unsupported or malformed repository pool state",
        ));
    }
    Ok(())
}
