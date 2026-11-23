use super::*;
struct Outgoing<'a> {
    service: &'a RepositoryService,
    binding: &'a Binding,
    operation: &'a str,
    limits: ArtifactLimits,
}
impl RepositoryService {
    pub(super) fn export_artifact(&self, r: &Request, b: &Binding) -> Result<Value> {
        let limits = self.artifact_limits()?;
        let p: Export = serde_json::from_value(r.params.clone())?;
        operations::oid(&p.source)?;
        operations::oid(&p.base)?;
        let op = operations::opid(r)?;
        Outgoing {
            service: self,
            binding: b,
            operation: op,
            limits,
        }
        .produce(p)
    }
}
impl Outgoing<'_> {
    fn produce(&self, p: Export) -> Result<Value> {
        let b = self.binding;
        let op = self.operation;
        let repo = &b.repository_id;
        let limits = &self.limits;
        self.verify_source(&p)?;
        let unique = self.confirmed_prerequisites(&p)?;
        let reference = self.pin_transfer(&p)?;
        let directory = self.service.root.create_dir(format!("artifacts/{op}"))?;
        reserve(
            &directory,
            limits.max_bundle_bytes,
            limits.disk_reserve_bytes,
        )?;
        let (bundle_size, bundle_sha256) = self.write_bundle(&directory, &reference, &p)?;
        let installation: Value = self
            .service
            .root
            .read_json("format.json")?
            .ok_or_else(|| Error::new("state_corrupt", "missing root format"))?;
        let actual_prerequisites = self.bundle_prerequisites(&directory, &p, &unique)?;
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
        validate_manifest(&manifest, limits)?;
        self.service
            .root
            .write_json(format!("artifacts/{op}/manifest.json"), &manifest)?;
        Ok(json!({"artifact_handle":op,"manifest":manifest,"retention_handle":op}))
    }

    fn verify_source(&self, p: &Export) -> Result<()> {
        let repo = &self.binding.repository_id;
        let source = p.source.hex.as_str();
        let base = p.base.hex.as_str();
        let source_handle = self.service.active_handle(repo, &p.retention_handle)?;
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
        self.service.commit_exists(repo, source)?;
        self.service.commit_exists(repo, base)?;
        if !self
            .service
            .ancestor_at(&self.service.repositories.pool_path(repo), base, source)?
        {
            return Err(Error::new(
                "invalid_artifact",
                "base is not ancestor of source",
            ));
        }
        self.service.validate_tree_features(repo, source)?;
        Ok(())
    }
    fn confirmed_prerequisites(&self, p: &Export) -> Result<BTreeSet<String>> {
        let repo = &self.binding.repository_id;
        let source = p.source.hex.as_str();
        let mut unique = BTreeSet::new();
        for prerequisite in &p.confirmed_prerequisites {
            let oid = operations::oid(prerequisite)?;
            if !unique.insert(oid.to_owned()) {
                return Err(Error::new(
                    "invalid_request",
                    "duplicate confirmed prerequisite",
                ));
            }
            self.service.commit_exists(repo, oid)?;
            if !self
                .service
                .ancestor_at(&self.service.repositories.pool_path(repo), oid, source)?
            {
                return Err(Error::new(
                    "invalid_request",
                    "prerequisite is not an ancestor of source",
                ));
            }
        }
        p.validate_transfer_mode(unique.contains(source))?;
        Ok(unique)
    }
    fn pin_transfer(&self, p: &Export) -> Result<String> {
        let b = self.binding;
        let repo = &b.repository_id;
        let op = self.operation;
        let source = p.source.hex.as_str();
        self.service
            .root
            .acquire_dependency(&p.retention_handle, op)?;
        // A transfer owns its pin independently of the source handle before transport can start.
        let reference = format!("refs/aq-git/transfers/{op}");
        let pinned = self.service.pin(operations::Pin {
            repo,
            generation: b.binding_generation,
            handle: op,
            resource: "transfer",
            refs: vec![RetentionRef {
                name: reference.clone(),
                oid: source.into(),
            }],
        });
        if let Err(error) = pinned {
            self.service
                .root
                .release_dependency(&p.retention_handle, op)?;
            return Err(error);
        }
        self.service
            .root
            .release_dependency(&p.retention_handle, op)?;
        Ok(reference)
    }
    fn write_bundle(
        &self,
        directory: &Path,
        reference: &str,
        p: &Export,
    ) -> Result<(u64, Option<String>)> {
        let repo = &self.binding.repository_id;
        let limits = &self.limits;
        if p.reference_only {
            return Ok((0, None));
        }
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
            reference.to_owned(),
        ];
        for prerequisite in &p.confirmed_prerequisites {
            args.push(format!("^{}", prerequisite.hex));
        }
        let output = self.service.repositories.runner.run_to_file(
            &self.service.repositories.pool_path(repo),
            &args,
            durable_bundle(&temporary)?,
            limits.max_bundle_bytes as usize,
        )?;
        if !output.status.success() {
            return Err(Error::new("git_failed", "could not export bundle"));
        }
        let (size, digest) = digest_file(&temporary, limits.max_bundle_bytes)?;
        fs::rename(&temporary, directory.join("objects.bundle"))?;
        operations::sync_dir(directory)?;
        Ok((size, Some(digest)))
    }
    fn bundle_prerequisites(
        &self,
        directory: &Path,
        p: &Export,
        unique: &BTreeSet<String>,
    ) -> Result<Vec<ObjectId>> {
        if p.reference_only {
            return Ok(p.confirmed_prerequisites.clone());
        }
        let (actual, _, _) = bundle_header(&directory.join("objects.bundle"))?;
        if !actual.is_subset(unique) {
            return Err(Error::new(
                "invalid_artifact",
                "Git emitted an unconfirmed prerequisite",
            ));
        }
        Ok(actual
            .into_iter()
            .map(|hex| ObjectId {
                algorithm: "sha1".into(),
                hex,
            })
            .collect())
    }
}

impl Export {
    fn validate_transfer_mode(&self, source_confirmed: bool) -> Result<()> {
        if self.confirmed_prerequisites.len() > 64 {
            return Err(Error::new("limit_exceeded", "too many prerequisites"));
        }
        if self.reference_only && !source_confirmed {
            return Err(Error::new(
                "prerequisite_missing",
                "reference-only export requires receiver-retained source confirmation",
            ));
        }
        if !self.reference_only && source_confirmed {
            return Err(Error::new(
                "invalid_request",
                "use reference-only for an already retained source",
            ));
        }
        Ok(())
    }
}
