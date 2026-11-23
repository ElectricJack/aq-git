use super::*;
// Called before any threads exist. The lock's open-file description is inherited
// by the engine, while this parent retains it until every descendant is reaped.
pub(super) fn guard(
    state: aq_git_core::state::StateRoot,
) -> Result<aq_git_core::state::StateRoot, Box<dyn std::error::Error>> {
    let incarnation = uuid::Uuid::new_v4().to_string();
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if let Some(previous) = state.start_incarnation(&incarnation)? {
        eprintln!(
            "recovered: previous incarnation {previous} stopped (owner lock acquired); new incarnation {incarnation}"
        );
    }
    let engine = unsafe { libc::fork() };
    if engine < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if engine == 0 {
        return Ok(state);
    }
    forward_engine_signals(engine);
    wait_for_engine(engine)?;
    GUARDED_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
    let mut status = 0;
    if unsafe { libc::waitpid(engine, &mut status, 0) } != engine {
        return Err(std::io::Error::last_os_error().into());
    }
    reap_descendants()?;
    state.mark_clean(&incarnation)?;
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    };
    std::process::exit(code);
}
fn wait_for_engine(engine: libc::pid_t) -> CliResult<()> {
    // Leave the engine PID reserved as a zombie until signal forwarding is
    // disabled; a recycled PID must never receive a forwarded runtime signal.
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                engine as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
fn reap_descendants() -> CliResult<()> {
    let term_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        terminate_descendants(term_deadline)?;
        let mut child_status = 0;
        let result = unsafe { libc::waitpid(-1, &mut child_status, libc::WNOHANG) };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            break;
        }
        if std::time::Instant::now() > cleanup_deadline {
            return Err("recovery_blocked: descendant cleanup could not be established".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}
static GUARDED_ENGINE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
extern "C" fn forward_signal(signal: libc::c_int) {
    let pid = GUARDED_ENGINE.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FenceClearance {
    expected_incarnation: String,
    reason: String,
    evidence: serde_json::Value,
    old_environment_stopped: bool,
}
pub(super) fn clear_fence(args: Args) -> Result<i32, Box<dyn std::error::Error>> {
    if args.socket.is_some() {
        return Err("recovery clear-fence requires direct --state-dir".into());
    }
    let bytes = match args.input {
        Some(path) if path.as_os_str() != "-" => read_file(path)?,
        _ => read_stdin()?,
    };
    let request = decode_request(&bytes).map_err(|e| aq_git_core::Error::new(e.code, e.message))?;
    let params = clearance_params(&request)?;
    let root = aq_git_core::state::StateRoot::open_for_recovery(
        args.state_dir.ok_or("Choose --state-dir")?,
    )?;
    let id = request.operation_id.as_deref().unwrap();
    let prior = root.operation(id)?;
    let record = root.begin_operation(
        id,
        &json!({"operation":request.operation,"params":request.params}),
    )?;
    if matches!(record.status, aq_git_core::state::OperationStatus::Complete) {
        return print_clearance(&request, record.result.unwrap(), args.json);
    }
    clear_incarnation(&root, &params, prior.is_some())?;
    let result = json!({"cleared_incarnation":params.expected_incarnation,"local_cleanup_attested":true,"remote_effects_resolved":false});
    root.complete_operation(id, record.generation, result.clone())?;
    print_clearance(&request, result, args.json)
}
fn clear_incarnation(root: &StateRoot, params: &FenceClearance, prior: bool) -> CliResult<()> {
    let current = root.active_incarnation()?;
    let marker: Option<serde_json::Value> = root.read_json("incarnation.json")?;
    let resumed_clean = prior
        && current.is_none()
        && marker.as_ref().is_some_and(|m| {
            m["id"].as_str() == Some(params.expected_incarnation.as_str()) && m["active"] == false
        });
    if current.as_deref() != Some(&params.expected_incarnation) && !resumed_clean {
        return Err(aq_git_core::Error::new(
            "incarnation_conflict",
            "Expected active incarnation does not match",
        )
        .into());
    }
    if !resumed_clean {
        root.mark_clean(&params.expected_incarnation)?;
    }
    Ok(())
}
fn clearance_params(request: &Request) -> CliResult<FenceClearance> {
    let root_request =
        request.repository_id.is_none() && request.expected_binding_generation.is_none();
    if request.operation != "recovery.clear-fence" || !root_request {
        return Err("Invalid root-level fence clearance envelope".into());
    }
    let params: FenceClearance = serde_json::from_value(request.params.clone())?;
    aq_git_protocol::validate_id(&params.expected_incarnation)
        .map_err(|e| aq_git_core::Error::new(e.code, e.message))?;
    let evidence = cleanup_evidence(&params.evidence);
    let attested = params.old_environment_stopped && !params.reason.trim().is_empty();
    if !attested || !evidence {
        return Err(
            "Clearance requires old_environment_stopped=true, a reason, and cleanup evidence"
                .into(),
        );
    }
    Ok(params)
}

fn terminate_descendants(term_deadline: std::time::Instant) -> std::io::Result<()> {
    let children =
        std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?;
    for pid in children
        .split_whitespace()
        .filter_map(|p| p.parse::<i32>().ok())
    {
        unsafe {
            libc::kill(
                pid,
                if std::time::Instant::now() < term_deadline {
                    libc::SIGTERM
                } else {
                    libc::SIGKILL
                },
            );
        }
    }
    Ok(())
}

fn print_clearance(request: &Request, result: serde_json::Value, machine: bool) -> CliResult<i32> {
    let mut response = Response::success(request, result);
    response.operation_state = Some("complete".into());
    print_response(&response, machine, "recovery.clear-fence")?;
    Ok(0)
}

fn forward_engine_signals(engine: libc::pid_t) {
    GUARDED_ENGINE.store(engine, std::sync::atomic::Ordering::SeqCst);
    unsafe {
        libc::signal(
            libc::SIGINT,
            forward_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            forward_signal as *const () as libc::sighandler_t,
        );
    }
}

fn cleanup_evidence(evidence: &serde_json::Value) -> bool {
    match evidence {
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::String(text) => !text.trim().is_empty(),
        _ => false,
    }
}
