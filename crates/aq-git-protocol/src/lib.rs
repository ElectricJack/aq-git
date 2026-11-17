use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const SOCKET_IO_WORKERS: usize = 4;
/// Snapshot a managed workspace's working tree (tracked edits plus untracked,
/// non-ignored files) into a deterministic bundle with a sha256 digest,
/// without moving the workspace HEAD or index.
pub const WORKSPACE_SNAPSHOT: &str = "workspace.snapshot";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: String,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub expected_binding_generation: Option<u64>,
    #[serde(default = "empty_params")]
    pub params: Value,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}
fn empty_params() -> Value {
    json!({})
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryAdvice {
    SafeSameOperation,
    RefreshInputs,
    ReconcileFirst,
    Manual,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    pub details: Value,
    pub retry: RetryAdvice,
}
impl ProtocolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, details: Value) -> Self {
        let code = code.into();
        let retry = match code.as_str() {
            "overloaded" | "state_busy" | "io_error" | "state_io" | "deadline_exceeded" => {
                RetryAdvice::SafeSameOperation
            }
            "binding_conflict"
            | "target_moved"
            | "object_unavailable"
            | "observation_stale"
            | "workspace_changed"
            | "prerequisite_missing"
            | "generation_conflict" => RetryAdvice::RefreshInputs,
            "remote_auth_failed" if details["remote_effect_unknown"] == true => {
                RetryAdvice::ReconcileFirst
            }
            "operation_indeterminate" => RetryAdvice::ReconcileFirst,
            _ => RetryAdvice::Manual,
        };
        Self {
            code,
            message: message.into(),
            details,
            retry,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(
        default,
        deserialize_with = "present_result",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_status: Option<Value>,
}
fn present_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}
impl Response {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::new(
                "unsupported_protocol",
                "Unsupported response protocol version",
                json!({"supported":[PROTOCOL_VERSION],"received":self.protocol_version}),
            ));
        }
        if (self.ok && (self.result.is_none() || self.error.is_some()))
            || (!self.ok && (self.error.is_none() || self.result.is_some()))
        {
            return Err(ProtocolError::new(
                "invalid_response",
                "Response must contain exactly its success result or failure error",
                json!({}),
            ));
        }
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.code.is_empty())
            || self
                .resource_status
                .as_ref()
                .is_some_and(|value| !value.is_object())
        {
            return Err(ProtocolError::new(
                "invalid_response",
                "Malformed response metadata",
                json!({}),
            ));
        }
        if let Some(id) = &self.operation_id
            && validate_id(id).is_err()
        {
            return Err(ProtocolError::new(
                "invalid_response",
                "Malformed response operation identifier",
                json!({}),
            ));
        }
        Ok(())
    }
    pub fn success(request: &Request, result: Value) -> Self {
        Self {
            protocol_version: 1,
            request_id: request.request_id.clone(),
            ok: true,
            result: Some(result),
            error: None,
            operation_id: request.operation_id.clone(),
            operation_state: None,
            resource_status: None,
        }
    }
    pub fn error(
        request: &Request,
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        let mut r = Self::failure(&request.request_id, code, message, details);
        r.operation_id = request.operation_id.clone();
        r
    }
    pub fn failure(
        request_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            protocol_version: 1,
            request_id: request_id.into(),
            ok: false,
            result: None,
            error: Some(ProtocolError::new(code, message, details)),
            operation_id: None,
            operation_state: None,
            resource_status: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct ObjectId {
    pub algorithm: String,
    pub hex: String,
}
pub type Oid = ObjectId;
impl ObjectId {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.algorithm != "sha1" {
            return Err(ProtocolError::new(
                "unsupported_feature",
                "Only SHA-1 objects are enabled",
                json!({"algorithm":self.algorithm}),
            ));
        }
        if self.hex.len() != 40
            || !self
                .hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(invalid("Object ID must be 40 lowercase hexadecimal digits"));
        }
        Ok(())
    }
}
pub fn validate_id(id: &str) -> Result<(), ProtocolError> {
    let parsed =
        uuid::Uuid::parse_str(id).map_err(|_| invalid("Persistent identifiers must be UUIDs"))?;
    if id.len() != 36 || parsed.hyphenated().to_string() != id {
        return Err(invalid(
            "Persistent identifiers must use canonical UUID notation",
        ));
    }
    Ok(())
}
pub fn validate_ref(name: &str) -> Result<(), ProtocolError> {
    if !name.starts_with("refs/")
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("@{")
        || name.contains("//")
        || name
            .bytes()
            .any(|b| b <= 32 || b == 127 || b"~^:?*[\\".contains(&b))
        || name
            .split('/')
            .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with(".lock"))
    {
        return Err(invalid("Invalid full Git ref name"));
    }
    Ok(())
}
fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new("invalid_request", message, json!({}))
}
impl Request {
    pub fn is_mutation(&self) -> bool {
        matches!(
            self.operation.as_str(),
            "repo.register"
                | "repo.binding-update"
                | "repo.sync"
                | "repo.cache-evict"
                | "workspace.create"
                | "workspace.checkpoint"
                | "workspace.retire"
                | WORKSPACE_SNAPSHOT
                | "artifact.export"
                | "artifact.import"
                | "candidate.create"
                | "publish"
                | "operation.reconcile"
                | "operation.resolve"
                | "retention.release"
                | "recovery.clear-fence"
                | "maintenance"
        )
    }
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol_version != 1 {
            return Err(ProtocolError::new(
                "unsupported_protocol",
                "Unsupported protocol version",
                json!({"supported":[1]}),
            ));
        }
        if self.request_id.is_empty()
            || self.request_id.chars().count() > 128
            || self.request_id.chars().any(char::is_control)
        {
            return Err(invalid(
                "request_id must contain 1 to 128 non-control characters",
            ));
        }
        if !self.params.is_object() {
            return Err(invalid("params must be an object"));
        }
        if let Some(id) = &self.repository_id {
            validate_id(id)?;
        }
        if let Some(id) = &self.operation_id {
            validate_id(id)?;
        }
        if self.deadline_ms == Some(0) {
            return Err(invalid("deadline_ms must be positive"));
        }
        if self.is_mutation() && self.operation_id.is_none() {
            return Err(invalid("Mutation requires operation_id"));
        }
        if self.is_mutation()
            && !matches!(self.operation.as_str(), "recovery.clear-fence")
            && (self.repository_id.is_none() || self.expected_binding_generation.is_none())
        {
            return Err(invalid(
                "Repository mutation requires repository_id and expected_binding_generation",
            ));
        }
        if self.operation == "repo.register" && self.expected_binding_generation != Some(0) {
            return Err(invalid(
                "Registration requires expected_binding_generation zero",
            ));
        }
        Ok(())
    }
}
pub fn decode_request(bytes: &[u8]) -> Result<Request, ProtocolError> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::new(
            "limit_exceeded",
            "Request frame exceeds limit",
            json!({"max_bytes":MAX_REQUEST_BYTES}),
        ));
    }
    let request: Request = serde_json::from_slice(bytes)
        .map_err(|e| invalid(&format!("Invalid request JSON: {e}")))?;
    request.validate()?;
    Ok(request)
}
/// Decode the response wire envelope, including field presence and null results.
/// Additive response fields remain compatible; contradictory fields never do.
pub fn decode_response(bytes: &[u8]) -> Result<Response, ProtocolError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ProtocolError::new(
            "limit_exceeded",
            "Response frame exceeds limit",
            json!({"max_bytes":MAX_RESPONSE_BYTES}),
        ));
    }
    let malformed =
        || ProtocolError::new("invalid_response", "Malformed response envelope", json!({}));
    let value: Value = serde_json::from_slice(bytes).map_err(|_| malformed())?;
    if let Some(version) = value["protocol_version"].as_u64()
        && version != u64::from(PROTOCOL_VERSION)
    {
        return Err(ProtocolError::new(
            "unsupported_protocol",
            "Unsupported response protocol version",
            json!({"supported":[PROTOCOL_VERSION],"received":version}),
        ));
    }
    let object = value.as_object().ok_or_else(malformed)?;
    match value["ok"].as_bool() {
        Some(true) if object.contains_key("result") && !object.contains_key("error") => {}
        Some(false) if object.contains_key("error") && !object.contains_key("result") => {}
        _ => return Err(malformed()),
    }
    // Deserialize the original bytes as well, so duplicate known fields are
    // rejected instead of being silently overwritten by a generic JSON map.
    let response: Response = serde_json::from_slice(bytes).map_err(|_| malformed())?;
    response.validate()?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_envelope() {
        assert!(
            decode_request(
                br#"{"protocol_version":1,"request_id":"a","operation":"capabilities","extra":1}"#
            )
            .is_err()
        );
    }
    #[test]
    fn versions_and_limits() {
        assert_eq!(
            decode_request(
                br#"{"protocol_version":2,"request_id":"a","operation":"capabilities"}"#
            )
            .unwrap_err()
            .code,
            "unsupported_protocol"
        );
        assert_eq!(
            decode_request(&vec![0; MAX_REQUEST_BYTES + 1])
                .unwrap_err()
                .code,
            "limit_exceeded"
        );
    }
    #[test]
    fn identifiers() {
        assert!(validate_id("../../bad").is_err());
        assert!(validate_ref("refs/heads/main").is_ok());
        for r in ["main", "refs/a..b", "refs/a.lock", "refs/a b"] {
            assert!(validate_ref(r).is_err());
        }
    }
    #[test]
    fn full_oid() {
        assert!(
            ObjectId {
                algorithm: "sha1".into(),
                hex: "a".repeat(40)
            }
            .validate()
            .is_ok()
        );
        assert!(
            ObjectId {
                algorithm: "sha1".into(),
                hex: "a".repeat(39)
            }
            .validate()
            .is_err()
        );
    }
}
