use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    binding: Binding,
    reason: String,
}
impl RepositoryManager {
    pub(crate) fn check_request_admission(&self, request: &Request) -> Result<()> {
        if let Some(repository) = request.repository_id.as_deref() {
            self.check_binding_admission(
                repository,
                if request.operation == "repo.binding-update" {
                    request.operation_id.as_deref()
                } else {
                    None
                },
            )?;
        }
        Ok(())
    }
    pub(super) fn check_binding_admission(
        &self,
        repository: &str,
        resuming: Option<&str>,
    ) -> Result<()> {
        let fences = crate::git::lock_with_deadline(&self.pending_bindings)?;
        if let Some(operation) = fences.get(repository)
            && resuming != Some(operation.as_str())
        {
            return Err(Error::new(
                "recovery_blocked",
                "an interrupted binding transition must finish before repository admission",
            )
            .details(json!({"repository_id":repository,"blocking_operation_id":operation})));
        }
        Ok(())
    }
    fn fence_binding(&self, repository: &str, operation: &str) -> Result<()> {
        let mut fences = crate::git::lock_with_deadline(&self.pending_bindings)?;
        if let Some(existing) = fences.get(repository)
            && existing != operation
        {
            return Err(Error::new(
                "recovery_blocked",
                "another binding transition owns repository admission",
            )
            .details(json!({"blocking_operation_id":existing})));
        }
        fences.insert(repository.into(), operation.into());
        Ok(())
    }
    fn clear_binding_fence(&self, repository: &str, operation: &str) -> Result<()> {
        let mut fences = crate::git::lock_with_deadline(&self.pending_bindings)?;
        if fences.get(repository).map(String::as_str) == Some(operation) {
            fences.remove(repository);
        }
        Ok(())
    }
    pub fn repository_lifecycle(&self, id: &str) -> Arc<RwLock<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(id).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(RwLock::new(()));
        gates.insert(id.into(), Arc::downgrade(&gate));
        gate
    }
    pub(super) fn update_binding(&self, r: &Request) -> Result<Value> {
        let id = r
            .repository_id
            .as_deref()
            .ok_or_else(|| Error::new("invalid_request", "repository_id required"))?;
        aq_git_protocol::validate_id(id).map_err(|e| Error::new(e.code, e.message))?;
        let op = r
            .operation_id
            .as_deref()
            .ok_or_else(|| Error::new("invalid_request", "operation_id required"))?;
        let transition = format!("repositories/{id}/binding-transitions/{op}.json");
        if let Some(intent) = self.root.read_json::<Value>(&transition)? {
            return self.finish_binding(&transition, intent);
        }
        self.check_binding_admission(id, Some(op))?;
        let old = self.binding_unfenced(id)?;
        if r.expected_binding_generation
            .is_some_and(|generation| generation != old.binding_generation)
        {
            return Err(
                Error::new("binding_conflict", "Expected current binding generation")
                    .details(json!({"binding_generation":old.binding_generation})),
            );
        }
        let p: Update = serde_json::from_value(r.params.clone())?;
        let new = p.binding;
        if p.reason.trim().is_empty() {
            return Err(Error::new(
                "invalid_request",
                "Binding transition reason required",
            ));
        }
        if new.repository_id != old.repository_id
            || new.mode != old.mode
            || new.identity != old.identity
            || new.object_format != old.object_format
        {
            return Err(Error::new(
                "binding_conflict",
                "Identity, mode and object format cannot change",
            ));
        }
        if old.mode == "central"
            && new.binding_generation
                != old
                    .binding_generation
                    .checked_add(1)
                    .ok_or_else(|| Error::new("binding_conflict", "Generation exhausted"))?
            || old.mode == "worker" && new.binding_generation <= old.binding_generation
        {
            return Err(Error::new(
                "binding_conflict",
                "Replacement generation must advance according to mode",
            ));
        }
        self.validate_profiles_and_remote(&new)?;
        new.validate_ref_scopes()?;
        if new.mode == "worker" && (new.remote_url.is_some() || new.credential_profile.is_some()) {
            return Err(Error::new(
                "invalid_request",
                "Worker bindings cannot contain remotes or credentials",
            ));
        }
        if let Some(url) = &new.remote_url
            && (url.contains(['\n', '\0'])
                || !(url.starts_with("https://")
                    || url.starts_with("ssh://")
                    || new.allow_file_transport
                        && (url.starts_with('/') || url.starts_with("file://"))))
        {
            return Err(Error::new(
                "unsupported_feature",
                "Remote protocol is not enabled",
            ));
        }
        let mut blockers = Vec::new();
        for relative in self.json_files("retention")? {
            let v: Value = self
                .root
                .read_json(&relative)?
                .ok_or_else(|| Error::new("state_corrupt", "Retention record disappeared"))?;
            if v["repository_id"] == id && v["state"] != "released" {
                blockers.push(json!({"kind":"retention","id":v["id"]}));
            }
        }
        for relative in self.json_files(&format!("repositories/{id}/workspaces"))? {
            let v: Value = self
                .root
                .read_json(&relative)?
                .ok_or_else(|| Error::new("state_corrupt", "Workspace record disappeared"))?;
            if v["lifecycle"] != "retired" {
                blockers.push(json!({"kind":"workspace","id":v["workspace_id"]}));
            }
        }
        for relative in self.json_files("operations")? {
            let v: Value = self
                .root
                .read_json(&relative)?
                .ok_or_else(|| Error::new("state_corrupt", "Operation record disappeared"))?;
            if v["id"] != op && v["payload"]["repository_id"] == id && v["status"] == "pending" {
                blockers.push(json!({"kind":"operation","id":v["id"]}));
            }
        }
        if !blockers.is_empty() {
            return Err(Error::new(
                "binding_in_use",
                "Live resources or unresolved operations block binding changes",
            )
            .details(json!({"blockers":blockers})));
        }
        let intent = json!({"format_version":1,"operation_id":op,"old":old,"new":new,"reason":p.reason,"complete":false});
        // Publish admission closure before the first potentially ambiguous durable write.
        self.fence_binding(id, op)?;
        if let Err(error) = self.root.write_json(&transition, &intent) {
            // If no transition was installed and authority is still exactly old,
            // this is a proven pre-intent failure. Otherwise retain admission closure.
            let uninstalled = matches!(self.root.read_json::<Value>(&transition), Ok(None))
                && matches!(self.root.read_json::<Value>(format!("repositories/{id}/binding.json")),Ok(Some(current)) if current==intent["old"]);
            if uninstalled {
                self.clear_binding_fence(id, op)?;
            }
            return Err(error);
        }
        self.finish_binding(&transition, intent)
    }
    fn finish_binding(&self, path: &str, mut intent: Value) -> Result<Value> {
        if intent["format_version"] != 1 {
            return Err(Error::new(
                "state_incompatible",
                "unsupported binding transition format",
            ));
        }
        let operation = intent["operation_id"]
            .as_str()
            .ok_or_else(|| Error::new("state_corrupt", "binding transition has no operation ID"))?
            .to_owned();
        aq_git_protocol::validate_id(&operation)
            .map_err(|error| Error::new(error.code, error.message))?;
        let old: Binding = serde_json::from_value(intent["old"].clone())?;
        let new: Binding = serde_json::from_value(intent["new"].clone())?;
        let pending = self
            .root
            .operation(&operation)?
            .is_some_and(|record| record.status == crate::state::OperationStatus::Pending);
        if intent["complete"] == true {
            if pending {
                self.fence_binding(&old.repository_id, &operation)?;
                self.root.write_json(path, &intent)?;
            }
            self.clear_binding_fence(&old.repository_id, &operation)?;
            return Ok(serde_json::to_value(new)?);
        }
        self.fence_binding(&old.repository_id, &operation)?;
        // Recovery checks durable authority directly, never a possibly stale pre-error cache.
        let current: Binding = self
            .root
            .read_json(format!("repositories/{}/binding.json", old.repository_id))?
            .ok_or_else(|| {
                Error::new(
                    "recovery_blocked",
                    "binding authority disappeared during transition",
                )
            })?;
        if serde_json::to_value(&current)? != serde_json::to_value(&old)?
            && serde_json::to_value(&current)? != serde_json::to_value(&new)?
        {
            return Err(Error::new(
                "recovery_blocked",
                "Binding transition generation changed unexpectedly",
            ));
        }
        if intent["complete"] != true {
            self.root.write_json(
                format!(
                    "repositories/{}/binding-history/{}.json",
                    old.repository_id, old.binding_generation
                ),
                &old,
            )?;
            self.root.write_json(
                format!("repositories/{}/binding.json", new.repository_id),
                &new,
            )?;
            self.bindings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(new.repository_id.clone(), new.clone());
            self.invalidate(&new.repository_id);
            intent["complete"] = json!(true);
            self.root.write_json(path, &intent)?;
        }
        self.clear_binding_fence(&old.repository_id, &operation)?;
        Ok(serde_json::to_value(new)?)
    }
    pub(super) fn json_files(&self, relative: &str) -> Result<Vec<String>> {
        let path = self.root.path().join(relative);
        let mut result = Vec::new();
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| Error::new("state_corrupt", "Invalid metadata filename"))?;
                result.push(format!("{relative}/{name}"));
            }
        }
        Ok(result)
    }
    pub fn invalidate_storage(&self) {
        self.clear_cache()
    }
    pub fn recover_bindings(&self) -> Result<()> {
        let repositories = self.root.path().join("repositories");
        let entries = match std::fs::read_dir(repositories) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let id = entry?
                .file_name()
                .into_string()
                .map_err(|_| Error::new("state_corrupt", "Invalid repository directory"))?;
            aq_git_protocol::validate_id(&id).map_err(|e| Error::new(e.code, e.message))?;
            let gate = self.repository_lifecycle(&id);
            let _drain = crate::git::write_with_deadline(&gate)?;
            for path in self.json_files(&format!("repositories/{id}/binding-transitions"))? {
                let intent: Value = self
                    .root
                    .read_json(&path)?
                    .ok_or_else(|| Error::new("state_corrupt", "Transition disappeared"))?;
                let result = self.finish_binding(&path, intent.clone())?;
                if let Some(id) = intent["operation_id"].as_str()
                    && let Some(operation) = self.root.operation(id)?
                    && operation.status == crate::state::OperationStatus::Pending
                {
                    self.root
                        .complete_operation(id, operation.generation, result)?;
                }
            }
        }
        Ok(())
    }
}
