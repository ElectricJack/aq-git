use super::*;

impl RepositoryService {
    pub(super) fn retire_workspace(&self, r: &Request, repo: &str) -> Result<Value> {
        let p: Retire = serde_json::from_value(r.params.clone())?;
        let expected = operations::oid(&p.expected_head)?;
        let op = operations::opid(r)?;
        let mut w = self.workspace_record(repo, &p.workspace_id)?;
        self.begin_retirement(&mut w, &p, op)?;
        self.root.create_dir("trash")?;
        let trash = self.root.path().join("trash").join(&w.workspace_id);
        let original_exists = w.path.symlink_metadata().is_ok();
        let trash_exists = trash.symlink_metadata().is_ok();
        match (w.lifecycle.as_str(), original_exists, trash_exists) {
            (_, true, true) => {
                return Err(Error::new(
                    "recovery_blocked",
                    "workspace and retirement trash both exist",
                ));
            }
            ("retired", true, false) => {
                return Err(Error::new(
                    "recovery_blocked",
                    "retired workspace path was replaced",
                ));
            }
            ("retired", false, trash_exists) => {
                if trash_exists {
                    self.finish_retired_trash(&w, &trash, expected)?;
                }
                return Ok(serde_json::to_value(w)?);
            }
            (_, false, false) => {
                return Err(Error::new(
                    "recovery_blocked",
                    "retiring workspace and trash are both missing",
                ));
            }
            (_, true, false) => self.move_workspace_to_trash(&w, &trash, expected)?,
            (_, false, true) => {}
        }
        self.check_retirement_trash(&w, &trash, expected)?;
        self.release_workspace_pin(&w, op)?;
        w.lifecycle = "retired".into();
        w.generation = p.expected_generation + 1;
        self.save_workspace(&w)?;
        if trash.exists() {
            operations::checked_directory(&trash)?;
            fs::remove_dir_all(&trash)?;
            operations::sync_dir(trash.parent().unwrap())?;
        }
        Ok(serde_json::to_value(w)?)
    }
    fn begin_retirement(&self, w: &mut WorkspaceRecord, p: &Retire, op: &str) -> Result<()> {
        if !p.quiescent {
            return Err(Error::new(
                "workspace_changed",
                "caller must stop all workspace writers",
            ));
        }
        if w.lifecycle == "active" {
            self.check_retirement(w, p)?;
            w.lifecycle = "retiring".into();
            w.retiring_operation = Some(op.into());
            self.save_workspace(w)?;
        } else if w.retiring_operation.as_deref() != Some(op) {
            return Err(Error::new("workspace_changed", "workspace is not active"));
        }
        Ok(())
    }
    fn release_workspace_pin(&self, w: &WorkspaceRecord, op: &str) -> Result<()> {
        if let Some(checkpoint) = &w.last_checkpoint {
            self.root.release_dependency(checkpoint, &w.workspace_id)?;
        }
        let pin = self
            .root
            .retention(&w.workspace_id)?
            .ok_or_else(|| Error::new("retention_not_found", "workspace borrow pin missing"))?;
        self.release_handle(operations::HandleRelease {
            handle: &w.workspace_id,
            generation: pin.generation,
            operation: op,
            reason: "workspace retired",
        })?;
        Ok(())
    }
    fn move_workspace_to_trash(
        &self,
        w: &WorkspaceRecord,
        trash: &std::path::Path,
        expected: &str,
    ) -> Result<()> {
        self.workspace_integrity(w)?;
        if self.workspace_head(w)? != expected || !self.workspace_status(w)?.is_empty() {
            return Err(Error::new(
                "workspace_changed",
                "workspace changed after retirement intent",
            ));
        }
        fs::rename(&w.path, trash)?;
        operations::sync_dir(w.path.parent().unwrap())?;
        operations::sync_dir(trash.parent().unwrap())?;
        Ok(())
    }
    fn finish_retired_trash(
        &self,
        w: &WorkspaceRecord,
        trash: &std::path::Path,
        expected: &str,
    ) -> Result<()> {
        self.check_retirement_trash(w, trash, expected)?;
        fs::remove_dir_all(trash)?;
        operations::sync_dir(trash.parent().unwrap())
    }
    fn check_retirement(&self, w: &WorkspaceRecord, p: &Retire) -> Result<()> {
        let expected = p.expected_head.hex.as_str();
        let repo = &w.repository_id;
        self.workspace_integrity(w)?;
        if w.generation != p.expected_generation || self.workspace_head(w)? != expected {
            return Err(Error::new(
                "workspace_changed",
                "workspace generation or HEAD changed",
            ));
        }
        if !self.workspace_status(w)?.is_empty() {
            return Err(Error::new(
                "workspace_dirty",
                "retirement refuses dirty work",
            ));
        }
        let handle = w.last_checkpoint.as_ref().ok_or_else(|| {
            Error::new(
                "retention_not_found",
                "checkpoint required before retirement",
            )
        })?;
        let pin = self.active_handle(repo, handle)?;
        if !pin.refs.iter().any(|x| x.oid == expected) {
            return Err(Error::new(
                "workspace_changed",
                "retained checkpoint differs from HEAD",
            ));
        }
        Ok(())
    }
    fn check_retirement_trash(
        &self,
        w: &WorkspaceRecord,
        trash: &std::path::Path,
        expected: &str,
    ) -> Result<()> {
        let mut retired = w.clone();
        retired.path = trash.to_owned();
        self.workspace_integrity_at(&retired, false)?;
        if self.workspace_head(&retired)? != expected
            || !self.workspace_status(&retired)?.is_empty()
        {
            return Err(Error::new(
                "workspace_changed",
                "retirement trash differs from recorded source",
            ));
        }
        Ok(())
    }
}
