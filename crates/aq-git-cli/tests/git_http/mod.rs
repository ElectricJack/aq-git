//! End to end: `aq-git serve --git-http-socket`, a real Git client through a TCP
//! forwarder to the Unix socket, with the grant in `http.extraHeader`. The project
//! repository is a plain Git server: fetch sees main, `aq/*` and `aq-try/*`, a push
//! may update only the grant's task branch (and its try ref, when granted), and the
//! branch is the record of the result.
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
    /// Sent as `AQ-Try-Ref` when set (verbatim, so a test can send a malformed one).
    try_ref: Option<String>,
    max_push_bytes: u64,
}
struct Fixture {
    temp: tempfile::TempDir,
    service: Child,
    json_socket: PathBuf,
    http_socket: PathBuf,
    port: u16,
    repository: String,
    base: String,
    pool: PathBuf,
    remote: PathBuf,
    source: PathBuf,
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
fn oid(hex: &str) -> Value {
    json!({"algorithm":"sha1","hex":hex})
}
fn read_ref(repository: &Path, name: &str) -> Option<String> {
    let output = git_in(repository, &["rev-parse", "--verify", "--quiet", name]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn seed_remote(directory: &Path) -> (PathBuf, PathBuf, String) {
    let source = directory.join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "base\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let base = git(&source, &["rev-parse", "HEAD"]);
    let remote = directory.join("remote.git");
    git(
        directory,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    (source, remote, base)
}

fn register(socket: &Path, remote: &Path, repository: &str, scope: Value) -> Value {
    let mut binding = json!({"identity":"fixture","mode":"central","allowed_refs":["refs/heads/main"],
            "allowed_targets":["refs/heads/main"],"allowed_target_prefixes":["refs/heads/aq/"],
            "remote_url":remote,"allow_file_transport":true});
    for (key, value) in scope.as_object().unwrap() {
        binding[key] = value.clone();
    }
    rpc(
        socket,
        json!({"protocol_version":1,"request_id":"register","operation":"repo.register",
                "repository_id":repository,"operation_id":new_id(),"expected_binding_generation":0,
                "params":{"binding":binding}}),
    )
}

impl Fixture {
    fn new() -> Self {
        Self::with_limits(None)
    }
    /// `limits` is written to `git_http_limits.json` before the service starts.
    fn with_limits(limits: Option<Value>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let (source, remote, base) = seed_remote(temp.path());
        let root = temp.path().join("state");
        if let Some(limits) = limits {
            // A direct run creates the state root; settings go in before `serve`.
            let created = bin()
                .arg("--state-dir")
                .arg(&root)
                .args(["--json", "capabilities"])
                .output()
                .unwrap();
            assert!(created.status.success());
            std::fs::write(root.join("git_http_limits.json"), limits.to_string()).unwrap();
        }
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
        let registered = register(&json_socket, &remote, &repository, json!({}));
        assert_eq!(registered["ok"], true, "{registered}");
        let port = forward(http_socket.clone());
        let fixture = Self {
            pool: root.join(format!("repositories/{repository}/objects.git")),
            temp,
            service,
            json_socket,
            http_socket,
            port,
            repository,
            base,
            remote,
            source,
        };
        fixture.sync();
        fixture
    }
    /// `repo.sync` of main; it moves the repository's `refs/heads/main`.
    fn sync(&self) {
        let synced = self.call("repo.sync", true, json!({"refs":["refs/heads/main"]}));
        assert_eq!(synced["ok"], true, "{synced}");
    }
    /// A JSON-protocol request on the fixture's repository.
    fn call(&self, operation: &str, mutation: bool, params: Value) -> Value {
        let mut request = json!({"protocol_version":1,"request_id":operation,"operation":operation,
            "repository_id":self.repository,"params":params});
        if mutation {
            request["operation_id"] = json!(new_id());
            request["expected_binding_generation"] = json!(1);
        }
        rpc(&self.json_socket, request)
    }
    fn ref_show(&self, name: &str) -> Value {
        let shown = self.call("ref.show", false, json!({"ref":name}));
        assert_eq!(shown["ok"], true, "{shown}");
        shown["result"]["oid"].clone()
    }
    fn grant(&self) -> Grant {
        Grant {
            task_ref: "refs/heads/aq/task-1".into(),
            try_ref: None,
            max_push_bytes: 10_000_000,
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
            format!("AQ-Max-Push-Bytes: {}", grant.max_push_bytes),
            format!("AQ-Operation: {}", new_id()),
        ];
        if let Some(try_ref) = &grant.try_ref {
            headers.push(format!("AQ-Try-Ref: {try_ref}"));
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
        read_ref(&self.pool, name)
    }
    /// Every branch and tag of the repository, to show a refused push changed nothing.
    fn branches(&self) -> String {
        git(
            &self.pool,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/tags",
            ],
        )
    }
    fn listing(&self, dir: &Path, grant: &Grant, extra: &[&str]) -> Vec<String> {
        let url = self.url();
        let args: Vec<&str> = extra
            .iter()
            .copied()
            .chain(["ls-remote", url.as_str()])
            .collect();
        self.ok(dir, grant, &args)
            .lines()
            .map(|l| l.split('\t').nth(1).unwrap().to_string())
            .collect()
    }
}

#[test]
fn fetch_sees_main_and_task_branches_and_main_follows_sync() {
    let f = Fixture::new();
    let grant = f.grant();
    // repo.sync created a real main branch at the synced head.
    assert_eq!(
        f.pool_ref("refs/heads/main").as_deref(),
        Some(f.base.as_str())
    );
    assert_eq!(f.ref_show("refs/heads/main"), oid(&f.base));
    // Other task branches are visible; refs outside main and aq/ are not.
    git(&f.pool, &["update-ref", "refs/heads/secret", &f.base]);
    git(&f.pool, &["update-ref", "refs/heads/aq/other", &f.base]);
    git(&f.pool, &["update-ref", "refs/tags/v1", &f.base]);
    let scratch = f.temp.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    git(&scratch, &["init", "-q"]);
    for version in ["0", "2"] {
        let protocol = format!("protocol.version={version}");
        assert_eq!(
            f.listing(&scratch, &grant, &["-c", &protocol]),
            ["refs/heads/aq/other", "refs/heads/main"],
            "protocol v{version}"
        );
    }
    // A fetch of a task branch works.
    let work = f.clone_main("work", &grant);
    f.ok(&work, &grant, &["fetch", "-q", "origin", "aq/other"]);
    assert_eq!(git(&work, &["rev-parse", "FETCH_HEAD"]), f.base);

    // main moves when the remote moves and aq-git syncs; fetch serves the new head.
    let next = f.commit(&f.source, "next.txt", "next\n");
    let remote = f.remote.to_str().unwrap();
    git(&f.source, &["push", "-q", remote, "main"]);
    f.sync();
    assert_eq!(f.ref_show("refs/heads/main"), oid(&next));
    f.ok(&work, &grant, &["fetch", "-q", "origin"]);
    assert_eq!(git(&work, &["rev-parse", "origin/main"]), next);
    // repo.retain_ref of main serves the same head.
    let retained = f.call("repo.retain_ref", true, json!({"ref":"refs/heads/main"}));
    assert_eq!(retained["ok"], true, "{retained}");
    assert_eq!(
        f.pool_ref("refs/heads/main").as_deref(),
        Some(next.as_str())
    );
}

#[test]
fn push_to_the_task_branch_lands_and_a_returned_task_can_rebuild_it() {
    let f = Fixture::new();
    let grant = f.grant();
    let url = f.url();
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "result.txt", "work\n");
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), Value::Null);
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(
        f.pool_ref("refs/heads/aq/task-1").as_deref(),
        Some(head.as_str())
    );
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), oid(&head));
    // No pin and no receipt: the branch is the record.
    assert_eq!(
        git(
            &f.pool,
            &["for-each-ref", "--format=%(refname)", "refs/aq-git/pushes"]
        ),
        ""
    );
    let receipt = f.call("push.receipt", false, json!({"operation_id":new_id()}));
    assert_eq!(receipt["error"]["code"], "unsupported_feature", "{receipt}");

    // A fresh clone of the task branch sees the work.
    let again = f.temp.path().join("again");
    std::fs::create_dir(&again).unwrap();
    f.ok(
        &again,
        &grant,
        &["clone", "-q", "-b", "aq/task-1", &url, "."],
    );
    assert_eq!(git(&again, &["rev-parse", "HEAD"]), head);

    // A returned task rewrites its branch: a non-fast-forward push with the right lease.
    git(&work, &["reset", "-q", "--hard", &f.base]);
    let rebuilt = f.commit(&work, "rebuilt.txt", "rebuilt\n");
    let stale = format!("--force-with-lease=refs/heads/aq/task-1:{}", f.base);
    let message = f.rejected(
        &work,
        &grant,
        &["push", &stale, "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert!(message.contains("stale info"), "{message}");
    assert_eq!(
        f.pool_ref("refs/heads/aq/task-1").as_deref(),
        Some(head.as_str())
    );
    let lease = format!("--force-with-lease=refs/heads/aq/task-1:{head}");
    f.ok(
        &work,
        &grant,
        &["push", "-q", &lease, "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), oid(&rebuilt));
}

#[test]
fn push_outside_the_granted_task_branch_or_a_delete_is_refused() {
    let f = Fixture::new();
    let grant = f.grant();
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "result.txt", "work\n");
    git(&f.pool, &["update-ref", "refs/heads/aq/task-2", &f.base]);
    let before = f.branches();

    for (target, expected) in [
        ("refs/heads/main", "not the granted task ref"),
        ("refs/heads/aq/task-2", "not the granted task ref"),
        ("refs/heads/aq/task-3", "not the granted task ref"),
        ("refs/heads/other", "hidden"),
        ("refs/tags/v1", "hidden"),
    ] {
        let spec = format!("HEAD:{target}");
        let message = f.rejected(&work, &grant, &["push", "origin", &spec]);
        assert!(
            message.contains(expected) || message.contains("declined"),
            "{target}: {message}"
        );
        assert_eq!(f.branches(), before, "{target}");
    }
    // Together with the granted branch, the whole push is refused.
    for options in [&["--atomic"][..], &[]] {
        let mut args = vec!["push"];
        args.extend_from_slice(options);
        args.extend([
            "origin",
            "HEAD:refs/heads/aq/task-1",
            "HEAD:refs/heads/aq/task-2",
        ]);
        let message = f.rejected(&work, &grant, &args);
        assert!(message.contains("not the granted task ref"), "{message}");
    }
    assert_eq!(f.branches(), before);

    // Deleting the task branch, or another branch, is refused.
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let before = f.branches();
    for target in [
        ":refs/heads/aq/task-1",
        ":refs/heads/aq/task-2",
        ":refs/heads/main",
    ] {
        let message = f.rejected(&work, &grant, &["push", "origin", target]);
        assert!(
            message.contains("deleting")
                || message.contains("deletion prohibited")
                || message.contains("declined")
                || message.contains("not the granted task ref"),
            "{target}: {message}"
        );
        assert_eq!(f.branches(), before, "{target}");
    }
    assert_eq!(
        f.pool_ref("refs/heads/aq/task-1").as_deref(),
        Some(head.as_str())
    );
}

fn create_invalid_commit(work: &Path, base: &str) {
    // A commit object that git fsck --strict rejects (author without an email).
    let tree = git(work, &["rev-parse", "HEAD^{tree}"]);
    let body = format!(
        "tree {tree}\nparent {}\nauthor Broken Author 1 +0000\ncommitter Broken <b@example.test> 1 +0000\n\nbroken\n",
        base
    );
    std::fs::write(work.join("broken-commit"), body).unwrap();
    let corrupt = git(
        work,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "broken-commit",
        ],
    );
    git(work, &["update-ref", "refs/heads/corrupt", &corrupt]);
}

#[test]
fn fsck_failing_and_oversize_pushes_are_refused_without_a_ref_change() {
    let f = Fixture::new();
    let mut grant = f.grant();
    let work = f.clone_main("work", &grant);
    let before = f.branches();

    create_invalid_commit(&work, &f.base);
    let message = f.rejected(
        &work,
        &grant,
        &["push", "origin", "refs/heads/corrupt:refs/heads/aq/task-1"],
    );
    assert!(
        message.contains("fsck") || message.contains("missingEmail") || message.contains("unpack"),
        "{message}"
    );
    assert_eq!(f.branches(), before);

    // Above the grant's byte limit: receive.maxInputSize and the request body limit.
    grant.max_push_bytes = 4096;
    let noise: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(work.join("blob.bin"), noise).unwrap();
    git(&work, &["add", "blob.bin"]);
    git(&work, &["commit", "-q", "-m", "big"]);
    f.rejected(
        &work,
        &grant,
        &["push", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(f.branches(), before);
    // Chunked bodies (above git's post buffer) are held to the same limit.
    let big = f.clone_main("big", &grant);
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
    assert_eq!(f.branches(), before);
    // The same push with a sufficient grant is accepted.
    grant.max_push_bytes = 50_000_000;
    f.ok(
        &big,
        &grant,
        &[
            "-c",
            "http.postBuffer=65536",
            "push",
            "-q",
            "origin",
            "HEAD:refs/heads/aq/task-1",
        ],
    );
    let head = git(&big, &["rev-parse", "HEAD"]);
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), oid(&head));
}

#[test]
fn a_pushed_task_branch_publishes_to_origin_and_is_deleted() {
    let f = Fixture::new();
    let grant = f.grant();
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "result.txt", "work\n");
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let zero = "0".repeat(40);
    let publish = |expected: &str, source: &str, update: &str| {
        f.call(
            "publish",
            true,
            json!({"source_ref":"refs/heads/aq/task-1","target":"refs/heads/aq/task-1",
                "expected_old":oid(expected),"expected_source":oid(source),"update":update}),
        )
    };
    let stale = publish(&zero, &f.base, "fast_forward");
    assert_eq!(stale["error"]["code"], "source_moved", "{stale}");
    assert_eq!(read_ref(&f.remote, "refs/heads/aq/task-1"), None);
    let published = publish(&zero, &head, "fast_forward");
    assert_eq!(published["ok"], true, "{published}");
    assert_eq!(published["result"]["outcome"], "applied", "{published}");
    assert_eq!(
        read_ref(&f.remote, "refs/heads/aq/task-1").as_deref(),
        Some(head.as_str())
    );

    // The returned task's rebuilt branch replaces the published head.
    git(&work, &["reset", "-q", "--hard", &f.base]);
    let rebuilt = f.commit(&work, "rebuilt.txt", "rebuilt\n");
    let lease = format!("--force-with-lease=refs/heads/aq/task-1:{head}");
    f.ok(
        &work,
        &grant,
        &["push", "-q", &lease, "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let refused = publish(&head, &rebuilt, "fast_forward");
    assert_eq!(refused["error"]["code"], "target_moved", "{refused}");
    let stale = publish(&head, &head, "replace");
    assert_eq!(stale["error"]["code"], "source_moved", "{stale}");
    assert_eq!(
        read_ref(&f.remote, "refs/heads/aq/task-1").as_deref(),
        Some(head.as_str())
    );
    let replaced = publish(&head, &rebuilt, "replace");
    assert_eq!(replaced["result"]["outcome"], "applied", "{replaced}");
    assert_eq!(
        read_ref(&f.remote, "refs/heads/aq/task-1").as_deref(),
        Some(rebuilt.as_str())
    );
    // Only task branches are a ref source.
    let other = f.call(
        "publish",
        true,
        json!({"source_ref":"refs/heads/main","target":"refs/heads/aq/x","expected_old":oid(&zero)}),
    );
    assert_eq!(other["error"]["code"], "invalid_request", "{other}");

    // After the merge, the branch is deleted here and on origin.
    let deleted = f.call(
        "ref.delete",
        true,
        json!({"ref":"refs/heads/aq/task-1","expected_old":oid(&rebuilt),"remote":true}),
    );
    assert_eq!(deleted["result"]["outcome"], "deleted", "{deleted}");
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), Value::Null);
    assert_eq!(read_ref(&f.remote, "refs/heads/aq/task-1"), None);
}

#[test]
fn periodic_gc_packs_the_repository() {
    let f = Fixture::with_limits(Some(json!({"gc_interval_secs":1,"gc_auto_objects":1})));
    let grant = f.grant();
    let work = f.clone_main("work", &grant);
    for i in 0..1200 {
        std::fs::write(work.join(format!("{i}.txt")), format!("{i}\n")).unwrap();
    }
    let head = f.commit(&work, "result.txt", "work\n");
    // Enough reachable loose objects that `git gc --auto` (which samples objects/17) packs them.
    git(
        &f.pool,
        &[
            "-c",
            "fetch.unpackLimit=100000",
            "fetch",
            "-q",
            work.to_str().unwrap(),
            "HEAD:refs/heads/aq/task-1",
        ],
    );
    let loose = || {
        git(&f.pool, &["count-objects", "-v"])
            .lines()
            .find_map(|l| l.strip_prefix("count: "))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    assert!(loose() > 1000);
    wait_until(|| loose() == 0);
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), oid(&head));
    f.ok(&work, &grant, &["fetch", "-q", "origin", "aq/task-1"]);
}

#[test]
fn malformed_grants_and_foreign_repositories_get_http_errors() {
    let f = Fixture::new();
    let send = |request: &str| -> String {
        let mut stream = UnixStream::connect(&f.http_socket).unwrap();
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
            "GET /{repository}/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: x\r\nAQ-Repository: {repository}\r\nAQ-Base: {}\r\nAQ-Task-Ref: refs/heads/aq/t\r\nAQ-Max-Push-Bytes: 100\r\nAQ-Operation: {}\r\n{extra}\r\n",
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
    let task_ref_outside =
        headers(&f.repository, "").replacen("refs/heads/aq/t", "refs/heads/main", 1);
    assert!(send(&task_ref_outside).contains(" 400 "));
}

/// A grant for `task-1` that also grants `attempt`'s try ref.
fn try_grant(f: &Fixture, attempt: &str) -> (Grant, String) {
    let try_ref = format!("refs/heads/aq-try/{attempt}");
    let grant = Grant {
        try_ref: Some(try_ref.clone()),
        ..f.grant()
    };
    (grant, try_ref)
}

#[test]
fn a_granted_try_ref_can_be_created_and_force_updated() {
    let f = Fixture::new();
    let (grant, try_ref) = try_grant(&f, &new_id());
    let work = f.clone_main("work", &grant);
    let task = f.commit(&work, "task.txt", "task\n");
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    assert_eq!(f.ref_show(&try_ref), Value::Null);

    // The first try creates the ref.
    git(&work, &["reset", "-q", "--hard", &f.base]);
    let first = f.commit(&work, "try.txt", "first\n");
    let spec = format!("HEAD:{try_ref}");
    f.ok(&work, &grant, &["push", "-q", "origin", &spec]);
    assert_eq!(f.ref_show(&try_ref), oid(&first));

    // The next try is a sibling, not a descendant: a forced update moves the ref.
    git(&work, &["reset", "-q", "--hard", &f.base]);
    let second = f.commit(&work, "try.txt", "second\n");
    assert_ne!(first, second);
    let forced = format!("+HEAD:{try_ref}");
    f.ok(&work, &grant, &["push", "-q", "--force", "origin", &forced]);
    assert_eq!(f.ref_show(&try_ref), oid(&second));
    assert_eq!(f.pool_ref(&try_ref).as_deref(), Some(second.as_str()));
    // The task branch is unchanged.
    assert_eq!(f.ref_show("refs/heads/aq/task-1"), oid(&task));
}

#[test]
fn a_try_ref_push_without_the_grant_or_for_another_attempt_is_refused() {
    let f = Fixture::new();
    let (a, b) = (new_id(), new_id());
    let (granted, try_a) = try_grant(&f, &a);
    let try_b = format!("refs/heads/aq-try/{b}");
    let work = f.clone_main("work", &granted);
    f.commit(&work, "try.txt", "try\n");
    let before = f.branches();

    // Without AQ-Try-Ref, a push to the try ref is refused.
    let spec_a = format!("HEAD:{try_a}");
    let message = f.rejected(&work, &f.grant(), &["push", "origin", &spec_a]);
    assert!(message.contains("not the granted task ref"), "{message}");
    assert_eq!(f.branches(), before);
    // A grant for attempt A cannot push attempt B's try ref.
    let spec_b = format!("HEAD:{try_b}");
    let message = f.rejected(&work, &granted, &["push", "origin", &spec_b]);
    assert!(message.contains("not the granted task ref"), "{message}");
    assert_eq!(f.branches(), before);
    // A malformed AQ-Try-Ref is a grant error (400) before any Git runs.
    for malformed in [
        format!("refs/heads/aq/{a}"),
        format!("refs/heads/aq-try/{}", a.to_uppercase()),
        format!("{try_a}/x"),
    ] {
        let bad = Grant {
            try_ref: Some(malformed.clone()),
            ..f.grant()
        };
        let message = f.rejected(&work, &bad, &["push", "origin", &spec_a]);
        assert!(message.contains("400"), "{malformed}: {message}");
        assert_eq!(f.branches(), before, "{malformed}");
    }

    // Deleting the granted try ref is refused.
    f.ok(&work, &granted, &["push", "-q", "origin", &spec_a]);
    let before = f.branches();
    let delete = format!(":{try_a}");
    let message = f.rejected(&work, &granted, &["push", "origin", &delete]);
    assert!(
        message.contains("deletion prohibited")
            || message.contains("deleting the try ref is not allowed"),
        "{message}"
    );
    assert_eq!(f.branches(), before);
}

#[test]
fn fetch_advertises_try_refs() {
    let f = Fixture::new();
    let (grant, try_ref) = try_grant(&f, &new_id());
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "try.txt", "try\n");
    let spec = format!("HEAD:{try_ref}");
    f.ok(&work, &grant, &["push", "-q", "origin", &spec]);

    // A builder (no try grant of its own) mirrors every branch.
    let builder = f.temp.path().join("builder");
    std::fs::create_dir(&builder).unwrap();
    git(&builder, &["init", "-q"]);
    let url = f.url();
    let plain = f.grant();
    assert!(
        f.listing(&builder, &plain, &[]).contains(&try_ref),
        "try ref is advertised"
    );
    f.ok(
        &builder,
        &plain,
        &["fetch", "-q", &url, "+refs/heads/*:refs/remotes/origin/*"],
    );
    let mirrored = try_ref.replacen("refs/heads/", "refs/remotes/origin/", 1);
    assert_eq!(git(&builder, &["rev-parse", &mirrored]), head);
}

#[test]
fn ref_delete_removes_a_try_ref_locally_and_is_idempotent() {
    let f = Fixture::new();
    let (grant, try_ref) = try_grant(&f, &new_id());
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "try.txt", "try\n");
    let spec = format!("HEAD:{try_ref}");
    f.ok(&work, &grant, &["push", "-q", "origin", &spec]);
    let origin_refs = || {
        git(
            &f.remote,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
    };
    let origin_before = origin_refs();
    let delete = |params: Value| f.call("ref.delete", true, params);

    // A mismatched lease answers target_moved and the ref stays.
    let moved = delete(json!({"ref":try_ref,"expected_old":oid(&f.base),"remote":false}));
    assert_eq!(moved["error"]["code"], "target_moved", "{moved}");
    assert_eq!(moved["error"]["details"]["side"], "local", "{moved}");
    assert_eq!(f.pool_ref(&try_ref).as_deref(), Some(head.as_str()));
    // remote:true is refused and nothing changes.
    let remote = delete(json!({"ref":try_ref,"remote":true}));
    assert_eq!(remote["error"]["code"], "unsupported_feature", "{remote}");
    assert_eq!(f.pool_ref(&try_ref).as_deref(), Some(head.as_str()));

    // The local deletion, outside every publication scope of the binding.
    let request = json!({"protocol_version":1,"request_id":"delete","operation":"ref.delete",
        "repository_id":f.repository,"operation_id":new_id(),"expected_binding_generation":1,
        "params":{"ref":try_ref,"expected_old":oid(&head),"remote":false}});
    let deleted = rpc(&f.json_socket, request.clone());
    assert_eq!(deleted["ok"], true, "{deleted}");
    assert_eq!(deleted["result"]["outcome"], "deleted", "{deleted}");
    assert_eq!(deleted["result"]["ref"], json!(try_ref), "{deleted}");
    assert_eq!(deleted["result"]["old"], oid(&head), "{deleted}");
    assert_eq!(
        deleted["result"]["remote"],
        json!({"requested":false,"outcome":"not_requested"}),
        "{deleted}"
    );
    assert_eq!(
        deleted["result"]["local"]["outcome"], "deleted",
        "{deleted}"
    );
    assert_eq!(f.pool_ref(&try_ref), None);
    assert_eq!(f.ref_show(&try_ref), Value::Null);
    // A replay of the same operation succeeds.
    let replay = rpc(&f.json_socket, request);
    assert_eq!(replay["ok"], true, "{replay}");
    // A new operation on the absent ref answers already_absent.
    let again = delete(json!({"ref":try_ref,"remote":false}));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(again["result"]["outcome"], "already_absent", "{again}");
    assert_eq!(
        again["result"]["local"]["outcome"], "already_absent",
        "{again}"
    );
    // The origin never had the ref and is unchanged.
    assert_eq!(origin_refs(), origin_before);
    assert!(!origin_before.contains("aq-try"), "{origin_before}");

    // A malformed try ref name follows the ordinary scope rules.
    let malformed = delete(json!({"ref":format!("{try_ref}/x"),"remote":false}));
    assert_eq!(
        malformed["error"]["code"], "unsupported_feature",
        "{malformed}"
    );
}

fn binding_with_legacy_try_prefix(f: &Fixture) -> String {
    let covering = new_id();
    let state = f.pool.parent().unwrap().parent().unwrap();
    let mut binding: Value = serde_json::from_slice(
        &std::fs::read(state.join(&f.repository).join("binding.json")).unwrap(),
    )
    .unwrap();
    binding["repository_id"] = json!(covering);
    binding["allowed_target_prefixes"] = json!(["refs/heads/aq/", "refs/heads/aq-try/"]);
    std::fs::create_dir(state.join(&covering)).unwrap();
    std::fs::write(
        state.join(&covering).join("binding.json"),
        binding.to_string(),
    )
    .unwrap();
    covering
}

#[test]
fn try_refs_are_never_published() {
    let f = Fixture::new();
    let (grant, try_ref) = try_grant(&f, &new_id());
    let work = f.clone_main("work", &grant);
    let head = f.commit(&work, "try.txt", "try\n");
    let spec = format!("HEAD:{try_ref}");
    f.ok(&work, &grant, &["push", "-q", "origin", &spec]);
    f.ok(
        &work,
        &grant,
        &["push", "-q", "origin", "HEAD:refs/heads/aq/task-1"],
    );
    let zero = "0".repeat(40);
    let origin_refs = || {
        git(
            &f.remote,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
    };
    let origin_before = origin_refs();

    // A try ref is not a publication source.
    let source = f.call(
        "publish",
        true,
        json!({"source_ref":try_ref,"target":"refs/heads/aq/task-1","expected_old":oid(&zero),
            "expected_source":oid(&head)}),
    );
    assert_eq!(source["error"]["code"], "invalid_request", "{source}");

    // Registration refuses try refs as publication scopes.
    for scope in [
        json!({"allowed_targets":[try_ref]}),
        json!({"allowed_refs":[try_ref]}),
        json!({"allowed_target_prefixes":["refs/heads/aq-try/"]}),
        json!({"allowed_target_prefixes":["refs/heads/aq-try/x/"]}),
    ] {
        let refused = register(&f.json_socket, &f.remote, &new_id(), scope.clone());
        assert_eq!(
            refused["error"]["code"], "invalid_request",
            "{scope}: {refused}"
        );
    }

    // A target under refs/heads/aq-try/ is refused, also under a binding whose prefix
    // covers it (one stored before registration refused such prefixes).
    let covering = binding_with_legacy_try_prefix(&f);
    for repository in [&f.repository, &covering] {
        let target = rpc(
            &f.json_socket,
            json!({"protocol_version":1,"request_id":"publish","operation":"publish",
                "repository_id":repository,"operation_id":new_id(),"expected_binding_generation":1,
                "params":{"source_ref":"refs/heads/aq/task-1","target":try_ref,
                    "expected_old":oid(&zero),"update":"replace"}}),
        );
        assert_eq!(
            target["error"]["code"], "unsupported_feature",
            "{repository}: {target}"
        );
        assert_eq!(
            target["error"]["message"], "try refs are never published",
            "{repository}: {target}"
        );
    }
    assert_eq!(origin_refs(), origin_before);
}
