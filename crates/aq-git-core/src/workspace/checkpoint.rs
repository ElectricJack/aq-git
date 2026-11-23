use super::*;

impl RepositoryService {
    pub(super) fn checkpoint_workspace(
        &self,
        r: &Request,
        binding: &crate::repository::Binding,
    ) -> Result<Value> {
        let repo = &binding.repository_id;
        let mut p: Checkpoint = serde_json::from_value(r.params.clone())?;
        operations::oid(&p.expected_head)?;
        if !p.quiescent {
            return Err(Error::new(
                "workspace_changed",
                "caller must stop all workspace writers",
            ));
        }
        if p.mode != "head" && p.mode != "all" {
            return Err(Error::new(
                "invalid_request",
                "checkpoint mode must be head or all",
            ));
        }
        let op = operations::opid(r)?;
        let checkpointing = CheckpointOperation {
            service: self,
            repo,
            op,
            generation: binding.binding_generation,
            record_path: format!("repositories/{repo}/checkpoints/{op}.json"),
        };
        checkpointing.run(&mut p)
    }
}
struct CheckpointOperation<'a> {
    service: &'a RepositoryService,
    repo: &'a str,
    op: &'a str,
    generation: u64,
    record_path: String,
}
impl CheckpointOperation<'_> {
    fn run(&self, p: &mut Checkpoint) -> Result<Value> {
        let repo = self.repo;
        let op = self.op;
        let mut w = self.service.workspace_record(repo, &p.workspace_id)?;
        if w.lifecycle != "active" {
            return Err(Error::new("workspace_changed", "workspace is not active"));
        }
        self.service.workspace_integrity(&w)?;
        let mut checkpoint = if let Some(c) = self
            .service
            .root
            .read_json::<CheckpointRecord>(&self.record_path)?
        {
            c
        } else {
            self.prepare_record(&w, p)?
        };
        self.validate_record(&w, &checkpoint, p)?;
        if checkpoint.completed {
            self.remove_temporary_index()?;
            return Ok(checkpoint.result);
        }
        let (workspace_updated, head, current_index) = self.workspace_position(&w, &checkpoint)?;
        self.retain_source(&w, &checkpoint)?;
        if !workspace_updated {
            self.apply(&mut w, &checkpoint, (&head, &current_index))?;
        }
        if let Some(previous) = &checkpoint.old_checkpoint
            && previous != op
        {
            self.service
                .root
                .release_dependency(previous, &w.workspace_id)?;
        }
        checkpoint.result = json!({"workspace_id":w.workspace_id,"workspace_generation":w.generation,"source":operations::wire_oid(&checkpoint.source),"retention_handle":op,"unchanged":checkpoint.source==checkpoint.old_head});
        checkpoint.completed = true;
        self.service
            .root
            .write_json(&self.record_path, &checkpoint)?;
        self.remove_temporary_index()?;
        Ok(checkpoint.result)
    }
    fn validate_record(
        &self,
        w: &WorkspaceRecord,
        checkpoint: &CheckpointRecord,
        p: &Checkpoint,
    ) -> Result<()> {
        let op = self.op;
        let expected = &p.expected_head.hex;
        let recorded = (
            checkpoint.operation_id.as_str(),
            checkpoint.workspace_id.as_str(),
            checkpoint.old_head.as_str(),
        );
        let identity_matches = recorded == (op, w.workspace_id.as_str(), expected.as_str());
        let version_matches = checkpoint.format_version == 1
            && checkpoint.workspace_generation == p.expected_generation;
        if !identity_matches || !version_matches {
            return Err(Error::new(
                "recovery_blocked",
                "checkpoint record differs from durable request",
            ));
        }
        Ok(())
    }
    fn workspace_position(
        &self,
        w: &WorkspaceRecord,
        checkpoint: &CheckpointRecord,
    ) -> Result<(bool, String, String)> {
        let op = self.op;
        let workspace_updated = checkpoint.applied(w, op)?;
        let head = self.service.workspace_head(w)?;
        let current_index = if workspace_updated {
            String::new()
        } else {
            self.index_tree(w)?
        };
        let changed_head = head != checkpoint.old_head && head != checkpoint.source;
        let changed_index = current_index != checkpoint.index_tree_before
            && current_index != checkpoint.source_tree;
        let changed = changed_head || changed_index;
        if !workspace_updated && changed {
            return Err(Error::new(
                "recovery_blocked",
                "workspace HEAD or staged content changed after checkpoint intent",
            ));
        }
        Ok((workspace_updated, head, current_index))
    }
    fn prepare_record(&self, w: &WorkspaceRecord, p: &mut Checkpoint) -> Result<CheckpointRecord> {
        let op = self.op;
        let expected = p.expected_head.hex.clone();
        let expected = expected.as_str();
        if w.generation != p.expected_generation || self.service.workspace_head(w)? != expected {
            return Err(Error::new(
                "workspace_changed",
                "workspace generation or HEAD changed",
            ));
        }
        if p.mode == "head" && !self.service.workspace_status(w)?.is_empty() {
            return Err(Error::new(
                "workspace_dirty",
                "head checkpoint requires a clean workspace",
            ));
        }
        let index_tree_before = self.index_tree(w)?;
        let source = if p.mode == "all" {
            self.snapshot_source(w, p)?
        } else {
            expected.to_owned()
        };
        if !self.service.ancestor_at(&w.path, &w.base, &source)? {
            return Err(Error::new(
                "invalid_request",
                "assignment base is not ancestor of source",
            ));
        }
        self.service.validate_tree_at(&w.path, &source)?;
        let source_tree = self.commit_tree(w, &source)?;
        let c = CheckpointRecord {
            index_tree_before,
            source_tree,
            format_version: 1,
            old_checkpoint: w.last_checkpoint.clone(),
            operation_id: op.into(),
            workspace_id: w.workspace_id.clone(),
            old_head: expected.into(),
            source,
            workspace_generation: w.generation,
            completed: false,
            result: Value::Null,
        };
        self.service.root.write_json(&self.record_path, &c)?;
        Ok(c)
    }
    fn commit_tree(&self, w: &WorkspaceRecord, commit: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.service.git_at(
            &w.path,
            vec!["rev-parse".into(), format!("{commit}^{{tree}}")],
        )?)
        .trim()
        .to_owned())
    }
    fn index_tree(&self, w: &WorkspaceRecord) -> Result<String> {
        Ok(
            String::from_utf8_lossy(&self.service.git_at(&w.path, vec!["write-tree".into()])?)
                .trim()
                .to_owned(),
        )
    }
    fn snapshot_source(&self, w: &WorkspaceRecord, p: &mut Checkpoint) -> Result<String> {
        let op = self.op;
        let expected = p.expected_head.hex.as_str();
        let CheckpointCommit {
            name,
            email,
            message,
            author,
        } = p.commit.take().ok_or_else(|| {
            Error::new("invalid_request", "all checkpoint requires commit metadata")
        })?;
        let metadata = CommitMetadata {
            name,
            email,
            message,
        };
        metadata.validate()?;
        if author
            .as_ref()
            .is_some_and(|author| !operations::valid_commit_identity(&author.name, &author.email))
        {
            return Err(Error::new(
                "invalid_request",
                "invalid explicit commit author",
            ));
        }
        self.service.root.create_dir("indexes")?;
        let index = self.service.root.path().join("indexes").join(op);
        let env = vec![(
            "GIT_INDEX_FILE".into(),
            index.to_string_lossy().into_owned(),
        )];
        self.service
            .git_with(&w.path, vec!["read-tree".into(), expected.into()], &env)?;
        self.service.git_with(
            &w.path,
            vec!["add".into(), "--all".into(), "--".into(), ".".into()],
            &env,
        )?;
        let tree = String::from_utf8_lossy(&self.service.git_with(
            &w.path,
            vec!["write-tree".into()],
            &env,
        )?)
        .trim()
        .to_string();
        let prior = self.commit_tree(w, expected)?;
        if tree != prior {
            return self.service.create_commit(
                &w.path,
                CommitTree {
                    tree: &tree,
                    parents: &[expected.to_string()],
                    metadata: &metadata,
                    author: author.as_ref(),
                    operation: op,
                },
            );
        }
        Ok(expected.to_owned())
    }
    fn retain_source(&self, w: &WorkspaceRecord, checkpoint: &CheckpointRecord) -> Result<()> {
        let op = self.op;
        let repo = self.repo;
        self.service.validate_tree_at(&w.path, &checkpoint.source)?;
        // Transfer the private source closure into the node pool before retention acknowledgement.
        self.service.repositories.git(
            repo,
            vec![
                "fetch".into(),
                "--no-tags".into(),
                "--no-write-fetch-head".into(),
                w.path.to_string_lossy().into_owned(),
                checkpoint.source.clone(),
            ],
        )?;
        self.service
            .validate_tree_features(repo, &checkpoint.source)?;
        self.service.pin(operations::Pin {
            repo,
            generation: self.generation,
            handle: op,
            resource: "checkpoint",
            refs: vec![RetentionRef {
                name: format!("refs/aq-git/checkpoints/{op}"),
                oid: checkpoint.source.clone(),
            }],
        })?;
        self.service.root.acquire_dependency(op, &w.workspace_id)?;
        Ok(())
    }
    fn apply(
        &self,
        w: &mut WorkspaceRecord,
        checkpoint: &CheckpointRecord,
        current: (&str, &str),
    ) -> Result<()> {
        let op = self.op;
        let (head, current_index) = current;
        if head != checkpoint.source {
            if head != checkpoint.old_head {
                return Err(Error::new(
                    "workspace_changed",
                    "checkpoint recovery found an unexpected HEAD",
                ));
            }
            self.service.git_at(
                &w.path,
                vec![
                    "update-ref".into(),
                    "HEAD".into(),
                    checkpoint.source.clone(),
                    checkpoint.old_head.clone(),
                ],
            )?;
        }
        if current_index != checkpoint.source_tree {
            self.service.git_at(
                &w.path,
                vec![
                    "read-tree".into(),
                    "--reset".into(),
                    checkpoint.source.clone(),
                ],
            )?;
        }
        w.last_checkpoint = Some(op.into());
        w.generation = checkpoint.workspace_generation + 1;
        self.service.save_workspace(w)?;
        Ok(())
    }
    fn remove_temporary_index(&self) -> Result<()> {
        let op = self.op;
        let temporary_index = self.service.root.path().join("indexes").join(op);
        if temporary_index.exists() {
            fs::remove_file(&temporary_index)?;
            operations::sync_dir(temporary_index.parent().unwrap())?;
        }
        Ok(())
    }
}

impl CheckpointRecord {
    fn applied(&self, w: &WorkspaceRecord, op: &str) -> Result<bool> {
        let pending =
            w.generation == self.workspace_generation && w.last_checkpoint == self.old_checkpoint;
        let applied = w.generation == self.workspace_generation + 1
            && w.last_checkpoint.as_deref() == Some(op);
        if !pending && !applied {
            return Err(Error::new(
                "workspace_changed",
                "workspace advanced beyond this pending checkpoint",
            ));
        }
        Ok(applied)
    }
}
