use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    binding: Binding,
    reason: String,
}
impl Binding {
    fn validate_replacement(&self, old: &Self) -> Result<()> {
        if self.repository_identity() != old.repository_identity() {
            return Err(Error::new(
                "binding_conflict",
                "Identity, mode and object format cannot change",
            ));
        }
        let advances = match old.mode.as_str() {
            "central" => {
                self.binding_generation
                    == old
                        .binding_generation
                        .checked_add(1)
                        .ok_or_else(|| Error::new("binding_conflict", "Generation exhausted"))?
            }
            "worker" => self.binding_generation > old.binding_generation,
            _ => true,
        };
        if !advances {
            return Err(Error::new(
                "binding_conflict",
                "Replacement generation must advance according to mode",
            ));
        }
        Ok(())
    }

    fn repository_identity(&self) -> (&str, &str, &str, &str) {
        (
            &self.repository_id,
            &self.mode,
            &self.identity,
            &self.object_format,
        )
    }

    pub(super) fn worker_with_remote(&self) -> bool {
        let has_remote = self.remote_url.is_some() || self.credential_profile.is_some();
        self.mode == "worker" && has_remote
    }

    pub(super) fn remote_protocol_enabled(&self, url: &str) -> bool {
        let file_url = url.starts_with('/') || url.starts_with("file://");
        let network_url = url.starts_with("https://") || url.starts_with("ssh://");
        network_url || self.allow_file_transport && file_url
    }
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
        let p = self.binding_replacement(r, &old)?;
        let new = p.binding;
        self.check_binding_dependencies(id, op)?;
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
    fn binding_replacement(&self, r: &Request, old: &Binding) -> Result<Update> {
        let p: Update = serde_json::from_value(r.params.clone())?;
        let new = &p.binding;
        if p.reason.trim().is_empty() {
            return Err(Error::new(
                "invalid_request",
                "Binding transition reason required",
            ));
        }
        new.validate_replacement(old)?;
        self.validate_profiles_and_remote(new)?;
        new.validate_ref_scopes()?;
        if new.worker_with_remote() {
            return Err(Error::new(
                "invalid_request",
                "Worker bindings cannot contain remotes or credentials",
            ));
        }
        if new
            .remote_url
            .as_ref()
            .is_some_and(|url| url.contains(['\n', '\0']) || !new.remote_protocol_enabled(url))
        {
            return Err(Error::new(
                "unsupported_feature",
                "Remote protocol is not enabled",
            ));
        }
        Ok(p)
    }

    fn binding_blockers(
        &self,
        directory: &str,
        missing: &str,
        blocker: impl Fn(&Value) -> Option<Value>,
    ) -> Result<Vec<Value>> {
        let mut blockers = Vec::new();
        for relative in self.json_files(directory)? {
            let record: Value = self
                .root
                .read_json(&relative)?
                .ok_or_else(|| Error::new("state_corrupt", missing))?;
            blockers.extend(blocker(&record));
        }
        Ok(blockers)
    }
    fn check_binding_dependencies(&self, id: &str, op: &str) -> Result<()> {
        let mut blockers =
            self.binding_blockers("retention", "Retention record disappeared", |v| {
                (v["repository_id"] == id && v["state"] != "released")
                    .then(|| json!({"kind":"retention","id":v["id"]}))
            })?;
        blockers.extend(self.binding_blockers(
            &format!("repositories/{id}/workspaces"),
            "Workspace record disappeared",
            |v| {
                (v["lifecycle"] != "retired")
                    .then(|| json!({"kind":"workspace","id":v["workspace_id"]}))
            },
        )?);
        blockers.extend(self.binding_blockers(
            "operations",
            "Operation record disappeared",
            |v| {
                let pending_for_repo =
                    v["payload"]["repository_id"] == id && v["status"] == "pending";
                (pending_for_repo && v["id"] != op)
                    .then(|| json!({"kind":"operation","id":v["id"]}))
            },
        )?);
        if !blockers.is_empty() {
            return Err(Error::new(
                "binding_in_use",
                "Live resources or unresolved operations block binding changes",
            )
            .details(json!({"blockers":blockers})));
        }
        Ok(())
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
                self.complete_recovered_operation(&intent, result)?;
            }
        }
        Ok(())
    }
    fn complete_recovered_operation(&self, intent: &Value, result: Value) -> Result<()> {
        let Some(id) = intent["operation_id"].as_str() else {
            return Ok(());
        };
        if let Some(operation) = self
            .root
            .operation(id)?
            .filter(|r| r.status == crate::state::OperationStatus::Pending)
        {
            self.root
                .complete_operation(id, operation.generation, result)?;
        }
        Ok(())
    }
}
