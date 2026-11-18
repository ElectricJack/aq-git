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
    port: u16,
    repository: String,
    base: String,
    pool: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.service.id() as i32, libc::SIGINT);
        }
        let _ = self.service.wait();
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
impl Fixture {
    fn new() -> Self {
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
        let mut service = bin()
            .arg("--state-dir")
            .arg(&root)
            .arg("--listen")
            .arg(&json_socket)
            .arg("--git-http-socket")
            .arg(&http_socket)
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
            UnixStream::connect(&json_socket).is_ok() && UnixStream::connect(&http_socket).is_ok()
        });
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
        // aq-server publishes the current main into the pool's branch namespace.
        git(&pool, &["update-ref", "refs/heads/main", &base]);
        let port = forward(http_socket);
        Self {
            temp,
            service,
            json_socket,
            port,
            repository,
            base,
            pool,
        }
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
        assert_eq!(
            f.pool_ref("refs/heads/main").as_deref(),
            Some(f.base.as_str())
        );
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
