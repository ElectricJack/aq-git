use super::*;

impl RepositoryManager {
    pub(super) fn register(&self, r: &Request) -> Result<Value> {
        let id = r
            .repository_id
            .as_deref()
            .ok_or_else(|| Error::new("invalid_request", "repository_id required"))?;
        aq_git_protocol::validate_id(id).map_err(|e| Error::new(e.code, e.message))?;
        let intent = format!("repositories/{id}/registration.json");
        let previous: Option<Value> = self.root.read_json(&intent)?;
        if let Some(binding) = self.replay_registration(id, r, previous.as_ref())? {
            return Ok(binding);
        }
        let b = self.registration_binding(id, r)?;
        self.root
            .write_json(&intent, &json!({"operation_id":r.operation_id,"binding":b}))?;
        self.init_pool(id, r)?;
        self.root.write_json(
            format!("repositories/{id}/pool.json"),
            &json!({"format_version":1,"storage_generation":1,"lifecycle":"present"}),
        )?;
        self.root
            .write_json(format!("repositories/{id}/binding.json"), &b)?;
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.into(), b.clone());
        Ok(serde_json::to_value(b)?)
    }
    fn replay_registration(
        &self,
        id: &str,
        r: &Request,
        previous: Option<&Value>,
    ) -> Result<Option<Value>> {
        if self
            .root
            .read_json::<Binding>(format!("repositories/{id}/binding.json"))?
            .is_some()
        {
            if previous.is_some_and(|v| v["operation_id"] == json!(r.operation_id)) {
                return Ok(Some(serde_json::to_value(self.binding(id)?)?));
            }
            return Err(Error::new(
                "binding_conflict",
                "Repository already registered",
            ));
        }
        if previous.is_some_and(|v| v["operation_id"] != json!(r.operation_id)) {
            return Err(Error::new(
                "binding_in_use",
                "An unfinished registration owns this repository ID",
            )
            .details(json!({"blockers":[previous.unwrap()["operation_id"]]})));
        }
        if r.expected_binding_generation != Some(0) {
            return Err(Error::new(
                "binding_conflict",
                "Registration requires generation zero",
            ));
        }
        Ok(None)
    }
    fn registration_binding(&self, id: &str, r: &Request) -> Result<Binding> {
        let b = parse_binding(id, r)?;
        b.validate_ref_scopes()?;
        if b.worker_with_remote() {
            return Err(Error::new(
                "invalid_request",
                "Worker bindings cannot contain remote credentials",
            ));
        }
        self.validate_profiles_and_remote(&b)?;
        if let Some(url) = &b.remote_url {
            check_registration_transport(&b, url)?;
        }
        Ok(b)
    }
    fn init_pool(&self, id: &str, r: &Request) -> Result<()> {
        let pool = self
            .root
            .create_dir(format!("repositories/{id}/objects.git"))?;
        let o = self.runner.run(
            &pool,
            &[
                "init".into(),
                "--bare".into(),
                "--object-format=sha1".into(),
                ".".into(),
            ],
        )?;
        if !o.status.success() {
            return Err(Error::new("git_failed", "Cannot initialize pool"));
        }
        crate::git::capabilities::probe(
            &self.root,
            &self.runner,
            r.operation_id
                .as_deref()
                .ok_or_else(|| Error::new("invalid_request", "operation_id required"))?,
        )?;
        Ok(())
    }
    pub(super) fn validate_profiles_and_remote(&self, binding: &Binding) -> Result<()> {
        validate_feature_profile(&binding.feature_profile)?;
        if let Some(url) = &binding.remote_url {
            validate_remote_url(url)?;
        }
        if binding.credential_profile.is_some() {
            crate::git::credentials::RemoteAuth::load(
                &self.root,
                self.credential_profiles.as_deref(),
                binding,
            )?;
        }
        Ok(())
    }
}
/// Validate fields that must never be recorded in a mutation intent before rejection.
/// Services call this after checking historical replay and before creating a journal record.
pub fn validate_sensitive_input(request: &Request) -> Result<()> {
    if !["repo.register", "repo.binding-update"].contains(&request.operation.as_str()) {
        return Ok(());
    }
    let value = request.params.get("binding").unwrap_or(&request.params);
    if !value.is_object() {
        return Err(Error::new("invalid_request", "Binding must be an object"));
    }
    if let Some(profile) = value.get("credential_profile") {
        crate::git::credentials::validate_name(profile)?;
    }
    validate_feature_profile(&value["feature_profile"])?;
    if let Some(url) = value.get("remote_url").filter(|v| !v.is_null()) {
        let url = url
            .as_str()
            .ok_or_else(|| Error::new("invalid_request", "Remote URL must be a string"))?;
        validate_remote_url(url)?;
    }
    Ok(())
}
fn validate_feature_profile(profile: &Value) -> Result<()> {
    if !profile.is_null() && profile != &json!("no_attributes_v1") {
        return Err(Error::new(
            "unsupported_feature",
            "Feature profile is not enabled",
        ));
    }
    Ok(())
}
fn validate_remote_url(url: &str) -> Result<()> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(Error::new(
            "invalid_request",
            "Remote URL contains invalid characters",
        ));
    }
    if url
        .strip_prefix("https://")
        .is_some_and(|s| s.split(['/', '?', '#']).next().unwrap_or("").contains('@'))
    {
        return Err(Error::new(
            "invalid_request",
            "Embedded HTTPS credentials are not allowed",
        ));
    }
    if url.strip_prefix("ssh://").is_some_and(|s| {
        s.split('/')
            .next()
            .unwrap_or("")
            .split_once('@')
            .is_some_and(|(user, _)| user.contains(':'))
    }) {
        return Err(Error::new(
            "invalid_request",
            "Embedded SSH passwords are not allowed",
        ));
    }
    Ok(())
}

fn parse_binding(id: &str, r: &Request) -> Result<Binding> {
    let p = r.params.get("binding").unwrap_or(&r.params);
    let mut v = p.clone();
    v["repository_id"] = json!(id);
    if v.get("binding_generation").is_none() {
        v["binding_generation"] = json!(1)
    }
    if v.get("object_format").is_none() {
        v["object_format"] = json!("sha1")
    }
    let b: Binding = serde_json::from_value(v)?;
    validate_initial_binding(&b)?;
    Ok(b)
}

fn check_registration_transport(b: &Binding, url: &str) -> Result<()> {
    if url.starts_with('-') || url.contains(['\n', '\0']) {
        return Err(Error::new("invalid_request", "Invalid remote"));
    }
    if !b.remote_protocol_enabled(url) {
        return Err(Error::new(
            "unsupported_feature",
            "Remote protocol is not enabled",
        ));
    }
    Ok(())
}

fn validate_initial_binding(b: &Binding) -> Result<()> {
    let valid_generation = match b.mode.as_str() {
        "central" => b.binding_generation == 1,
        "worker" => b.binding_generation != 0,
        _ => false,
    };
    if !valid_generation || b.identity.is_empty() {
        return Err(Error::new(
            "invalid_request",
            "Invalid binding mode, identity or generation",
        ));
    }
    if b.object_format != "sha1" {
        return Err(Error::new(
            "unsupported_feature",
            "Only complete SHA-1 repositories are supported",
        ));
    }
    Ok(())
}
