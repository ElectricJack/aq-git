//! End to end: `aq-git serve --git-http-socket`, a real Git client through a TCP
//! forwarder to the Unix socket, with the grant in `http.extraHeader`.
use super::{bin, wait_until};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::{Shutdown, TcpListener},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=Worker",
            "-c",
            "user.email=worker@example.test",
        ])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00")
        .output()
        .unwrap()
}
fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_in(dir, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}

/// Forwards `127.0.0.1:<port>` to a Unix socket: what aq-server's forwarder does.
fn forward(socket: PathBuf) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { continue };
            let Ok(server) = UnixStream::connect(&socket) else {
                continue;
            };
            let (mut client_in, mut server_out) =
                (client.try_clone().unwrap(), server.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut client_in, &mut server_out);
                let _ = server_out.shutdown(Shutdown::Write);
            });
            let (mut server_in, mut client_out) = (server, client);
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut server_in, &mut client_out);
                let _ = client_out.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

#[derive(Clone)]
struct Grant {
    task_ref: String,
    lease: Option<String>,
    max_push_bytes: u64,
    operation: String,
}
struct Fixture {
    temp: tempfile::TempDir,
    service: Child,
    json_socket: PathBuf,
    http_socket: PathBuf,
    root: PathBuf,
    port: u16,
    repository: String,
    base: String,
    pool: PathBuf,
    source: PathBuf,
    remote: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        stop_service(&mut self.service);
    }
}
fn rpc(socket: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
/// Starts `aq-git serve` with both sockets and waits until it answers.
fn spawn_service(root: &Path, json_socket: &Path, http_socket: &Path) -> Child {
    let mut service = bin()
        .arg("--state-dir")
        .arg(root)
        .arg("--listen")
        .arg(json_socket)
        .arg("--git-http-socket")
        .arg(http_socket)
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(|| {
        if service.try_wait().unwrap().is_some() {
            use std::io::Read;
            let mut diagnostic = String::new();
            service
                .stderr
                .as_mut()
                .unwrap()
                .read_to_string(&mut diagnostic)
                .unwrap();
            panic!("service exited prematurely: {diagnostic}");
        }
        UnixStream::connect(json_socket).is_ok() && UnixStream::connect(http_socket).is_ok()
    });
    service
}
fn stop_service(service: &mut Child) {
    unsafe {
        libc::kill(service.id() as i32, libc::SIGINT);
    }
    let _ = service.wait();
}

impl Fixture {
    fn new() -> Self {
        Self::with_limits(json!({}))
    }
    /// `settings` is written to `git_http_limits.json` before the service starts.
    fn with_limits(settings: Value) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("file.txt"), "base\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        let remote = temp.path().join("remote.git");
        git(
            temp.path(),
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        let root = temp.path().join("state");
        let json_socket = temp.path().join("json.sock");
        let http_socket = temp.path().join("http.sock");
        let service = spawn_service(&root, &json_socket, &http_socket);
        // Small quota settings so the free-disk reserve check passes anywhere.
        std::fs::write(
            root.join("artifact_limits.json"),
            json!({"max_bundle_bytes":10_000_000,"max_expanded_bytes":50_000_000,"disk_reserve_bytes":1_000_000}).to_string(),
        )
        .unwrap();
        let repository = new_id();
        let registered = rpc(
            &json_socket,
            json!({"protocol_version":1,"request_id":"register","operation":"repo.register",
                "repository_id":repository,"operation_id":new_id(),"expected_binding_generation":0,
                "params":{"binding":{"identity":"fixture","mode":"central","allowed_refs":["refs/heads/main"],
                    "allowed_targets":["refs/heads/main"],"remote_url":remote,"allow_file_transport":true}}}),
        );
        assert_eq!(registered["ok"], true, "{registered}");
        let synced = rpc(
            &json_socket,
            json!({"protocol_version":1,"request_id":"sync","operation":"repo.sync",
                "repository_id":repository,"operation_id":new_id(),"expected_binding_generation":1,
                "params":{"refs":["refs/heads/main"]}}),
        );
        assert_eq!(synced["ok"], true, "{synced}");
        let pool = root.join(format!("repositories/{repository}/objects.git"));
        let port = forward(http_socket.clone());
        let fixture = Self {
            temp,
            service,
            json_socket,
            http_socket,
            root,
            port,
            repository,
            base,
            pool,
            source,
            remote,
        };
        // The documented way main reaches the endpoint: repo.retain_ref.
        let retained = fixture.retain_main();
        assert_eq!(retained["ok"], true, "{retained}");
        if settings != json!({}) {
            // Settings are read when the endpoint starts.
            let mut fixture = fixture;
            std::fs::write(
                fixture.root.join("git_http_limits.json"),
                settings.to_string(),
            )
            .unwrap();
            fixture.restart();
            return fixture;
        }
        fixture
    }
    /// `repo.retain_ref` for main: syncs it from the remote and retains its head.
    fn retain_main(&self) -> Value {
        rpc(
            &self.json_socket,
            json!({"protocol_version":1,"request_id":"retain","operation":"repo.retain_ref",
                "repository_id":self.repository,"operation_id":new_id(),"expected_binding_generation":1,
                "params":{"ref":"refs/heads/main"}}),
        )
    }
    /// A new commit on the remote's main (what a merged pull request does).
    fn advance_remote_main(&self, file: &str) -> String {
        let head = self.commit(&self.source, file, "advanced\n");
        git(
            &self.source,
            &["push", "-q", self.remote.to_str().unwrap(), "main"],
        );
        head
    }
    fn restart(&mut self) {
        stop_service(&mut self.service);
        self.service = spawn_service(&self.root, &self.json_socket, &self.http_socket);
    }
    fn grant(&self, operation: &str) -> Grant {
        Grant {
            task_ref: "refs/heads/aq/task-1".into(),
            lease: None,
            max_push_bytes: 10_000_000,
            operation: operation.into(),
        }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.repository)
    }
    /// Runs `git <args>` with the grant's headers, from `dir`.
    fn client(&self, dir: &Path, grant: &Grant, args: &[&str]) -> std::process::Output {
        let mut headers = vec![
            format!("AQ-Repository: {}", self.repository),
            format!("AQ-Base: {}", self.base),
            format!("AQ-Task-Ref: {}", grant.task_ref),
            "AQ-Main-Ref: refs/heads/main".to_string(),
            format!("AQ-Max-Push-Bytes: {}", grant.max_push_bytes),
            format!("AQ-Operation: {}", grant.operation),
        ];
        if let Some(lease) = &grant.lease {
            headers.push(format!("AQ-Task-Ref-Lease: {lease}"));
        }
        let configs: Vec<String> = headers
            .into_iter()
            .flat_map(|h| ["-c".to_string(), format!("http.extraHeader={h}")])
            .collect();
        let configs: Vec<&str> = configs.iter().map(String::as_str).collect();
        let all: Vec<&str> = configs.into_iter().chain(args.iter().copied()).collect();
        git_in(dir, &all)
    }
    fn ok(&self, dir: &Path, grant: &Grant, args: &[&str]) -> String {
        let output = self.client(dir, grant, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().into()
    }
    /// A failed Git command's combined output; asserts it failed.
    fn rejected(&self, dir: &Path, grant: &Grant, args: &[&str]) -> String {
        let output = self.client(dir, grant, args);
        assert!(
            !output.status.success(),
            "git {args:?} unexpectedly succeeded"
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
    fn clone_main(&self, name: &str, grant: &Grant) -> PathBuf {
        let dir = self.temp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let url = self.url();
        self.ok(&dir, grant, &["clone", "-q", "-b", "main", &url, "."]);
        dir
    }
    fn commit(&self, dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", file]);
        git(dir, &["rev-parse", "HEAD"])
    }
    fn pool_ref(&self, name: &str) -> Option<String> {
        let output = git_in(&self.pool, &["rev-parse", "--verify", "--quiet", name]);
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
    fn receipt(&self, operation: &str) -> Value {
        rpc(
            &self.json_socket,
            json!({"protocol_version":1,"request_id":"receipt","operation":"push.receipt",
                "operation_id":null,"params":{"operation_id":operation}}),
        )
    }
}

#[test]
fn push_to_task_ref_is_pinned_and_receipted_and_fetch_sees_only_granted_refs() {
    let f = Fixture::new();
    let grant = f.grant(&new_id());
    // Refs the grant does not cover exist in the pool and must stay invisible.
    git(&f.pool, &["update-ref", "refs/heads/secret", &f.base]);
    git(&f.pool, &["update-ref", "refs/heads/aq/other", &f.base]);
    git(&f.pool, &["update-ref", "refs/tags/v1", &f.base]);
    let scratch = f.temp.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    git(&scratch, &["init", "-q"]);
    let url = f.url();
    for version in ["0", "2"] {
        let listing = f.ok(
            &scratch,
            &grant,
            &[
                "-c",
                &format!("protocol.version={version}"),
                "ls-remote",
                &url,
            ],
        );
        let refs: Vec<&str> = listing
            .lines()
            .map(|l| l.split('\t').nth(1).unwrap())
            .collect();
        assert_eq!(refs, ["refs/heads/main"], "protocol v{version}: {listing}");
    }
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "result.txt", "work\n");
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(
        f.pool_ref("refs/heads/aq/task-1").as_deref(),
        Some(head.as_str())
    );

    let response = f.receipt(&grant.operation);
    assert_eq!(response["ok"], true, "{response}");
    let receipt = &response["result"];
    assert_eq!(receipt["repository"], f.repository.as_str());
    assert_eq!(receipt["ref"], "refs/heads/aq/task-1");
    assert_eq!(receipt["old"], Value::Null);
    assert_eq!(receipt["new"], head.as_str());
    assert_eq!(receipt["base"], f.base.as_str());
    assert_eq!(receipt["operation_id"], grant.operation.as_str());
    assert_eq!(receipt["retention_state"], "active");
    let handle = receipt["retention_handle"].as_str().unwrap();
    assert_eq!(response["resource_status"]["handle_id"], handle);
    assert_eq!(response["resource_status"]["state"], "active");
    assert_eq!(
        f.pool_ref(&format!("refs/aq-git/pushes/{handle}"))
            .as_deref(),
        Some(head.as_str())
    );

    // The advertisement now holds exactly the main ref and the task ref.
    let listing = f.ok(&scratch, &grant, &["ls-remote", &url]);
    let refs: Vec<&str> = listing
        .lines()
        .map(|l| l.split('\t').nth(1).unwrap())
        .collect();
    assert_eq!(
        refs,
        ["refs/heads/aq/task-1", "refs/heads/main"],
        "{listing}"
    );
    // And the pushed work can be fetched back, by a fresh clone of the task ref.
    let again = f.temp.path().join("again");
    std::fs::create_dir(&again).unwrap();
    f.ok(
        &again,
        &grant,
        &["clone", "-q", "-b", "aq/task-1", &url, "."],
    );
    assert_eq!(git(&again, &["rev-parse", "HEAD"]), head);

    // Updating under the right lease works and records the old value.
    let mut next = f.grant(&new_id());
    next.lease = Some(head.clone());
    let second = f.commit(&work, "more.txt", "more\n");
    f.ok(
        &work,
        &next,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(
        f.pool_ref("refs/heads/aq/task-1").as_deref(),
        Some(second.as_str())
    );
    let receipt = f.receipt(&next.operation);
    assert_eq!(receipt["result"]["old"], head.as_str());
    assert_eq!(receipt["result"]["new"], second.as_str());
    assert_eq!(
        f.receipt(&new_id())["error"]["code"],
        "push_receipt_not_found"
    );
}

#[test]
fn push_is_rejected_unless_it_is_exactly_the_granted_update() {
    let f = Fixture::new();
    let grant = f.grant(&new_id());
    let work = f.clone_main("work", &grant);
    f.commit(&work, "result.txt", "work\n");
    let nothing_moved = |f: &Fixture| {
        assert_eq!(f.pool_ref("refs/heads/aq/task-1"), None);
        assert_eq!(f.pool_ref("refs/heads/aq/task-2"), None);
        // main is served from the retained head; no branch of that name is ever created.
        assert_eq!(f.pool_ref("refs/heads/main"), None);
    };

    let message = f.rejected(
        &work,
        &grant,
        &[
            "push",
            "origin",
            "HEAD:refs/heads/aq/task-1",
            "HEAD:refs/heads/aq/task-2",
        ],
    );
    assert!(message.contains("exactly one ref"), "{message}");
    nothing_moved(&f);

    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-2"],
    );
    assert!(message.contains("not the granted task ref"), "{message}");
    let message = f.rejected(&work, &grant, &["push", "origin", "HEAD:refs/heads/main"]);
    assert!(
        message.contains("not the granted task ref") || message.contains("hidden"),
        "{message}"
    );
    nothing_moved(&f);

    let mut wrong = grant.clone();
    wrong.lease = Some("a".repeat(40));
    let message = f.rejected(
        &work,
        &wrong,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(message.contains("wrong lease"), "{message}");
    nothing_moved(&f);
    assert_eq!(
        f.receipt(&grant.operation)["error"]["code"],
        "push_receipt_not_found"
    );

    // Deleting the task ref is not an update the grant allows either.
    let head = git(&work, &["rev-parse", "HEAD"]);
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let mut created = f.grant(&new_id());
    created.lease = Some(head);
    let message = f.rejected(
        &work,
        &created,
        &["push", "origin", ":refs/heads/aq/task-1"],
    );
    assert!(
        message.contains("deleting")
            || message.contains("denyDeletes")
            || message.contains("declined"),
        "{message}"
    );
    // A create-grant (no lease) cannot overwrite the now existing ref.
    let again = f.grant(&new_id());
    f.commit(&work, "newer.txt", "newer\n");
    let message = f.rejected(
        &work,
        &again,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(message.contains("wrong lease"), "{message}");
}

#[test]
fn push_must_descend_from_the_base_and_pass_fsck_and_feature_profile() {
    let f = Fixture::new();
    let grant = f.grant(&new_id());
    let work = f.clone_main("work", &grant);

    // A root commit unrelated to the base.
    git(&work, &["checkout", "-q", "--orphan", "unrelated"]);
    git(&work, &["rm", "-rfq", "."]);
    f.commit(&work, "alien.txt", "alien\n");
    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(message.contains("does not descend from base"), "{message}");
    // A merge that brings the unrelated history in is rejected as well.
    git(&work, &["checkout", "-q", "main"]);
    git(
        &work,
        &[
            "merge",
            "-q",
            "--allow-unrelated-histories",
            "-m",
            "merge",
            "unrelated",
        ],
    );
    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(message.contains("does not descend from base"), "{message}");
    assert_eq!(f.pool_ref("refs/heads/aq/task-1"), None);

    // A commit object that git fsck --strict rejects (author without an email).
    git(&work, &["reset", "-q", "--hard", &f.base]);
    let tree = git(&work, &["rev-parse", "HEAD^{tree}"]);
    let body = format!(
        "tree {tree}\nparent {}\nauthor Broken Author 1 +0000\ncommitter Broken <b@example.test> 1 +0000\n\nbroken\n",
        f.base
    );
    let path = work.join("broken-commit");
    std::fs::write(&path, body).unwrap();
    let corrupt = git(
        &work,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "broken-commit",
        ],
    );
    git(&work, &["update-ref", "refs/heads/corrupt", &corrupt]);
    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "refs/heads/corrupt:refs/heads/aq/task-1"],
    );
    assert!(
        message.contains("fsck") || message.contains("missingEmail") || message.contains("unpack"),
        "{message}"
    );
    assert_eq!(f.pool_ref("refs/heads/aq/task-1"), None);

    // The repository's feature profile: submodules are unsupported.
    git(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},sub", f.base),
        ],
    );
    git(&work, &["commit", "-q", "-m", "submodule"]);
    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(
        message.contains("submodules are not supported"),
        "{message}"
    );
    assert_eq!(f.pool_ref("refs/heads/aq/task-1"), None);
}

#[test]
fn oversize_push_is_rejected_and_other_requests_still_work() {
    let f = Fixture::new();
    let mut grant = f.grant(&new_id());
    grant.max_push_bytes = 4096;
    let work = f.clone_main("work", &grant);
    let noise: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(work.join("blob.bin"), noise).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "big"]);
    f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(f.pool_ref("refs/heads/aq/task-1"), None);
    assert_eq!(
        f.receipt(&grant.operation)["error"]["code"],
        "push_receipt_not_found"
    );
    // Chunked bodies (above git's post buffer) are held to the same limit.
    let big = f.temp.path().join("big");
    std::fs::create_dir(&big).unwrap();
    f.ok(&big, &grant, &["clone", "-q", "-b", "main", &f.url(), "."]);
    let noise: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect();
    std::fs::write(big.join("blob.bin"), noise).unwrap();
    git(&big, &["add", "."]);
    git(&big, &["commit", "-q", "-m", "huge"]);
    f.rejected(
        &big,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(f.pool_ref("refs/heads/aq/task-1"), None);
    // The same push with a sufficient grant is accepted.
    let mut roomy = f.grant(&new_id());
    roomy.max_push_bytes = 50_000_000;
    f.ok(
        &big,
        &roomy,
        &[
            "-c",
            "http.postBuffer=65536",
            "push",
            "-q",
            "origin",
            "HEAD:refs/heads/aq/task-1",
        ],
    );
    assert_eq!(f.receipt(&roomy.operation)["ok"], true);
}

#[test]
fn malformed_grants_and_foreign_repositories_get_http_errors() {
    let f = Fixture::new();
    let socket = f.temp.path().join("http.sock");
    let send = |request: &str| -> String {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        let _ = std::io::Read::read_to_string(&mut stream, &mut reply);
        reply.lines().next().unwrap_or("").to_string()
    };
    let headers = |repository: &str, extra: &str| {
        format!(
            "GET /{repository}/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: x\r\nAQ-Repository: {repository}\r\nAQ-Base: {}\r\nAQ-Task-Ref: refs/heads/aq/t\r\nAQ-Main-Ref: refs/heads/main\r\nAQ-Max-Push-Bytes: 100\r\nAQ-Operation: {}\r\n{extra}\r\n",
            f.base,
            new_id()
        )
    };
    assert!(send(&headers(&f.repository, "")).contains(" 200 "));
    assert!(send(&headers(&f.repository, "AQ-Base: abc\r\n")).contains(" 400 "));
    assert!(send(&headers(&new_id(), "")).contains(" 404 "));
    assert!(send("GET /x/info/refs?service=git-upload-pack HTTP/1.1\r\n\r\n").contains(" 400 "));
    let path_mismatch = headers(&f.repository, "").replacen(
        &format!("GET /{}", f.repository),
        &format!("GET /{}", new_id()),
        1,
    );
    assert!(send(&path_mismatch).contains(" 400 "));
    let _ = ZERO;
}

// ---- Hardening (Epic 0 G2b): raw requests, isolation, receipts, deadlines ----

fn git_stdin(dir: &Path, args: &[&str], input: &str) -> Vec<u8> {
    let output = git_stdin_output(dir, args, input.as_bytes());
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn git_stdin_output(dir: &Path, args: &[&str], input: &[u8]) -> std::process::Output {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00")
        .env("GIT_AUTHOR_NAME", "Worker")
        .env("GIT_AUTHOR_EMAIL", "worker@example.test")
        .env("GIT_COMMITTER_NAME", "Worker")
        .env("GIT_COMMITTER_EMAIL", "worker@example.test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}
fn pkt(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}
/// The body of a one-ref `git-receive-pack` request.
fn push_body(old: &str, new: &str, reference: &str, caps: &str, pack: &[u8]) -> Vec<u8> {
    let mut body = pkt(format!("{old} {new} {reference}\0{caps}").as_bytes());
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}
/// Everything reachable in `dir` from `new` but not from the `not` commits, as a pack.
fn pack_revs(dir: &Path, new: &str, not: &[&str], thin: bool) -> Vec<u8> {
    let mut input = format!("{new}\n");
    for n in not {
        input.push_str(&format!("^{n}\n"));
    }
    let mut args = vec!["pack-objects", "--revs", "--stdout", "-q"];
    if thin {
        args.push("--thin");
    }
    git_stdin(dir, &args, &input)
}
/// Exactly the listed objects as a pack (no traversal, no connectivity).
fn pack_list(dir: &Path, objects: &[&str]) -> Vec<u8> {
    git_stdin(
        dir,
        &["pack-objects", "--stdout", "-q"],
        &objects.join("\n"),
    )
}

impl Fixture {
    fn grant_headers(&self, grant: &Grant, extra: &str) -> String {
        let mut headers = format!(
            "AQ-Repository: {}\r\nAQ-Base: {}\r\nAQ-Task-Ref: {}\r\nAQ-Main-Ref: refs/heads/main\r\nAQ-Max-Push-Bytes: {}\r\nAQ-Operation: {}\r\n",
            self.repository, self.base, grant.task_ref, grant.max_push_bytes, grant.operation
        );
        if let Some(lease) = &grant.lease {
            headers.push_str(&format!("AQ-Task-Ref-Lease: {lease}\r\n"));
        }
        headers.push_str(extra);
        headers
    }
    /// One raw HTTP exchange with the endpoint socket: `(status, body)`.
    fn raw(
        &self,
        method: &str,
        route: &str,
        grant: &Grant,
        content_type: &str,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        use std::io::Read as _;
        let mut stream = UnixStream::connect(&self.http_socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let head = format!(
            "{method} /{}/{route} HTTP/1.1\r\nHost: x\r\n{}Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            self.repository,
            self.grant_headers(grant, ""),
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        parse_reply(&reply)
    }
    fn raw_push(&self, grant: &Grant, body: &[u8]) -> (u16, Vec<u8>) {
        self.raw(
            "POST",
            "git-receive-pack",
            grant,
            "application/x-git-receive-pack-request",
            body,
        )
    }
    fn advertisement(&self, grant: &Grant, service: &str) -> (u16, String) {
        let (status, body) = self.raw(
            "GET",
            &format!("info/refs?service=git-{service}"),
            grant,
            "text/plain",
            b"",
        );
        (status, String::from_utf8_lossy(&body).into_owned())
    }
    fn pool_objects(&self) -> std::collections::BTreeSet<String> {
        git(
            &self.pool,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }
    /// Nothing is left in Git's quarantine, the staging area or the scratch views.
    fn assert_no_leftovers(&self) {
        let quarantines: Vec<_> = std::fs::read_dir(self.pool.join("objects"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("incoming-"))
            .collect();
        assert!(quarantines.is_empty(), "quarantines left: {quarantines:?}");
        for dir in ["staging", "views"] {
            let left = std::fs::read_dir(self.root.join("git-http").join(dir))
                .unwrap()
                .count();
            assert_eq!(left, 0, "git-http/{dir} is not empty");
        }
    }
    fn metrics(&self) -> Value {
        rpc(
            &self.json_socket,
            json!({"protocol_version":1,"request_id":"metrics","operation":"service.metrics","operation_id":null,"params":{}}),
        )["result"]
            .clone()
    }
}
fn parse_reply(reply: &[u8]) -> (u16, Vec<u8>) {
    let text = String::from_utf8_lossy(&reply[..reply.len().min(64)]).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let start = reply
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(reply.len(), |i| i + 4);
    (status, reply[start..].to_vec())
}
const TASK: &str = "refs/heads/aq/task-1";

fn varint(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}
/// A pack entry header: object type and the size of the (inflated) entry data.
fn pack_header(kind: u8, size: usize) -> Vec<u8> {
    let mut out = vec![(kind << 4) | (size & 0xf) as u8];
    let mut rest = size >> 4;
    while rest > 0 {
        *out.last_mut().unwrap() |= 0x80;
        out.push((rest & 0x7f) as u8);
        rest >>= 7;
    }
    out
}
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    assert!(data.len() < 65535);
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    let mut out = vec![0x78, 0x01, 0x01];
    out.extend((data.len() as u16).to_le_bytes());
    out.extend((!(data.len() as u16)).to_le_bytes());
    out.extend(data);
    out.extend(((b << 16) | a).to_be_bytes());
    out
}
fn pack_entry(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut out = pack_header(kind, data.len());
    out.extend(zlib_stored(data));
    out
}
fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}
fn crafted_pack(entries: &[Vec<u8>]) -> Vec<u8> {
    use std::io::Write as _;
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend((entries.len() as u32).to_be_bytes());
    for entry in entries {
        pack.extend(entry);
    }
    let mut child = Command::new("sha1sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&pack).unwrap();
    let digest = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    pack.extend(hex_bytes(&digest[..40]));
    pack
}

/// A secret in the pool that no grant covers: a commit on another task's ref.
fn plant_secret(f: &Fixture) -> (String, String, String) {
    let content = (0..200)
        .map(|i| format!("secret line {i}\n"))
        .collect::<String>();
    let blob = String::from_utf8(git_stdin(
        &f.pool,
        &["hash-object", "-w", "--stdin"],
        &content,
    ))
    .unwrap()
    .trim()
    .to_owned();
    let tree = String::from_utf8(git_stdin(
        &f.pool,
        &["mktree"],
        &format!("100644 blob {blob}\tsecret.txt\n"),
    ))
    .unwrap()
    .trim()
    .to_owned();
    let commit = git(
        &f.pool,
        &["commit-tree", &tree, "-p", &f.base, "-m", "secret"],
    );
    git(&f.pool, &["update-ref", "refs/heads/aq/other", &commit]);
    (blob, commit, content)
}

#[test]
fn fetch_and_push_cannot_reach_other_tasks_objects() {
    let f = Fixture::new();
    let grant = f.grant(&new_id());
    let (secret_blob, secret_commit, secret_text) = plant_secret(&f);
    // A neighbour of the granted (absent) ref: a negated hideRefs rule would have shown it.
    git(
        &f.pool,
        &["update-ref", "refs/heads/aq/task-1/child", &f.base],
    );
    git(&f.pool, &["update-ref", "refs/heads/aq/task-10", &f.base]);
    let neighbours_gone =
        |f: &Fixture| git(&f.pool, &["update-ref", "-d", "refs/heads/aq/task-1/child"]);

    for service in ["upload-pack", "receive-pack"] {
        let (status, advertised) = f.advertisement(&grant, service);
        assert_eq!(status, 200, "{advertised}");
        assert!(
            advertised.contains("refs/heads/main"),
            "{service}: {advertised}"
        );
        for hidden in [
            "aq/other",
            "task-1/child",
            "task-10",
            &secret_commit,
            ".have",
        ] {
            assert!(
                !advertised.contains(hidden),
                "{service} advertises {hidden}: {advertised}"
            );
        }
    }
    neighbours_gone(&f);
    // Asking for a hidden commit by object ID is refused.
    let scratch = f.temp.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    git(&scratch, &["init", "-q"]);
    let url = f.url();
    let message = f.rejected(
        &scratch,
        &grant,
        &["-c", "protocol.version=0", "fetch", &url, &secret_commit],
    );
    assert!(!message.is_empty());
    assert!(
        !git_in(&scratch, &["cat-file", "-e", &secret_blob])
            .status
            .success()
    );

    // (a) A commit whose tree names the secret blob, without carrying it.
    let work = f.clone_main("work", &grant);
    let tree = String::from_utf8(git_stdin(
        &work,
        &["mktree", "--missing"],
        &format!("100644 blob {secret_blob}\tsecret.txt\n"),
    ))
    .unwrap()
    .trim()
    .to_owned();
    let commit = git(&work, &["commit-tree", &tree, "-p", &f.base, "-m", "steal"]);
    let before = f.pool_objects();
    let body = push_body(
        ZERO,
        &commit,
        TASK,
        "report-status",
        &pack_list(&work, &[&commit, &tree]),
    );
    let (status, reply) = f.raw_push(&grant, &body);
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status} {reply:?}"
    );
    assert_eq!(f.pool_ref(TASK), None);
    assert_eq!(f.pool_objects(), before, "a rejected push left objects");

    // (b) A thin pack whose delta base is the secret blob: a pack the attacker crafts by hand,
    // a REF_DELTA against an object ID they never held. Git would copy the blob into the
    // pack from the pool and hand its content out with the derived blob.
    let attacker = f.temp.path().join("attacker");
    std::fs::create_dir(&attacker).unwrap();
    git(&attacker, &["init", "-q"]);
    git(&attacker, &["fetch", "-q", work.to_str().unwrap(), "main"]);
    git(&attacker, &["checkout", "-q", "-b", "mine", &f.base]);
    let derived_text = format!("{secret_text}extra\n");
    std::fs::write(attacker.join("s2.txt"), &derived_text).unwrap();
    // The tree also names the secret blob itself, so it is part of the pushed history.
    std::fs::write(attacker.join("s.txt"), &secret_text).unwrap();
    git(&attacker, &["add", "."]);
    git(&attacker, &["commit", "-q", "-m", "derived"]);
    let derived = git(&attacker, &["rev-parse", "HEAD"]);
    let derived_tree = git(&attacker, &["rev-parse", "HEAD^{tree}"]);
    let derived_blob = git(&attacker, &["rev-parse", "HEAD:s2.txt"]);
    let raw =
        |kind: &str, oid: &str| git_stdin_output(&attacker, &["cat-file", kind, oid], b"").stdout;
    let delta = {
        let (base, extra) = (secret_text.len(), "extra\n");
        let mut d = Vec::new();
        d.extend(varint(base));
        d.extend(varint(base + extra.len()));
        d.extend([0x80 | 0x10 | 0x20, (base & 0xff) as u8, (base >> 8) as u8]);
        d.push(extra.len() as u8);
        d.extend(extra.as_bytes());
        d
    };
    let mut delta_entry = pack_header(7, delta.len());
    delta_entry.extend(hex_bytes(&secret_blob));
    delta_entry.extend(zlib_stored(&delta));
    let thin = crafted_pack(&[
        pack_entry(1, &raw("commit", &derived)),
        pack_entry(2, &raw("tree", &derived_tree)),
        delta_entry,
    ]);
    let _ = derived_blob;
    let empty = f.temp.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    assert!(
        !git_stdin_output(&empty, &["index-pack", "--stdin"], &thin)
            .status
            .success(),
        "the attack pack must be thin"
    );
    let body = push_body(ZERO, &derived, TASK, "report-status", &thin);
    let (status, reply) = f.raw_push(&grant, &body);
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status} {reply:?}"
    );
    assert_eq!(f.pool_ref(TASK), None);
    assert_eq!(
        f.pool_objects(),
        before,
        "a rejected thin push left objects: {status} {}",
        String::from_utf8_lossy(&reply)
    );

    // A ref below the task ref would make Git's own update fail after promotion: refused up front.
    git(
        &f.pool,
        &["update-ref", "refs/heads/aq/task-1/child", &f.base],
    );
    f.commit(&work, "mine.txt", "mine\n");
    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(
        message.contains("conflicts with the existing ref"),
        "{message}"
    );
    assert_eq!(f.pool_objects(), before);
    neighbours_gone(&f);
    // The honest push still works, and fetching the task ref never yields the secret.
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let again = f.temp.path().join("again");
    std::fs::create_dir(&again).unwrap();
    f.ok(
        &again,
        &grant,
        &["clone", "-q", "-b", "aq/task-1", &url, "."],
    );
    assert!(
        !git_in(&again, &["cat-file", "-e", &secret_blob])
            .status
            .success()
    );
    f.assert_no_leftovers();
}

#[test]
fn thin_push_against_the_granted_history_is_accepted() {
    let f = Fixture::new();
    let work = f.clone_main("work", &f.grant(&new_id()));
    let text: String = (0..400)
        .map(|i| format!("line {i} of a file big enough to delta\n"))
        .collect();
    let first_grant = f.grant(&new_id());
    std::fs::write(work.join("big.txt"), &text).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "big"]);
    f.ok(
        &work,
        &first_grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let head = git(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("big.txt"), text.replacen("line 7 ", "LINE 7 ", 1)).unwrap();
    git(&work, &["commit", "-q", "-am", "edit"]);
    let mut next = f.grant(&new_id());
    next.lease = Some(head);
    f.ok(
        &work,
        &next,
        &[
            "push",
            "-q",
            "--thin",
            "origin",
            "HEAD:refs/heads/aq/task-1",
        ],
    );
    assert_eq!(f.receipt(&next.operation)["ok"], true);
    f.assert_no_leftovers();
}

#[test]
fn main_is_served_from_the_retained_head_and_follows_it() {
    let f = Fixture::new();
    let grant = f.grant(&new_id());
    // No branch named main exists in the pool; the fixture used only repo.sync-backed retain_ref.
    assert_eq!(f.pool_ref("refs/heads/main"), None);
    let first = f.clone_main("first", &grant);
    assert_eq!(git(&first, &["rev-parse", "HEAD"]), f.base);

    let advanced = f.advance_remote_main("later.txt");
    // Until it is retained again, the served main does not move.
    let stale = f.clone_main("stale", &grant);
    assert_eq!(git(&stale, &["rev-parse", "HEAD"]), f.base);
    let retained = f.retain_main();
    assert_eq!(retained["ok"], true, "{retained}");
    f.ok(&first, &grant, &["fetch", "-q", "origin", "main"]);
    assert_eq!(git(&first, &["rev-parse", "FETCH_HEAD"]), advanced);
    let fresh = f.clone_main("fresh", &grant);
    assert_eq!(git(&fresh, &["rev-parse", "HEAD"]), advanced);
    // A push on top of the advanced main, under a grant whose base is the old main, descends from it.
    f.commit(&fresh, "task.txt", "task\n");
    f.ok(
        &fresh,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(f.receipt(&grant.operation)["ok"], true);
    assert_eq!(f.pool_ref("refs/heads/main"), None);
}

fn replay_fixture() -> (Fixture, Grant, PathBuf, String, Vec<u8>) {
    let f = Fixture::new();
    let grant = f.grant(&new_id());
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "result.txt", "work\n");
    let body = push_body(
        ZERO,
        &head,
        TASK,
        "report-status",
        &pack_revs(&work, &head, &[&f.base], false),
    );
    (f, grant, work, head, body)
}

#[test]
fn an_operation_gets_the_same_answer_again_and_conflicts_with_a_different_update() {
    let (f, grant, work, head, body) = replay_fixture();
    let (status, first) = f.raw_push(&grant, &body);
    assert_eq!(status, 200, "{first:?}");
    assert!(String::from_utf8_lossy(&first).contains("ok refs/heads/aq/task-1"));
    let receipt = f.receipt(&grant.operation);
    assert_eq!(receipt["result"]["new"], head.as_str());
    // The client lost the answer and retries the identical request.
    let (status, again) = f.raw_push(&grant, &body);
    assert_eq!(status, 200);
    assert_eq!(again, first);
    assert_eq!(f.receipt(&grant.operation)["result"], receipt["result"]);
    // Another update under the same operation id is a conflict, not a second push.
    let other = f.commit(&work, "more.txt", "more\n");
    let mut lease = grant.clone();
    lease.lease = Some(head.clone());
    let body = push_body(
        &head,
        &other,
        TASK,
        "report-status",
        &pack_revs(&work, &other, &[&head], false),
    );
    let (status, _) = f.raw_push(&lease, &body);
    assert_eq!(status, 409);
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(head.as_str()));
    f.assert_no_leftovers();
}

/// Moves the task ref the way an interrupted push does: the ref and the journal are
/// there, the pin and the receipt are not.
fn interrupt_push(f: &Fixture, work: &Path, operation: &str, old: &str) -> (String, String) {
    let new = f.commit(work, &format!("{operation}.txt"), "interrupted\n");
    git(
        &f.pool,
        &[
            "fetch",
            "-q",
            work.to_str().unwrap(),
            &format!("{new}:refs/scratch/{operation}"),
        ],
    );
    git(&f.pool, &["update-ref", TASK, &new, old]);
    git(
        &f.pool,
        &["update-ref", "-d", &format!("refs/scratch/{operation}")],
    );
    let handle = new_id();
    let journal = json!({"repository":f.repository,"operation_id":operation,"ref":TASK,"old":old,"new":new,
        "base":f.base,"retention_handle":handle,"binding_generation":1});
    std::fs::write(
        f.root.join(format!("git-http/journal/{operation}.json")),
        journal.to_string(),
    )
    .unwrap();
    assert_eq!(
        f.receipt(operation)["error"]["code"],
        "push_receipt_not_found"
    );
    (new, handle)
}

#[test]
fn a_push_interrupted_before_its_receipt_is_reconciled_on_restart_and_on_retry() {
    let (mut f, grant, work, head, body) = replay_fixture();
    assert_eq!(f.raw_push(&grant, &body).0, 200);

    // Restart: the journal is settled before the endpoint serves anything.
    let after_restart = new_id();
    let (new, handle) = interrupt_push(&f, &work, &after_restart, &head);
    f.restart();
    let receipt = f.receipt(&after_restart);
    assert_eq!(receipt["ok"], true, "{receipt}");
    assert_eq!(receipt["result"]["new"], new.as_str());
    assert_eq!(receipt["result"]["old"], head.as_str());
    assert_eq!(receipt["result"]["retention_handle"], handle.as_str());
    assert_eq!(receipt["result"]["retention_state"], "active");
    assert_eq!(
        f.pool_ref(&format!("refs/aq-git/pushes/{handle}"))
            .as_deref(),
        Some(new.as_str())
    );
    assert!(
        !f.root
            .join(format!("git-http/journal/{after_restart}.json"))
            .exists()
    );

    // Retry: the same request settles the journal and gets the same answer as a success.
    let on_retry = new_id();
    let (newer, handle) = interrupt_push(&f, &work, &on_retry, &new);
    let mut lease = f.grant(&on_retry);
    lease.lease = Some(new.clone());
    let retry = push_body(
        &new,
        &newer,
        TASK,
        "report-status",
        &pack_revs(&work, &newer, &[&new], false),
    );
    let (status, reply) = f.raw_push(&lease, &retry);
    assert_eq!(status, 200, "{reply:?}");
    assert!(String::from_utf8_lossy(&reply).contains("ok refs/heads/aq/task-1"));
    let receipt = f.receipt(&on_retry);
    assert_eq!(receipt["result"]["new"], newer.as_str());
    assert_eq!(receipt["result"]["retention_handle"], handle.as_str());

    // A journal whose ref never moved is dropped without a receipt.
    let never = new_id();
    let journal = json!({"repository":f.repository,"operation_id":never,"ref":TASK,"old":newer,"new":head,
        "base":f.base,"retention_handle":new_id(),"binding_generation":1});
    std::fs::write(
        f.root.join(format!("git-http/journal/{never}.json")),
        journal.to_string(),
    )
    .unwrap();
    f.restart();
    assert_eq!(f.receipt(&never)["error"]["code"], "push_receipt_not_found");
    assert!(
        !f.root
            .join(format!("git-http/journal/{never}.json"))
            .exists()
    );
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(newer.as_str()));
}

#[test]
fn a_push_whose_journal_cannot_be_written_never_moves_the_ref() {
    let (f, grant, _work, head, body) = replay_fixture();
    let objects = f.pool_objects();
    // The journal path is taken by a directory, so it cannot be written.
    let blocked = f
        .root
        .join(format!("git-http/journal/{}.json", grant.operation));
    std::fs::create_dir(&blocked).unwrap();
    let (status, reply) = f.raw_push(&grant, &body);
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status}"
    );
    assert_eq!(f.pool_ref(TASK), None, "the ref moved without a journal");
    assert_eq!(f.pool_objects(), objects);
    assert_eq!(
        f.receipt(&grant.operation)["error"]["code"],
        "push_receipt_not_found"
    );
    std::fs::remove_dir(&blocked).unwrap();
    f.assert_no_leftovers();
    // Once it can be written, the same operation succeeds.
    assert_eq!(f.raw_push(&grant, &body).0, 200);
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(head.as_str()));
}

#[test]
fn rejected_pushes_leave_no_objects_behind() {
    let (f, grant, work, head, body) = replay_fixture();
    assert_eq!(f.raw_push(&grant, &body).0, 200);
    let objects = f.pool_objects();

    // The client claims the grant's lease, but the ref is somewhere else: Git would only
    // notice after promoting the pushed objects.
    let next = f.commit(&work, "next.txt", "next\n");
    let mut stale = f.grant(&new_id());
    stale.lease = Some(f.base.clone());
    let pack = pack_revs(&work, &next, &[&head], false);
    let (status, reply) = f.raw_push(
        &stale,
        &push_body(&f.base, &next, TASK, "report-status", &pack),
    );
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status} {reply:?}"
    );
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(head.as_str()));
    assert_eq!(f.pool_objects(), objects);
    assert_eq!(
        f.receipt(&stale.operation)["error"]["code"],
        "push_receipt_not_found"
    );

    // An extra object that is not part of the pushed history.
    let junk = String::from_utf8(git_stdin(
        &work,
        &["hash-object", "-w", "--stdin"],
        "unreferenced junk\n",
    ))
    .unwrap()
    .trim()
    .to_owned();
    let mut listed = String::from_utf8(git_stdin(
        &work,
        &[
            "rev-list",
            "--objects",
            "--no-object-names",
            &next,
            "--not",
            &head,
        ],
        "",
    ))
    .unwrap();
    listed.push_str(&junk);
    let mut leased = f.grant(&new_id());
    leased.lease = Some(head.clone());
    let pack = pack_list(&work, &listed.lines().collect::<Vec<_>>());
    let (status, reply) = f.raw_push(
        &leased,
        &push_body(&head, &next, TASK, "report-status", &pack),
    );
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status} {reply:?}"
    );
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(head.as_str()));
    assert_eq!(f.pool_objects(), objects);
    f.assert_no_leftovers();

    // Git itself fails the ref update after it promoted the objects (a stale ref lock):
    // the promoted pack files are taken out again.
    let lock = f.pool.join(format!("{TASK}.lock"));
    std::fs::write(&lock, "").unwrap();
    let clean = pack_revs(&work, &next, &[&head], false);
    let (status, reply) = f.raw_push(
        &leased,
        &push_body(&head, &next, TASK, "report-status", &clean),
    );
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status} {reply:?}"
    );
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(head.as_str()));
    assert_eq!(
        f.pool_objects(),
        objects,
        "objects of a failed ref update stayed in the pool"
    );
    assert_eq!(
        f.receipt(&leased.operation)["error"]["code"],
        "push_receipt_not_found"
    );
    f.assert_no_leftovers();
    std::fs::remove_file(&lock).unwrap();

    // And the same push without the junk is accepted.
    let (status, _) = f.raw_push(
        &leased,
        &push_body(&head, &next, TASK, "report-status", &clean),
    );
    assert_eq!(status, 200);
    assert_eq!(f.pool_ref(TASK).as_deref(), Some(next.as_str()));
}

#[test]
fn push_certificates_and_extra_capabilities_are_refused_before_dispatch() {
    let (f, grant, _work, head, body) = replay_fixture();
    let objects = f.pool_objects();
    let mut cert = pkt(b"push-cert\0report-status");
    cert.extend(pkt(
        b"certificate version 0.1\npusher Worker <worker@example.test> 1 +0000\n",
    ));
    cert.extend(pkt(format!("\n{ZERO} {head} {TASK}\n").as_bytes()));
    cert.extend_from_slice(b"0000");
    let (status, _) = f.raw_push(&grant, &cert);
    assert_eq!(status, 400);
    for caps in [
        "report-status push-options",
        "report-status atomic",
        "report-status push-cert=abc",
    ] {
        let pack_start = body.windows(4).position(|w| w == b"PACK").unwrap();
        let (status, _) = f.raw_push(
            &grant,
            &push_body(ZERO, &head, TASK, caps, &body[pack_start..]),
        );
        assert_eq!(status, 400, "{caps}");
    }
    assert_eq!(f.pool_ref(TASK), None);
    assert_eq!(f.pool_objects(), objects);
    assert_eq!(
        f.receipt(&grant.operation)["error"]["code"],
        "push_receipt_not_found"
    );
    f.assert_no_leftovers();
    // Nothing of the certificate was written anywhere in the pool.
    let found = Command::new("grep")
        .args(["-rl", "certificate version", f.pool.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        found.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&found.stdout)
    );
}

#[test]
fn a_receive_that_cannot_fit_the_disk_reserve_or_quarantine_budget_is_refused_cleanly() {
    let (f, grant, work, head, body) = replay_fixture();
    let limits = |expanded: u64, reserve: u64| {
        std::fs::write(
            f.root.join("artifact_limits.json"),
            json!({"max_bundle_bytes":10_000_000,"max_expanded_bytes":expanded,"disk_reserve_bytes":reserve}).to_string(),
        )
        .unwrap();
    };
    // The shared free-disk reserve is unsatisfiable: refused before the body is read.
    limits(50_000_000, 1 << 60);
    let (status, _) = f.raw_push(&grant, &body);
    assert_eq!(status, 500);
    assert_eq!(f.pool_ref(TASK), None);
    // A quarantine budget the push exceeds: killed or rejected, and nothing stays behind.
    let noise: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(work.join("noise.bin"), noise).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "noise"]);
    let big = git(&work, &["rev-parse", "HEAD"]);
    let objects = f.pool_objects();
    let pack = pack_revs(&work, &big, &[&f.base], false);
    limits(pack.len() as u64 / 2, 1_000_000);
    let (status, reply) = f.raw_push(&grant, &push_body(ZERO, &big, TASK, "report-status", &pack));
    assert!(
        status != 200 || String::from_utf8_lossy(&reply).contains("ng "),
        "{status} {}",
        String::from_utf8_lossy(&reply)
    );
    assert_eq!(f.pool_ref(TASK), None);
    assert_eq!(f.pool_objects(), objects);
    f.assert_no_leftovers();
    // With room again the very same request goes through.
    limits(50_000_000, 1_000_000);
    assert_eq!(
        f.raw_push(&grant, &push_body(ZERO, &big, TASK, "report-status", &pack))
            .0,
        200
    );
    let _ = head;
}

#[test]
fn slow_request_bodies_hit_the_deadline_and_hold_no_shared_slot() {
    let f = Fixture::with_limits(json!({"request_deadline_secs":2}));
    let grant = f.grant(&new_id());
    let head = |operation: &str, length: usize| {
        let mut g = grant.clone();
        g.operation = operation.to_string();
        format!(
            "POST /{}/git-receive-pack HTTP/1.1\r\nHost: x\r\n{}Content-Type: application/x-git-receive-pack-request\r\nContent-Length: {length}\r\n\r\n",
            f.repository,
            f.grant_headers(&g, "")
        )
    };
    let started = std::time::Instant::now();
    // More slow uploads than the heavy-operation budget allows operations.
    let slow: Vec<_> = (0..6)
        .map(|_| {
            let head = head(&new_id(), 1_000_000);
            let socket = f.http_socket.clone();
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut stream = UnixStream::connect(socket).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                stream.write_all(head.as_bytes()).unwrap();
                let mut reply = Vec::new();
                // A byte every 300 ms keeps the inactivity timeout from ever firing.
                for _ in 0..40 {
                    if stream.write_all(b"0").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    stream.set_nonblocking(true).unwrap();
                    let mut chunk = [0u8; 512];
                    if let Ok(n) = stream.read(&mut chunk)
                        && n > 0
                    {
                        reply.extend_from_slice(&chunk[..n]);
                        break;
                    }
                    stream.set_nonblocking(false).unwrap();
                }
                parse_reply(&reply).0
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(700));
    // While they dribble: no heavy permit, no staging file kept, and a real push is served.
    let metrics = f.metrics();
    assert_eq!(metrics["heavy"]["active"], 0, "{metrics}");
    let work = f.clone_main("work", &grant);
    f.commit(&work, "fast.txt", "fast\n");
    let real = f.grant(&new_id());
    f.ok(
        &work,
        &real,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    for slow in slow {
        assert_eq!(slow.join().unwrap(), 408);
    }
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    f.assert_no_leftovers();
    assert_eq!(f.metrics()["heavy"]["active"], 0);
}

#[test]
fn shared_memory_budget_refuses_buffered_requests_beyond_it() {
    let f = Fixture::with_limits(json!({"max_buffered_bytes":64}));
    let grant = f.grant(&new_id());
    let (status, body) = f.advertisement(&grant, "upload-pack");
    assert_eq!(status, 503, "{body}");
    let (status, _) = f.raw(
        "POST",
        "git-upload-pack",
        &grant,
        "application/x-git-upload-pack-request",
        &[b'0'; 400],
    );
    assert_eq!(status, 503);
    f.assert_no_leftovers();
    // The git children never ran away with the child slots.
    assert_eq!(f.metrics()["git"]["active"], 0);
}
