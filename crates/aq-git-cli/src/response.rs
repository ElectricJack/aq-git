use super::*;
struct BoundedOutput(Vec<u8>);
impl std::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::other("Response limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn encode_response(response: &Response) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = BoundedOutput(Vec::new());
    if serde_json::to_writer(&mut output, response).is_ok() {
        return Ok((output.0, response.ok));
    }
    let response = Response::failure(
        &response.request_id,
        "limit_exceeded",
        "Response exceeds frame limit",
        json!({}),
    );
    Ok((serde_json::to_vec(&response)?, false))
}

pub(super) fn exit_code(response: &Response) -> i32 {
    if !response.ok && response.error.is_none() {
        return 2;
    }
    let Some(error) = &response.error else {
        return if response
            .result
            .as_ref()
            .is_some_and(|r| r["outcome"] == "rejected")
        {
            3
        } else {
            0
        };
    };
    match error.code.as_str() {
        "invalid_response"
        | "invalid_request"
        | "invalid_input"
        | "invalid_id"
        | "invalid_path"
        | "invalid_artifact"
        | "artifact_invalid"
        | "unsupported_protocol"
        | "unsupported_feature"
        | "state_incompatible"
        | "unsupported_state_format" => 2,
        "operation_indeterminate" => 5,
        "overloaded" | "limit_exceeded" | "state_busy" | "deadline_exceeded"
        | "object_unavailable" | "remote_unavailable" => 4,
        "repository_not_found"
        | "workspace_not_found"
        | "candidate_not_found"
        | "operation_not_found"
        | "retention_not_found"
        | "binding_conflict"
        | "idempotency_conflict"
        | "generation_conflict"
        | "workspace_changed"
        | "workspace_dirty"
        | "target_moved"
        | "conflicted"
        | "observation_stale"
        | "prerequisite_missing"
        | "retention_in_use"
        | "binding_in_use"
        | "retention_released"
        | "recovery_blocked"
        | "incarnation_conflict"
        | "operation_terminal"
        | "retention_conflict" => 3,
        _ => 1,
    }
}
pub(super) fn print_response(
    response: &Response,
    machine: bool,
    operation: &str,
) -> std::io::Result<i32> {
    use std::io::Write;
    let (bytes, ok) = encode_response(response)?;
    let effective = if ok == response.ok {
        response.clone()
    } else {
        serde_json::from_slice(&bytes)?
    };
    let mut stdout = std::io::stdout().lock();
    if machine {
        stdout.write_all(&bytes)?;
        stdout.write_all(b"\n")?;
        return Ok(exit_code(&effective));
    }
    if let Some(error) = &effective.error {
        print_error(&mut stdout, &effective, operation, error)?;
    } else {
        print_success(&mut stdout, &effective, operation)?;
    }
    Ok(exit_code(&effective))
}
fn print_error(
    mut stdout: impl std::io::Write,
    effective: &Response,
    operation: &str,
    error: &aq_git_protocol::ProtocolError,
) -> std::io::Result<()> {
    writeln!(stdout, "{}: {} ({})", operation, error.message, error.code)?;
    if let Some(id) = &effective.operation_id {
        writeln!(stdout, "operation_id: {id}")?;
    }
    let next = match error.code.as_str() {
        "operation_indeterminate" => {
            "operation show, then operation reconcile with a new operation ID"
        }
        "state_busy" => "use --socket with the running service",
        "recovery_blocked" => "doctor; stop the old environment; audited recovery clear-fence",
        "repository_not_found"
        | "workspace_not_found"
        | "candidate_not_found"
        | "operation_not_found"
        | "retention_not_found"
        | "binding_conflict" => "repo show; refresh the expected binding generation",
        "observation_stale" => "repo sync",
        "object_unavailable" | "prerequisite_missing" => "repo sync or artifact import",
        "workspace_changed" | "workspace_dirty" => "workspace inspect",
        "retention_in_use" | "retention_released" => "retention inspect",
        _ => "inspect capabilities and correct the request",
    };
    writeln!(stdout, "next: {next}")?;
    if error.details != json!({}) && !error.details.is_null() {
        writeln!(stdout, "details: {}", error.details)?;
    }
    Ok(())
}
fn print_success(
    mut stdout: impl std::io::Write,
    effective: &Response,
    operation: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "{operation}: {}",
        effective
            .result
            .as_ref()
            .and_then(|r| r["outcome"].as_str().or_else(|| r["state"].as_str()))
            .unwrap_or("ok")
    )?;
    if let Some(result) = &effective.result {
        print_value(&mut stdout, result)?;
    }
    if let Some(id) = &effective.operation_id {
        writeln!(stdout, "operation_id: {id}")?;
    }
    Ok(())
}
fn print_value(mut stdout: impl std::io::Write, result: &serde_json::Value) -> std::io::Result<()> {
    if let Some(object) = result.as_object() {
        for (key, value) in object {
            if let Some(string) = value.as_str() {
                writeln!(stdout, "{key}: {string}")?;
            } else {
                writeln!(stdout, "{key}: {value}")?;
            }
        }
    } else {
        writeln!(stdout, "{result}")?;
    }
    Ok(())
}
