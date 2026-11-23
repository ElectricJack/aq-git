use super::import::Incoming;
use super::*;
impl Incoming<'_> {
    pub(super) fn import_bundle(&self, reference: &str) -> Result<()> {
        let op = self.operation;
        let limits = &self.limits;
        let incoming = &self.directory;
        let manifest = &self.manifest;
        let pool = self
            .service
            .repositories
            .pool_path(&self.binding.repository_id);
        let bundle = incoming.join("objects.bundle");
        let quarantine = self
            .service
            .root
            .create_dir(format!("artifacts/quarantine/{op}"))?;
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
        self.initialize_quarantine(&quarantine, &pool)?;
        self.verify_bundle(&quarantine, &verified_bundle, reference)?;
        self.check_quarantine(&quarantine)?;
        self.migrate_objects(&pool, &verified_bundle, reference)
    }
    fn initialize_quarantine(&self, quarantine: &Path, pool: &Path) -> Result<()> {
        let limits = &self.limits;
        let op = self.operation;
        let initialized = self.service.repositories.runner.run_with_disk_budget(
            quarantine,
            &[
                "init".into(),
                "--bare".into(),
                "--object-format=sha1".into(),
                ".".into(),
            ],
            quarantine,
            limits.max_expanded_bytes,
        )?;
        if !initialized.status.success() {
            return Err(Error::new(
                "storage_limit",
                "quarantine initialization exceeded managed budget",
            ));
        }
        self.service.root.write_bytes(
            format!("artifacts/quarantine/{op}/objects/info/alternates"),
            format!("{}\n", pool.join("objects").display()).as_bytes(),
        )?;
        Ok(())
    }
    fn verify_bundle(
        &self,
        quarantine: &Path,
        verified_bundle: &Path,
        reference: &str,
    ) -> Result<()> {
        let limits = &self.limits;
        self.service.git_at(
            quarantine,
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
        let output = self.service.repositories.runner.run_with_disk_budget(
            quarantine,
            &fetch,
            quarantine,
            limits.max_expanded_bytes,
        )?;
        if !output.status.success() {
            return Err(Error::new(
                "invalid_artifact",
                "quarantined bundle import failed",
            ));
        }
        Ok(())
    }
    fn check_quarantine(&self, quarantine: &Path) -> Result<()> {
        let manifest = &self.manifest;
        let limits = &self.limits;
        self.service.git_at(
            quarantine,
            vec![
                "fsck".into(),
                "--full".into(),
                "--strict".into(),
                "--no-reflogs".into(),
            ],
        )?;
        let kind = self.service.git_at(
            quarantine,
            vec!["cat-file".into(), "-t".into(), manifest.source.hex.clone()],
        )?;
        if kind != b"commit\n" {
            return Err(Error::new(
                "invalid_artifact",
                "artifact source must be a commit",
            ));
        }
        if !self
            .service
            .ancestor_at(quarantine, &manifest.base.hex, &manifest.source.hex)?
        {
            return Err(Error::new(
                "invalid_artifact",
                "base is not ancestor of source",
            ));
        }
        self.service
            .validate_tree_at(quarantine, &manifest.source.hex)?;
        if directory_bytes(quarantine, limits.max_expanded_bytes)? > limits.max_expanded_bytes {
            return Err(Error::new(
                "storage_limit",
                "quarantine exceeded storage quota",
            ));
        }
        Ok(())
    }
    fn migrate_objects(&self, pool: &Path, verified_bundle: &Path, reference: &str) -> Result<()> {
        let op = self.operation;
        let manifest = &self.manifest;
        let limits = &self.limits;
        let before = directory_bytes(pool, u64::MAX)?;
        // Reuse the exact validated snapshot. A local repository fetch would
        // invoke upload-pack and repack the same closure under server-side defaults.
        let import = vec![
            "bundle".into(),
            "unbundle".into(),
            verified_bundle.to_string_lossy().into_owned(),
        ];
        // index-pack writes into a private staging directory: a child stopped
        // at its deadline (or failing) leaves no temporary pack in the pool,
        // because dropping the staging removes everything it wrote.
        let staging = crate::temp_packs::PoolStaging::create(pool, op)?;
        let output = self.service.repositories.runner.run_with_disk_budget_env(
            pool,
            &import,
            &staging.environment(),
            pool,
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
        staging.migrate()?;
        Ok(())
    }
}
