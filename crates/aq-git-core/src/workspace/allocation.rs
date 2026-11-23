use super::*;

impl RepositoryService {
    pub(super) fn create_workspace(
        &self,
        r: &Request,
        binding: &crate::repository::Binding,
    ) -> Result<Value> {
        let repo = &binding.repository_id;
        let p: Create = serde_json::from_value(r.params.clone())?;
        let base = operations::oid(&p.base)?;
        let wid = operations::opid(r)?;
        self.commit_exists(repo, base)?;
        self.validate_tree_features(repo, base)?;
        let mut w = self.allocate_record(binding, wid, base)?;
        if w.lifecycle == "active" {
            self.workspace_integrity(&w)?;
            return Ok(serde_json::to_value(w)?);
        }
        if w.lifecycle != "allocating" {
            return Err(Error::new(
                "recovery_blocked",
                "workspace allocation has incompatible lifecycle",
            ));
        }
        self.pin(operations::Pin {
            repo,
            generation: binding.binding_generation,
            handle: wid,
            resource: "workspace",
            refs: vec![RetentionRef {
                name: format!("refs/aq-git/workspaces/{wid}"),
                oid: base.into(),
            }],
        })?;
        if !w.path.exists() {
            self.populate_workspace(&mut w)?;
        }
        self.verify_allocation(&w)?;
        w.lifecycle = "active".into();
        self.save_workspace(&w)?;
        Ok(serde_json::to_value(w)?)
    }
    fn allocate_record(
        &self,
        binding: &crate::repository::Binding,
        wid: &str,
        base: &str,
    ) -> Result<WorkspaceRecord> {
        let repo = &binding.repository_id;
        let final_path = self.root.path().join("workspaces").join(wid);
        let w = if let Some(w) = self
            .root
            .read_json::<WorkspaceRecord>(format!("repositories/{repo}/workspaces/{wid}.json"))?
        {
            let requested = (wid, repo.as_str(), wid, base, binding.binding_generation);
            let actual = (
                w.workspace_id.as_str(),
                w.repository_id.as_str(),
                w.allocating_operation.as_str(),
                w.base.as_str(),
                w.binding_generation,
            );
            let durable_path = w.format_version == 1 && w.path == final_path;
            if requested != actual || !durable_path {
                return Err(Error::new(
                    "recovery_blocked",
                    "allocation record differs from durable request",
                ));
            }
            w
        } else {
            let w = WorkspaceRecord {
                format_version: 1,
                workspace_id: wid.into(),
                repository_id: repo.into(),
                binding_generation: binding.binding_generation,
                generation: 1,
                base: base.into(),
                path: final_path.clone(),
                lifecycle: "allocating".into(),
                allocating_operation: wid.into(),
                last_checkpoint: None,
                retiring_operation: None,
                config_digest: String::new(),
            };
            self.save_workspace(&w)?;
            w
        };
        Ok(w)
    }
    fn populate_workspace(&self, w: &mut WorkspaceRecord) -> Result<()> {
        let wid = &w.workspace_id;
        let repo = &w.repository_id;
        let base = w.base.as_str();
        let final_path = &w.path;
        let temporary = self.root.path().join("allocating").join(wid);
        self.root.create_dir(format!("allocating/{wid}"))?;
        if temporary.join(".git").symlink_metadata().is_ok() {
            operations::checked_directory(&temporary.join(".git"))?;
        }
        self.git_at(
            &temporary,
            vec!["init".into(), "--object-format=sha1".into(), ".".into()],
        )?;
        let alternate = self.repositories.pool_path(repo).join("objects");
        // The allocation directory belongs only to this durable operation.
        self.root.write_bytes(
            format!("allocating/{wid}/.git/objects/info/alternates"),
            format!("{}\n", alternate.display()).as_bytes(),
        )?;
        self.git_at(
            &temporary,
            vec!["config".into(), "core.hooksPath".into(), "/dev/null".into()],
        )?;
        self.git_at(
            &temporary,
            vec!["config".into(), "gc.auto".into(), "0".into()],
        )?;
        self.git_at(
            &temporary,
            vec!["checkout".into(), "--detach".into(), base.into()],
        )?;
        w.config_digest = self.workspace_config_digest(&temporary)?;
        self.save_workspace(w)?;
        self.root.write_json(
            format!("allocating/{wid}/.git/aq-git-owner.json"),
            &json!({"workspace_id":wid,"repository_id":repo,"base":base}),
        )?;
        self.root.create_dir("workspaces")?;
        fs::rename(&temporary, final_path)?;
        operations::sync_dir(final_path.parent().unwrap())?;
        operations::sync_dir(temporary.parent().unwrap())?;
        Ok(())
    }

    fn verify_allocation(&self, w: &WorkspaceRecord) -> Result<()> {
        let wid = &w.workspace_id;
        let repo = &w.repository_id;
        let base = w.base.as_str();
        let final_path = &w.path;
        let owner: Value = self
            .root
            .read_json(format!("workspaces/{wid}/.git/aq-git-owner.json"))?
            .ok_or_else(|| {
                Error::new(
                    "workspace_changed",
                    "allocation has no matching ownership marker",
                )
            })?;
        if owner != json!({"workspace_id":wid,"repository_id":repo,"base":base}) {
            return Err(Error::new(
                "workspace_changed",
                "allocation belongs to another workspace",
            ));
        }
        if w.config_digest.is_empty()
            || w.config_digest != self.workspace_config_digest(final_path)?
        {
            return Err(Error::new(
                "workspace_changed",
                "recovered allocation config differs from durable identity",
            ));
        }
        if self.workspace_head(w)? != base || !self.workspace_status(w)?.is_empty() {
            return Err(Error::new(
                "workspace_changed",
                "recovered allocation HEAD or files changed",
            ));
        }
        Ok(())
    }
}
