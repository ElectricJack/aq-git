use super::*;

fn assert_checkpoint_history(path: &Path, commit: &str, parent: &str, original: &str) {
    assert_eq!(git(path, &["rev-parse", "HEAD"]), commit);
    assert_eq!(git(path, &["rev-parse", "HEAD^"]), parent);
    assert_eq!(git(path, &["cat-file", "commit", parent]), original);
}
fn commit_existing_work(path: &Path) -> (String, String) {
    std::fs::write(path.join("file.txt"), "already committed\n").unwrap();
    git(path, &["add", "file.txt"]);
    git(
        path,
        &[
            "commit",
            "--author=Earlier Author <earlier@example.test>",
            "-m",
            "earlier",
        ],
    );
    let parent = git(path, &["rev-parse", "HEAD"]);
    let original = git(path, &["cat-file", "commit", &parent]);

    (parent, original)
}
fn reopen(state: &Path) -> aq_git_core::RepositoryService {
    use aq_git_core::RepositoryService;
    use std::time::{Duration, Instant};
    // A parallel test's Git child can inherit the owner lock until exec closes its copy.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match RepositoryService::open(state) {
            Err(error) if error.code == "state_busy" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => break result.expect("reopen checkpoint state after dropping the owner"),
        }
    }
}

#[test]
fn checkpoint_split_author_preserves_history_and_retries_after_restart() {
    use aq_git_core::state::WriteFault;

    let f = Fixture::new();
    assert_eq!(f.service.capabilities()["git"]["checkpoint_author"], true);
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    let (parent, original) = commit_existing_work(path);
    let mut params = split_commit(&w, &parent);
    let clean = success(
        &f.service,
        request(&f.repository, "workspace.checkpoint", params.clone()),
    );
    assert_eq!(clean["source"], oid(&parent));
    assert_eq!(git(path, &["rev-parse", "HEAD"]), parent);

    params["expected_generation"] = clean["workspace_generation"].clone();
    std::fs::write(path.join("file.txt"), "checkpoint changes\n").unwrap();
    let checkpoint = request(&f.repository, "workspace.checkpoint", params);
    let op = checkpoint.operation_id.as_deref().unwrap();
    // Fail after commit-tree, before the checkpoint record can save its OID.
    f.service
        .root
        .inject_write_failure_after(1, WriteFault::BeforeRename);
    let interrupted = f.service.execute(checkpoint.clone());
    assert_eq!(interrupted.error.unwrap().code, "io_error");
    assert_eq!(interrupted.operation_state.as_deref(), Some("pending"));
    assert_eq!(git(path, &["rev-parse", "HEAD"]), parent);
    let before_retry = git(path, &["fsck", "--unreachable", "--no-reflogs"]);
    let commit = before_retry
        .lines()
        .find_map(|line| line.strip_prefix("unreachable commit "))
        .expect("commit created before checkpoint record write");
    let intent = f.service.root.operation(op).unwrap().unwrap();
    let timestamp = intent.created_at_ms / 1000;
    assert_eq!(
        intent.payload["params"]["commit"],
        checkpoint.params["commit"]
    );

    drop(f.service);
    let state = f.temp.path().join("state");
    let service = reopen(&state);
    let mut changed = checkpoint.clone();
    changed.params["commit"]["author"]["email"] = json!("other@example.test");
    assert_eq!(
        service.execute(changed).error.unwrap().code,
        "idempotency_conflict"
    );
    let done = success(&service, checkpoint.clone());
    assert_eq!(done["source"], oid(commit));
    assert_eq!(done["retention_handle"], op);
    assert_eq!(success(&service, checkpoint.clone()), done);
    assert_checkpoint_history(path, commit, &parent, &original);
    assert_split_identity(path, commit, timestamp);
    assert_eq!(git(path, &["show", "HEAD:file.txt"]), "checkpoint changes");
    assert!(git(path, &["status", "--porcelain"]).is_empty());
}

#[test]
fn checkpoint_rejects_invalid_author_and_committer() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    std::fs::write(path.join("file.txt"), "dirty work\n").unwrap();
    let params = split_commit(&w, &f.base);
    for (pointer, invalid) in [
        ("/commit/author", json!({"name": "Missing email"})),
        (
            "/commit/author",
            json!({"name": "Task Author", "email": "task@example.test", "date": 0}),
        ),
        ("/commit/author/name", json!("")),
        ("/commit/author/email", json!("bad\raddress")),
        ("/commit/name", json!("bad\nname")),
        ("/commit/email", json!("bad<address>")),
    ] {
        let mut params = params.clone();
        *params.pointer_mut(pointer).unwrap() = invalid;
        let req = request(&f.repository, "workspace.checkpoint", params);
        let op = req.operation_id.clone().unwrap();
        let result = f.service.execute(req);
        assert_eq!(result.error.unwrap().code, "invalid_request", "{pointer}");
        assert!(f.service.root.retention(&op).unwrap().is_none());
    }
    assert_eq!(git(path, &["rev-parse", "HEAD"]), f.base);
    assert_eq!(git(path, &["status", "--porcelain"]), "M file.txt");
    assert_eq!(
        std::fs::read_to_string(path.join("file.txt")).unwrap(),
        "dirty work\n"
    );
}

fn assert_split_identity(path: &Path, commit: &str, timestamp: i64) {
    assert_eq!(
        git(
            path,
            &[
                "show",
                "-s",
                "--format=%an <%ae> %at%n%cn <%ce> %ct",
                commit
            ]
        ),
        format!(
            "Task Author <task@example.test> {timestamp}\nWorker <worker@example.test> {timestamp}"
        )
    );
}

fn split_commit(w: &serde_json::Value, head: &str) -> serde_json::Value {
    json!({
        "workspace_id": w["workspace_id"],
        "expected_generation": 1,
        "expected_head": oid(head),
        "quiescent": true,
        "mode": "all",
        "commit": {
            "name": "Worker",
            "email": "worker@example.test",
            "message": "checkpoint",
            "author": {"name": "Task Author", "email": "task@example.test"}
        }
    })
}
