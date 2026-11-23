use super::*;
pub(super) struct Incoming<'a> {
    pub(super) service: &'a RepositoryService,
    pub(super) binding: &'a Binding,
    pub(super) operation: &'a str,
    pub(super) limits: ArtifactLimits,
    pub(super) directory: PathBuf,
    pub(super) manifest: Manifest,
}
impl RepositoryService {
    pub(super) fn import_artifact(&self, r: &Request, b: &Binding) -> Result<Value> {
        let limits = self.artifact_limits()?;
        let p: Import = serde_json::from_value(r.params.clone())?;
        uuid::Uuid::parse_str(&p.artifact_handle)
            .map_err(|_| Error::new("invalid_artifact", "artifact handle must be a UUID"))?;
        let op = operations::opid(r)?;
        let relative = format!("artifacts/incoming/{}", p.artifact_handle);
        let incoming = self.root.create_dir(&relative)?;
        operations::checked_directory(&incoming)?;
        let manifest = read_manifest(&incoming, &limits, b)?;
        Incoming {
            service: self,
            binding: b,
            operation: op,
            limits,
            directory: incoming,
            manifest,
        }
        .receive(r)
    }
}
fn read_manifest(incoming: &Path, limits: &ArtifactLimits, b: &Binding) -> Result<Manifest> {
    let file = open_file(&incoming.join("manifest.json"))?;
    if file.metadata()?.len() > 1024 * 1024 {
        return Err(Error::new(
            "limit_exceeded",
            "artifact manifest exceeds byte limit",
        ));
    }
    let manifest: Manifest = serde_json::from_reader(file.take(1024 * 1024 + 1))?;
    validate_manifest(&manifest, limits)?;
    let target = (
        &manifest.repository_id,
        &manifest.repository_identity,
        manifest.binding_generation,
    );
    if target != (&b.repository_id, &b.identity, b.binding_generation) {
        return Err(Error::new(
            "binding_conflict",
            "artifact repository identity/generation does not match binding",
        ));
    }
    Ok(manifest)
}
impl Incoming<'_> {
    fn receive(self, request: &Request) -> Result<Value> {
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&self.manifest)?));
        self.record_intent(&digest)?;
        let transfer_path = format!(
            "repositories/{}/transfers/{}.json",
            self.binding.repository_id, self.manifest.transfer_id
        );
        if let Some(previous) = self
            .service
            .root
            .read_json::<TransferRecord>(&transfer_path)?
        {
            return self.historical_receipt(previous, &digest);
        }
        self.check_handle()?;
        let reference = format!("refs/aq-git/transfers/{}", self.manifest.transfer_id);
        self.check_bundle(&reference)?;
        self.ensure_pool(request)?;
        self.require_prerequisites()?;
        if self.manifest.reference_only {
            self.check_retained_source()?;
        } else {
            self.import_bundle(&reference)?;
        }
        self.write_receipt(reference, digest, transfer_path)
    }
    fn record_intent(&self, digest: &str) -> Result<()> {
        let op = self.operation;
        let manifest = &self.manifest;
        let intent =
            self.service.root.operation(op)?.ok_or_else(|| {
                Error::new("operation_not_found", "import durable intent missing")
            })?;
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
            self.service.root.advance_operation(
                op,
                intent.generation,
                "artifact_validated",
                json!({"manifest_digest":digest,"transfer_id":manifest.transfer_id}),
            )?;
        }
        Ok(())
    }
    fn historical_receipt(&self, previous: TransferRecord, digest: &str) -> Result<Value> {
        let manifest = &self.manifest;
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
            self.service
                .root
                .retention(&manifest.transfer_id)?
                .map(|record| record.state),
        )?;
        Ok(receipt)
    }
    fn check_handle(&self) -> Result<()> {
        let manifest = &self.manifest;
        let Some(handle) = self.service.root.retention(&manifest.transfer_id)? else {
            return Ok(());
        };
        let expected = RetentionRef {
            name: format!("refs/aq-git/transfers/{}", manifest.transfer_id),
            oid: manifest.source.hex.clone(),
        };
        let binding = (&handle.repository_id, handle.binding_generation);
        let active_transfer =
            handle.resource == "transfer" && handle.state == crate::state::RetentionState::Active;
        let owns_ref = active_transfer && handle.refs.as_slice() == [expected];
        if binding != (&self.binding.repository_id, self.binding.binding_generation) || !owns_ref {
            return Err(Error::new(
                "idempotency_conflict",
                "transfer handle conflicts with existing resource",
            ));
        }
        Ok(())
    }
    fn check_bundle(&self, reference: &str) -> Result<()> {
        let manifest = &self.manifest;
        let incoming = &self.directory;
        let limits = &self.limits;
        if manifest.reference_only {
            return Ok(());
        }
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
        if (prerequisites, head.as_str(), advertised.as_str())
            != (declared, manifest.source.hex.as_str(), reference)
        {
            return Err(Error::new(
                "invalid_artifact",
                "bundle prerequisites/source/advertised ref differ from manifest",
            ));
        }
        Ok(())
    }
    fn ensure_pool(&self, r: &Request) -> Result<()> {
        let repo = &self.binding.repository_id;
        let op = self.operation;
        let manifest = &self.manifest;
        let pool = self.service.repositories.pool_path(repo);
        let mut pool_record = self.service.artifact_pool_record(repo)?;
        if pool_record["lifecycle"] == "absent" {
            let lifecycle = self.service.repositories.repository_lifecycle(repo);
            let _rehydration = crate::git::write_with_deadline(&lifecycle)?;
            self.service.check_artifact_admission(r)?;
            if manifest.reference_only || !manifest.prerequisites.is_empty() {
                return Err(Error::new(
                    "prerequisite_missing",
                    "evicted pool requires a complete bundle",
                ));
            }
            let generation = self.rehydration_generation(&pool_record)?;
            self.service
                .root
                .create_dir(format!("repositories/{repo}/objects.git"))?;
            self.service.git_at(
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
            self.service.repositories.invalidate(repo);
            self.service
                .root
                .write_json(format!("repositories/{repo}/pool.json"), &pool_record)?;
        }
        Ok(())
    }
    fn rehydration_generation(&self, pool_record: &Value) -> Result<u64> {
        let op = self.operation;
        let record = self
            .service
            .root
            .operation(op)?
            .ok_or_else(|| Error::new("operation_not_found", "import intent missing"))?;
        Ok(
            if let Some(generation) = record.metadata["rehydrate_storage_generation"].as_u64() {
                generation
            } else {
                let generation = pool_record["storage_generation"]
                    .as_u64()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| Error::new("state_corrupt", "storage generation exhausted"))?;
                let mut metadata = record.metadata;
                metadata["rehydrate_storage_generation"] = json!(generation);
                self.service.root.advance_operation(
                    op,
                    record.generation,
                    "rehydrating",
                    metadata,
                )?;
                generation
            },
        )
    }
    fn require_prerequisites(&self) -> Result<()> {
        let manifest = &self.manifest;
        let repo = &self.binding.repository_id;
        for prerequisite in &manifest.prerequisites {
            if self.service.commit_exists(repo, &prerequisite.hex).is_err() {
                return Err(Error::new(
                    "prerequisite_missing",
                    "receiver lacks retained prerequisite history",
                )
                .details(json!({"prerequisite":prerequisite})));
            }
        }
        Ok(())
    }
    fn check_retained_source(&self) -> Result<()> {
        let manifest = &self.manifest;
        let repo = &self.binding.repository_id;
        let incoming = &self.directory;
        let pool = self.service.repositories.pool_path(repo);
        if incoming.join("objects.bundle").exists() {
            return Err(Error::new(
                "invalid_artifact",
                "reference-only artifact includes a bundle",
            ));
        }
        if !self.service.artifact_retained(repo, &manifest.source.hex)? {
            return Err(Error::new(
                "prerequisite_missing",
                "reference-only source is not actively retained",
            ));
        }
        self.service.commit_exists(repo, &manifest.source.hex)?;
        self.service.commit_exists(repo, &manifest.base.hex)?;
        if !self
            .service
            .ancestor_at(&pool, &manifest.base.hex, &manifest.source.hex)?
        {
            return Err(Error::new(
                "invalid_artifact",
                "base is not ancestor of source",
            ));
        }
        self.service
            .validate_tree_features(repo, &manifest.source.hex)?;
        Ok(())
    }
    fn write_receipt(
        self,
        reference: String,
        digest: String,
        transfer_path: String,
    ) -> Result<Value> {
        let b = self.binding;
        let repo = &b.repository_id;
        let op = self.operation;
        let manifest = self.manifest;
        self.service.repositories.invalidate(repo);
        self.service.pin(operations::Pin {
            repo,
            generation: b.binding_generation,
            handle: &manifest.transfer_id,
            resource: "transfer",
            refs: vec![RetentionRef {
                name: reference,
                oid: manifest.source.hex.clone(),
            }],
        })?;
        let installation: Value = self
            .service
            .root
            .read_json("format.json")?
            .ok_or_else(|| Error::new("state_corrupt", "missing root format"))?;
        let receipt = json!({"receiver_installation_id":installation["installation_id"],"repository_id":repo,"binding_generation":b.binding_generation,"transfer_id":manifest.transfer_id,"source":manifest.source,"retention_handle":manifest.transfer_id,"manifest_digest":digest,"accepted_at":self.service.root.operation(op)?.ok_or_else(||Error::new("operation_not_found","import intent missing"))?.created_at,"retention_state":"active","historical":false});
        self.service.root.write_json(
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
}
impl RepositoryService {
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
                && self.artifact_handle_contains(&handle, repo, oid)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn artifact_handle_contains(
        &self,
        handle: &crate::state::RetentionRecord,
        repo: &str,
        oid: &str,
    ) -> Result<bool> {
        if handle.repository_id != repo || handle.state != crate::state::RetentionState::Active {
            return Ok(false);
        }
        for reference in &handle.refs {
            if reference.oid == oid
                || self.ancestor_at(&self.repositories.pool_path(repo), oid, &reference.oid)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
