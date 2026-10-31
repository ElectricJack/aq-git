//! A fake AQ transport over the public process/socket protocol, without AQ state.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}
struct Runtime {
    child: Child,
    root: PathBuf,
    socket: PathBuf,
    stopped: bool,
}
impl Runtime {
    fn start(root: PathBuf, socket: PathBuf) -> Self {
        Self::start_with_git(root, socket, None)
    }
    fn start_with_git(root: PathBuf, socket: PathBuf, git: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aq-git"));
        if let Some(git) = git {
            command.arg("--git-executable").arg(git);
        }
        let mut child = command
            .arg("--state-dir")
            .arg(&root)
            .arg("--listen")
            .arg(&socket)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&socket).is_err() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "runtime exited before readiness"
            );
            assert!(Instant::now() < deadline, "runtime did not become ready");
            std::thread::sleep(Duration::from_millis(5));
        }
        Self {
            child,
            root,
            socket,
            stopped: false,
        }
    }
    fn call(&self, repository: &str, operation: &str, params: Value) -> Value {
        let id = uuid::Uuid::new_v4().to_string();
        let request = json!({"protocol_version":1,"request_id":id,"operation":operation,"repository_id":repository,"operation_id":id,"expected_binding_generation":if operation=="repo.register"{0}else{1},"params":params});
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["request_id"], id);
        assert_eq!(response["ok"], true, "{response}");
        response["result"].clone()
    }
    fn stop(&mut self) {
        if !self.stopped {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            assert!(self.child.wait().unwrap().success());
            self.stopped = true;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if !self.stopped {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let _ = self.child.wait();
        }
    }
}
fn oid(id: &str) -> Value {
    json!({"algorithm":"sha1","hex":id})
}
fn transfer(from: &Runtime, to: &Runtime, artifact: &Value) {
    let handle = artifact["artifact_handle"].as_str().unwrap();
    let source = from.root.join("artifacts").join(handle);
    let incoming = to.root.join("artifacts/incoming").join(handle);
    std::fs::create_dir_all(&incoming).unwrap();
    for name in ["manifest.json", "objects.bundle"] {
        if source.join(name).exists() {
            std::fs::copy(source.join(name), incoming.join(name)).unwrap();
        }
    }
}
#[test]
fn coordinator_worker_receipt_survives_worker_loss_and_publishes() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare"]);
    let seed = temp.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("hello"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "base"]);
    let base = git(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["push", remote.to_str().unwrap(), "main"]);
    let mut central = Runtime::start(
        temp.path().join("central"),
        temp.path().join("central.sock"),
    );
    let mut worker = Runtime::start(temp.path().join("worker"), temp.path().join("worker.sock"));
    let repo = uuid::Uuid::new_v4().to_string();
    central.call(&repo,"repo.register",json!({"identity":"standalone-fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]}));
    central.call(&repo, "repo.sync", json!({}));
    // AQ provisions a retained base and transfers completed bytes outward.
    let cw = central.call(&repo, "workspace.create", json!({"base":oid(&base)}));
    let cwid = cw["workspace_id"].as_str().unwrap();
    let checkpoint=central.call(&repo,"workspace.checkpoint",json!({"workspace_id":cwid,"expected_generation":1,"expected_head":oid(&base),"quiescent":true,"mode":"head"}));
    let base_artifact=central.call(&repo,"artifact.export",json!({"base":oid(&base),"source":oid(&base),"retention_handle":checkpoint["retention_handle"],"caller_correlation":{"attempt":"base-provisioning"}}));
    central.call(&repo,"workspace.retire",json!({"workspace_id":cwid,"expected_generation":2,"expected_head":oid(&base),"quiescent":true}));
    worker.call(
        &repo,
        "repo.register",
        json!({"identity":"standalone-fixture","mode":"worker","binding_generation":1}),
    );
    transfer(&central, &worker, &base_artifact);
    let imported = worker.call(
        &repo,
        "artifact.import",
        json!({"artifact_handle":base_artifact["artifact_handle"]}),
    );
    assert!(imported["retention_handle"].is_string());
    // The worker polls, receives an assignment, edits only its granted workspace,
    // then posts its retained checkpoint and upload through outbound AQ transport.
    let workspace = worker.call(&repo, "workspace.create", json!({"base":oid(&base)}));
    let wid = workspace["workspace_id"].as_str().unwrap();
    let path = Path::new(workspace["path"].as_str().unwrap());
    std::fs::write(path.join("hello"), "worker source survives worker loss\n").unwrap();
    let source=worker.call(&repo,"workspace.checkpoint",json!({"workspace_id":wid,"expected_generation":1,"expected_head":oid(&base),"quiescent":true,"mode":"all","commit":{"name":"Worker","email":"worker@example.test","message":"retained worker source"}}));
    let source_oid = source["source"].clone();
    let source_artifact=worker.call(&repo,"artifact.export",json!({"base":oid(&base),"source":source_oid,"retention_handle":source["retention_handle"],"caller_correlation":{"attempt":"attempt-1"}}));
    transfer(&worker, &central, &source_artifact);
    let receipt = central.call(
        &repo,
        "artifact.import",
        json!({"artifact_handle":source_artifact["artifact_handle"]}),
    );
    assert!(receipt["retention_handle"].is_string());
    // A central receipt is the durability boundary. AQ attempt acceptance remains
    // separate; after receipt the worker's complete volume can disappear.
    worker.stop();
    std::fs::remove_dir_all(&worker.root).unwrap();
    assert!(!worker.root.exists());
    let candidate=central.call(&repo,"candidate.create",json!({"base":oid(&base),"sources":[{"oid":source_oid,"retention_handle":receipt["retention_handle"]}],"commit":{"name":"Coordinator","email":"coordinator@example.test","message":"approved attempt-1"}}));
    assert_eq!(candidate["state"], "ready");
    let published=central.call(&repo,"publish",json!({"candidate_id":candidate["candidate_id"],"target":"refs/heads/main","expected_old":oid(&base)}));
    assert_eq!(published["outcome"], "applied");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        source_oid["hex"].as_str().unwrap()
    );
    assert_eq!(
        git(&remote, &["show", "refs/heads/main:hello"]),
        "worker source survives worker loss"
    );
    central.stop();
}

#[test]
fn socket_publication_engine_crash_after_real_push_never_repushes_on_restart() {
    use std::{
        fs::OpenOptions,
        io::Read,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare"]);
    let seed = temp.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("a"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "base"]);
    let base = git(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["checkout", "-b", "source"]);
    std::fs::write(seed.join("a"), "source\n").unwrap();
    git(&seed, &["commit", "-am", "source"]);
    let source = git(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["push", remote.to_str().unwrap(), "main", "source"]);
    let state = temp.path().join("state");
    let socket = temp.path().join("socket");
    let repo = uuid::Uuid::new_v4().to_string();
    let ready = temp.path().join("ready");
    let blocked = temp.path().join("blocked");
    for path in [&ready, &blocked] {
        let path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    let enabled = temp.path().join("enabled");
    let count = temp.path().join("push-count");
    let wrapper = temp.path().join("gated-git");
    let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    let pool = state.join(format!("repositories/{repo}/objects.git"));
    let script = format!(
        r#"#!/bin/sh
pushing=false
for arg in "$@"; do if [ "$arg" = push ]; then pushing=true; fi; done
if [ "$pushing" = true ] && [ "$PWD" = {pool} ]; then
    printf 'push\n' >> {count}
    /usr/bin/git "$@"
    status=$?
    if [ "$status" = 0 ] && [ -f {enabled} ]; then
        printf 'ready\n' > {ready}
        IFS= read -r token < {blocked}
    fi
    exit "$status"
fi
exec /usr/bin/git "$@"
"#,
        pool = quote(&pool),
        count = quote(&count),
        enabled = quote(&enabled),
        ready = quote(&ready),
        blocked = quote(&blocked)
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut runtime = Runtime::start_with_git(state.clone(), socket.clone(), Some(&wrapper));
    runtime.call(&repo,"repo.register",json!({"identity":"crash-fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/source"],"allowed_targets":["refs/heads/main"]}));
    runtime.call(&repo, "repo.sync", json!({}));
    let workspace = runtime.call(&repo, "workspace.create", json!({"base":oid(&source)}));
    let checkpoint=runtime.call(&repo,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":oid(&source),"quiescent":true,"mode":"head"}));
    let candidate=runtime.call(&repo,"candidate.create",json!({"base":oid(&base),"sources":[{"oid":oid(&source),"retention_handle":checkpoint["retention_handle"]}],"commit":{"name":"Fixture","email":"fixture@example.test","message":"candidate"}}));
    let mut gate = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&ready)
        .unwrap();
    std::fs::write(&enabled, b"gate").unwrap();
    let operation = uuid::Uuid::new_v4().to_string();
    let publish = json!({"protocol_version":1,"request_id":"before-crash","operation":"publish","repository_id":repo,"operation_id":operation,"expected_binding_generation":1,"params":{"candidate_id":candidate["candidate_id"],"target":"refs/heads/main","expected_old":oid(&base)}});
    let sent = publish.clone();
    let destination = socket.clone();
    let caller = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(destination).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        writeln!(stream, "{sent}").unwrap();
        let mut response = String::new();
        let bytes = BufReader::new(stream).read_line(&mut response).unwrap();
        (bytes, response)
    });
    let limit = Instant::now() + Duration::from_secs(15);
    let mut notification = Vec::new();
    while !notification.contains(&b'\n') {
        let mut bytes = [0u8; 64];
        match gate.read(&mut bytes) {
            Ok(n) => notification.extend_from_slice(&bytes[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(error) => panic!("{error}"),
        };
        assert!(
            Instant::now() < limit,
            "actual push did not reach its controlled post-write gate"
        );
    }
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), source);
    let intent = runtime.call(
        &repo,
        "operation.show",
        json!({"target_operation_id":operation}),
    );
    assert_eq!(intent["status"], "pending");
    assert_eq!(intent["phase"], "dispatched");
    let children = std::fs::read_to_string(format!(
        "/proc/{}/task/{}/children",
        runtime.child.id(),
        runtime.child.id()
    ))
    .unwrap();
    let engine = children
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    assert!(!runtime.child.wait().unwrap().success());
    runtime.stopped = true;
    let (bytes, response) = caller.join().unwrap();
    assert_eq!(
        bytes, 0,
        "engine must not manufacture an acknowledgement: {response}"
    );
    std::fs::remove_file(&enabled).unwrap();
    let mut restarted = Runtime::start_with_git(state, socket.clone(), Some(&wrapper));
    let exchange = |request: &Value| {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let mut retry = publish;
    retry["request_id"] = json!("after-restart");
    let uncertain = exchange(&retry);
    assert_eq!(uncertain["request_id"], "after-restart");
    assert_eq!(uncertain["error"]["code"], "operation_indeterminate");
    assert_eq!(std::fs::read_to_string(&count).unwrap(), "push\n");
    let target = restarted.call(
        &repo,
        "operation.show",
        json!({"target_operation_id":operation}),
    );
    let result = restarted.call(
        &repo,
        "operation.reconcile",
        json!({"target_operation_id":operation,"expected_generation":target["generation"]}),
    );
    assert_eq!(result["resolution"]["outcome"], "already_present");
    assert_eq!(result["resolution"]["caused_by_this_operation"], false);
    let historical = exchange(&retry);
    assert_eq!(historical["ok"], true);
    assert_eq!(historical["result"]["outcome"], "already_present");
    assert_eq!(std::fs::read_to_string(&count).unwrap(), "push\n");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), source);
    restarted.stop();
}
