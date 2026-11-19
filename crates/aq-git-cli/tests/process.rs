use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aq-git"))
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let limit = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < limit, "condition did not become true");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn start(root: &Path, socket: &Path) -> Child {
    let mut child = bin()
        .arg("--state-dir")
        .arg(root)
        .arg("--listen")
        .arg(socket)
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(|| {
        if child.try_wait().unwrap().is_some() {
            use std::io::Read;
            let mut diagnostic = String::new();
            child
                .stderr
                .as_mut()
                .unwrap()
                .read_to_string(&mut diagnostic)
                .unwrap();
            panic!("service exited prematurely: {diagnostic}");
        }
        UnixStream::connect(socket).is_ok()
    });
    assert_eq!(request(socket, "ready")["ok"], true);
    child
}
fn request(socket: &Path, id: &str) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        json!({"protocol_version":1,"request_id":id,"operation":"capabilities","params":{}})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
fn children(pid: u32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect()
}
#[test]
fn direct_and_socket_parity_and_clean_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let socket = temp.path().join("socket");
    let direct = bin()
        .arg("--json")
        .arg("--state-dir")
        .arg(&root)
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(
        direct.status.success(),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    let direct: Value = serde_json::from_slice(&direct.stdout).unwrap();
    let mut service = start(&root, &socket);
    let remote = request(&socket, "parity");
    assert_eq!(direct["result"], remote["result"]);
    assert_eq!(remote["request_id"], "parity");
    unsafe {
        libc::kill(service.id() as i32, libc::SIGINT);
    }
    assert!(service.wait().unwrap().success());
    let marker: Value =
        serde_json::from_slice(&std::fs::read(root.join("incarnation.json")).unwrap()).unwrap();
    assert_eq!(marker["active"], false);
}
#[test]
fn engine_death_is_reaped_before_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let socket = temp.path().join("socket");
    let mut service = start(&root, &socket);
    let engines = children(service.id());
    assert_eq!(engines.len(), 1);
    unsafe {
        libc::kill(engines[0], libc::SIGKILL);
    }
    assert!(!service.wait().unwrap().success());
    let result = bin()
        .arg("--state-dir")
        .arg(root)
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn recovery_records(root: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(root.join("operations")) else {
        return Vec::new();
    };
    entries
        .map(|e| {
            serde_json::from_slice::<Value>(&std::fs::read(e.unwrap().path()).unwrap()).unwrap()
        })
        .filter(|r| r["payload"]["operation"] == "recovery.owner-lock-clear-fence")
        .collect()
}
fn incarnation_marker(root: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("incarnation.json")).unwrap()).unwrap()
}
#[test]
fn guard_death_keeps_replacement_refused_while_engine_lives_then_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let socket = temp.path().join("socket");
    let mut service = start(&root, &socket);
    let old = incarnation_marker(&root)["id"].as_str().unwrap().to_owned();
    let engine = children(service.id())[0];
    unsafe {
        libc::kill(service.id() as i32, libc::SIGKILL);
    }
    service.wait().unwrap();
    // The surviving engine still holds the owner lock: the old incarnation is running.
    let refused = bin()
        .arg("--state-dir")
        .arg(&root)
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("state root is owned"));
    assert_eq!(incarnation_marker(&root)["id"], old);
    assert!(recovery_records(&root).is_empty());
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    let mut result = None;
    wait_until(|| {
        let output = bin()
            .arg("--state-dir")
            .arg(&root)
            .arg("capabilities")
            .output()
            .unwrap();
        if output.status.success() {
            result = Some(output);
            true
        } else {
            false
        }
    });
    assert!(
        String::from_utf8_lossy(&result.unwrap().stderr)
            .contains(&format!("previous incarnation {old} stopped"))
    );
    assert_eq!(recovery_records(&root).len(), 1);
}

#[test]
fn kill_nine_then_restart_recovers_and_serves_without_operator_step() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let socket = temp.path().join("socket");
    let mut first = start(&root, &socket);
    let old = incarnation_marker(&root)["id"].as_str().unwrap().to_owned();
    assert_eq!(incarnation_marker(&root)["active"], true);
    let engine = children(first.id())[0];
    unsafe {
        libc::kill(first.id() as i32, libc::SIGKILL);
        libc::kill(engine, libc::SIGKILL);
    }
    first.wait().unwrap();
    wait_until(|| !Path::new(&format!("/proc/{engine}")).exists());
    let mut second = start(&root, &socket);
    assert_eq!(request(&socket, "after-recovery")["ok"], true);
    let marker = incarnation_marker(&root);
    let new = marker["id"].as_str().unwrap().to_owned();
    assert_eq!(marker["active"], true);
    assert_ne!(new, old);
    let records = recovery_records(&root);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["status"], "complete");
    assert_eq!(records[0]["payload"]["params"]["cleared_incarnation"], old);
    assert_eq!(records[0]["payload"]["params"]["new_incarnation"], new);
    assert_eq!(records[0]["result"]["cleared_incarnation"], old);
    assert_eq!(records[0]["result"]["new_incarnation"], new);
    // A clean stop and restart records no further recovery.
    unsafe {
        libc::kill(second.id() as i32, libc::SIGTERM);
    }
    assert!(second.wait().unwrap().success());
    assert_eq!(incarnation_marker(&root)["active"], false);
    let mut third = start(&root, &socket);
    assert_eq!(recovery_records(&root).len(), 1);
    unsafe {
        libc::kill(third.id() as i32, libc::SIGTERM);
    }
    assert!(third.wait().unwrap().success());
}

#[test]
fn concurrent_start_on_same_state_root_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let mut first = start(&root, &temp.path().join("first.socket"));
    let old = incarnation_marker(&root);
    let second = bin()
        .arg("--state-dir")
        .arg(&root)
        .arg("--listen")
        .arg(temp.path().join("second.socket"))
        .arg("serve")
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("state root is owned"));
    assert_eq!(incarnation_marker(&root), old);
    assert!(recovery_records(&root).is_empty());
    unsafe {
        libc::kill(first.id() as i32, libc::SIGTERM);
    }
    assert!(first.wait().unwrap().success());
}

#[test]
fn explicit_fence_clearance_is_audited_and_replayable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let incarnation = uuid::Uuid::new_v4().to_string();
    let root = aq_git_core::state::StateRoot::open(&path).unwrap();
    root.mark_active(&incarnation).unwrap();
    drop(root);
    let input = temp.path().join("clear.json");
    let mut request = json!({"protocol_version":1,"request_id":"clear","operation":"recovery.clear-fence","operation_id":uuid::Uuid::new_v4().to_string(),"params":{"expected_incarnation":incarnation,"reason":"test environment stopped","evidence":{"fixture":"no descendants launched"},"old_environment_stopped":false}});
    std::fs::write(&input, serde_json::to_vec(&request).unwrap()).unwrap();
    let denied = bin()
        .arg("--state-dir")
        .arg(&path)
        .arg("--input")
        .arg(&input)
        .args(["recovery", "clear-fence"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    request["params"]["old_environment_stopped"] = json!(true);
    std::fs::write(&input, serde_json::to_vec(&request).unwrap()).unwrap();
    let execute = || {
        bin()
            .arg("--state-dir")
            .arg(&path)
            .arg("--input")
            .arg(&input)
            .args(["recovery", "clear-fence"])
            .output()
            .unwrap()
    };
    let first = execute();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let replay = execute();
    assert!(replay.status.success());
    assert_eq!(first.stdout, replay.stdout);
    assert!(aq_git_core::state::StateRoot::open(path).is_ok());
}

fn paused_git_fixture(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let wrapper = directory.join("paused-git");
    let work = directory.to_str().unwrap().replace('\'', "'\\''");
    let script = format!(
        r#"#!/bin/sh
work='{work}'
/usr/bin/mkfifo "$work/input"
exec 3<> "$work/input"
/usr/bin/git hash-object --stdin < "$work/input" &
echo $! > "$work/realgit.pid"
/usr/bin/setsid /bin/sh -c 'trap "" TERM; echo $$ > "$1/helper.pid"; /bin/sleep 120 & echo $! > "$1/leaf.pid"; wait' sh "$work" &
echo $$ > "$work/wrapper.pid"
wait
"#
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}
fn paused_register(directory: &Path) -> (Child, Vec<i32>) {
    let wrapper = paused_git_fixture(directory);
    let input = directory.join("request.json");
    std::fs::write(&input,serde_json::to_vec(&json!({"protocol_version":1,"request_id":"paused","operation":"repo.register","repository_id":uuid::Uuid::new_v4().to_string(),"operation_id":uuid::Uuid::new_v4().to_string(),"expected_binding_generation":0,"params":{"identity":"fixture","mode":"worker"}})).unwrap()).unwrap();
    let child = bin()
        .arg("--state-dir")
        .arg(directory.join("state"))
        .arg("--git-executable")
        .arg(wrapper)
        .arg("--input")
        .arg(input)
        .args(["repo", "register"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let names = ["realgit.pid", "wrapper.pid", "helper.pid", "leaf.pid"];
    wait_until(|| {
        names.iter().all(|name| {
            std::fs::read_to_string(directory.join(name))
                .is_ok_and(|s| s.trim().parse::<i32>().is_ok())
        })
    });
    let pids = names
        .iter()
        .map(|name| {
            std::fs::read_to_string(directory.join(name))
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap()
        })
        .collect();
    (child, pids)
}
#[test]
fn engine_crash_reaps_real_git_and_detached_forked_helpers_before_unlock() {
    let temp = tempfile::tempdir().unwrap();
    let (mut guard, pids) = paused_register(temp.path());
    let engine = children(guard.id())[0];
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    let competing = bin()
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .arg("capabilities")
        .output()
        .unwrap();
    if competing.status.success() {
        for pid in &pids {
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "descendant {pid} survived replacement admission"
            );
        }
    }
    assert!(!guard.wait().unwrap().success());
    for pid in pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "descendant {pid} was not reaped"
        );
    }
    assert!(
        bin()
            .arg("--state-dir")
            .arg(temp.path().join("state"))
            .arg("capabilities")
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn guard_crash_with_surviving_git_and_helpers_is_recovered_by_owner_lock() {
    // The owner lock is close-on-exec, so exec'd git and helpers do not hold it: once the
    // guard and engine are dead the lock decides, and surviving descendants are not evidence.
    let temp = tempfile::tempdir().unwrap();
    let (mut guard, pids) = paused_register(temp.path());
    let engine = children(guard.id())[0];
    unsafe {
        libc::kill(guard.id() as i32, libc::SIGKILL);
    }
    guard.wait().unwrap();
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    wait_until(|| {
        bin()
            .arg("--state-dir")
            .arg(temp.path().join("state"))
            .arg("capabilities")
            .output()
            .unwrap()
            .status
            .success()
    });
    assert_eq!(recovery_records(&temp.path().join("state")).len(), 1);
    for pid in pids {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[test]
fn another_state_root_cannot_replace_live_socket() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("socket");
    let mut first = start(&temp.path().join("first"), &socket);
    let second = bin()
        .arg("--state-dir")
        .arg(temp.path().join("second"))
        .arg("--listen")
        .arg(&socket)
        .arg("serve")
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("live or unverified socket"));
    assert_eq!(request(&socket, "still-first")["ok"], true);
    unsafe {
        libc::kill(first.id() as i32, libc::SIGTERM);
    }
    assert!(first.wait().unwrap().success());
}

#[test]
fn stale_socket_requires_exact_root_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let socket = temp.path().join("socket");
    let mut first = start(&root, &socket);
    let engine = children(first.id())[0];
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    first.wait().unwrap();
    let mut replacement = start(&root, &socket);
    assert_eq!(request(&socket, "replacement")["ok"], true);
    unsafe {
        libc::kill(replacement.id() as i32, libc::SIGTERM);
    }
    assert!(replacement.wait().unwrap().success());
    let unrelated = temp.path().join("unrelated.socket");
    let listener = std::os::unix::net::UnixListener::bind(&unrelated).unwrap();
    drop(listener);
    let refused = bin()
        .arg("--state-dir")
        .arg(&root)
        .arg("--listen")
        .arg(&unrelated)
        .arg("serve")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not recorded as owned"));
    assert!(unrelated.exists());
}

#[test]
fn thirty_two_persistent_read_clients_complete_without_overload() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("socket");
    let mut service = start(&temp.path().join("state"), &socket);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
    let clients=(0..32).map(|client|{let socket=socket.clone();let barrier=barrier.clone();std::thread::spawn(move||{let stream=UnixStream::connect(socket).unwrap();stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();let mut stream=BufReader::new(stream);barrier.wait();for item in 0..100{let id=format!("{client}-{item}");writeln!(stream.get_mut(),"{}",json!({"protocol_version":1,"request_id":id,"operation":"capabilities","params":{}})).unwrap();let mut line=String::new();stream.read_line(&mut line).unwrap();let response:Value=serde_json::from_str(&line).unwrap();assert_eq!(response["request_id"],id);assert_eq!(response["ok"],true,"{response}");}})}).collect::<Vec<_>>();
    for client in clients {
        client.join().unwrap();
    }
    unsafe {
        libc::kill(service.id() as i32, libc::SIGTERM);
    }
    assert!(service.wait().unwrap().success());
}

#[test]
fn credential_profiles_flag_reports_operator_labels() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("profiles.json");
    std::fs::write(
        &config,
        br#"{"deploy":{"kind":"ssh_deploy_key","remote_url":"ssh://git@github.com/o/r.git","private_key_path":"/vault/key","known_hosts_path":"/vault/hosts"}}"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = bin()
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .arg("--credential-profiles")
        .arg(&config)
        .args(["--json", "capabilities"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["result"]["git"]["credential_profile_labels"],
        json!(["deploy"])
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("/vault/"));
}
#[test]
fn human_and_json_output_and_read_only_fenced_doctor() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let human = bin()
        .arg("--state-dir")
        .arg(&path)
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).starts_with("capabilities: ok\n"));
    let machine = bin()
        .arg("--state-dir")
        .arg(&path)
        .args(["--json", "capabilities"])
        .output()
        .unwrap();
    assert!(machine.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&machine.stdout).unwrap()["ok"],
        true
    );
    let incarnation = uuid::Uuid::new_v4().to_string();
    let root = aq_git_core::state::StateRoot::open(&path).unwrap();
    root.mark_active(&incarnation).unwrap();
    drop(root);
    let doctor = bin()
        .arg("--state-dir")
        .arg(&path)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let response: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(response["result"]["process_incarnation"], incarnation);
    let still_active: Value =
        serde_json::from_slice(&std::fs::read(path.join("incarnation.json")).unwrap()).unwrap();
    assert_eq!(still_active["active"], true);
    // No process holds the owner lock, so the next start proves the incarnation stopped.
    let recovered = bin()
        .arg("--state-dir")
        .arg(&path)
        .args(["--json", "capabilities"])
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let records = recovery_records(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["payload"]["params"]["cleared_incarnation"],
        incarnation
    );
    assert_eq!(incarnation_marker(&path)["active"], false);
}

#[test]
fn machine_failures_preserve_wire_classifier_correlation_and_exit_category() {
    for (code, exit, details) in [
        ("binding_conflict", 3, json!({})),
        ("overloaded", 4, json!({})),
        ("operation_indeterminate", 5, json!({})),
        ("invalid_response", 2, json!("opaque details")),
        ("unsupported_protocol", 2, json!(["legacy", "details"])),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let input = temp.path().join("request.json");
        let envelope = json!({"protocol_version":1,"request_id":"error-correlation","operation":"publish","repository_id":uuid::Uuid::new_v4().to_string(),"operation_id":operation_id,"expected_binding_generation":1,"params":{"candidate_id":uuid::Uuid::new_v4().to_string(),"target":"refs/heads/main","expected_old":{"algorithm":"sha1","hex":"a".repeat(40)}}});
        std::fs::write(&input, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let original_details = details.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).unwrap();
            let request: aq_git_protocol::Request = serde_json::from_str(&line).unwrap();
            let response = if code == "binding_conflict" {
                aq_git_protocol::Response::failure(
                    &request.request_id,
                    code,
                    "Fixture refusal",
                    details,
                )
            } else {
                aq_git_protocol::Response::error(&request, code, "Fixture refusal", details)
            };
            writeln!(
                stream.get_mut(),
                "{}",
                serde_json::to_string(&response).unwrap()
            )
            .unwrap();
        });
        let output = bin()
            .arg("--socket")
            .arg(socket)
            .arg("--input")
            .arg(input)
            .args(["--json", "request"])
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(exit));
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["request_id"], "error-correlation");
        if code == "binding_conflict" {
            assert!(
                response.get("operation_id").is_none(),
                "pre-dispatch failure may omit operation metadata"
            );
        } else {
            assert_eq!(response["operation_id"], operation_id);
        }
        assert_eq!(response["error"]["code"], code);
        if code == "operation_indeterminate" {
            assert_eq!(response["error"]["retry"], "reconcile_first");
        }
        if matches!(code, "invalid_response" | "unsupported_protocol") {
            assert_eq!(
                response["error"]["details"]["server_details"],
                original_details
            );
            assert_eq!(response["error"]["details"]["operation_outcome"], "unknown");
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("invalid.json");
    std::fs::write(
        &input,
        br#"{"protocol_version":2,"request_id":"invalid","operation":"capabilities"}"#,
    )
    .unwrap();
    let invalid = bin()
        .arg("--socket")
        .arg(temp.path().join("not-contacted"))
        .arg("--input")
        .arg(input)
        .args(["--json", "request"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&invalid.stdout).unwrap()["error"]["code"],
        "unsupported_protocol"
    );
}

#[test]
fn socket_client_rejects_unsupported_and_contradictory_response_envelopes() {
    let error = json!({"code":"fixture_refusal","message":"Fixture","details":{},"retry":"manual"});
    for (mut reply, expected) in [
        (
            json!({"protocol_version":2,"ok":true,"result":{}}),
            Some("unsupported_protocol"),
        ),
        (
            json!({"protocol_version":1,"ok":false}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":true}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":true,"result":{},"error":error}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":false,"result":{},"error":error}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":true,"result":{},"error":null}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":false,"result":null,"error":error}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":true,"result":{},"operation_id":"11111111-1111-4111-8111-111111111111"}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":false,"error":null}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":true,"result":{},"resource_status":["invalid"]}),
            Some("invalid_response"),
        ),
        (
            json!({"protocol_version":1,"ok":true,"result":null,"future_response_field":true}),
            None,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let input = temp.path().join("request.json");
        std::fs::write(&input,serde_json::to_vec(&json!({"protocol_version":1,"request_id":"response-validation","operation":"capabilities","params":{}})).unwrap()).unwrap();
        reply["request_id"] = json!("response-validation");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).unwrap();
            writeln!(stream.get_mut(), "{reply}").unwrap();
        });
        let output = bin()
            .arg("--socket")
            .arg(socket)
            .arg("--input")
            .arg(input)
            .args(["--json", "request"])
            .output()
            .unwrap();
        server.join().unwrap();
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["request_id"], "response-validation");
        if let Some(code) = expected {
            assert_eq!(output.status.code(), Some(2));
            assert_eq!(response["ok"], false);
            assert_eq!(response["error"]["code"], code);
        } else {
            assert!(output.status.success());
            assert_eq!(response["ok"], true);
            assert!(response.get("result").unwrap().is_null());
        }
    }
}

// Git smart-HTTP endpoint for task attempts (Epic 0 G2); built only with the feature.
#[cfg(feature = "delivery_playbook")]
mod git_http;
