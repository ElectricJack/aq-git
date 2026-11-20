mod support;
use serde_json::json;
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};
use support::*;

#[test]
fn private_workspaces_checkpoint_modes_and_safe_retirement() {
    let f = Fixture::new();
    let a = f.create_workspace();
    let b = f.create_workspace();
    let path = Path::new(a["path"].as_str().unwrap());
    let other = Path::new(b["path"].as_str().unwrap());
    std::fs::write(path.join("file.txt"), "agent a\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(other.join("file.txt")).unwrap(),
        "base\n"
    );
    let params = json!({"workspace_id":a["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true});
    let refused = f.service.execute(request(
        &f.repository,
        "workspace.checkpoint",
        params.clone(),
    ));
    assert_eq!(refused.error.unwrap().code, "workspace_dirty");
    std::fs::write(path.join("script"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path.join("script"), std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink("file.txt", path.join("link")).unwrap();
    std::fs::write(path.join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(path.join("ignored"), "build output").unwrap();
    let mut all = params;
    all["mode"] = json!("all");
    all["commit"] = json!({"name":"Agent","email":"agent@example.test","message":"checkpoint"});
    let checkpoint_request = request(&f.repository, "workspace.checkpoint", all);
    let checkpoint = success(&f.service, checkpoint_request.clone());
    assert_eq!(checkpoint, success(&f.service, checkpoint_request));
    let head = checkpoint["source"]["hex"].as_str().unwrap();
    assert_eq!(
        git(path, &["show", "-s", "--format=%an <%ae>%n%cn <%ce>", head]),
        "Agent <agent@example.test>\nAgent <agent@example.test>"
    );
    assert_eq!(git(path, &["status", "--porcelain"]), "");
    assert!(git(path, &["ls-tree", head, "script"]).starts_with("100755"));
    assert!(git(path, &["ls-tree", head, "link"]).starts_with("120000"));
    assert!(git(path, &["ls-tree", head, "ignored"]).is_empty());
    std::fs::write(path.join("new"), "unretained").unwrap();
    let retire = request(
        &f.repository,
        "workspace.retire",
        json!({"workspace_id":a["workspace_id"],"expected_generation":2,"expected_head":oid(head),"quiescent":true}),
    );
    let refusal = f.service.execute(retire);
    assert_eq!(refusal.error.unwrap().code, "workspace_dirty");
    assert!(path.exists());
    std::fs::remove_file(path.join("new")).unwrap();
    let done = success(
        &f.service,
        request(
            &f.repository,
            "workspace.retire",
            json!({"workspace_id":a["workspace_id"],"expected_generation":2,"expected_head":oid(head),"quiescent":true}),
        ),
    );
    assert_eq!(done["lifecycle"], "retired");
    assert!(!path.exists());
    assert!(other.exists());
    let retention = f
        .service
        .root
        .retention(checkpoint["retention_handle"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(retention.dependencies.is_empty());
    assert_eq!(retention.state, aq_git_core::state::RetentionState::Active);
}

#[test]
fn checkpoint_split_author_preserves_history_and_retries_after_restart() {
    use aq_git_core::{RepositoryService, state::WriteFault};
    use std::time::{Duration, Instant};

    let f = Fixture::new();
    assert_eq!(f.service.capabilities()["git"]["checkpoint_author"], true);
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
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

    let mut params = json!({
        "workspace_id": w["workspace_id"],
        "expected_generation": 1,
        "expected_head": oid(&parent),
        "quiescent": true,
        "mode": "all",
        "commit": {
            "name": "Worker",
            "email": "worker@example.test",
            "message": "checkpoint",
            "author": {"name": "Task Author", "email": "task@example.test"}
        }
    });
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
    // A parallel test's Git child can inherit the owner lock until exec closes its copy.
    let deadline = Instant::now() + Duration::from_secs(2);
    let service = loop {
        match RepositoryService::open(&state) {
            Err(error) if error.code == "state_busy" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => break result.expect("reopen checkpoint state after dropping the owner"),
        }
    };
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
    assert_eq!(git(path, &["rev-parse", "HEAD"]), commit);
    assert_eq!(git(path, &["rev-parse", "HEAD^"]), parent);
    assert_eq!(git(path, &["cat-file", "commit", &parent]), original);
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
    assert_eq!(git(path, &["show", "HEAD:file.txt"]), "checkpoint changes");
    assert!(git(path, &["status", "--porcelain"]).is_empty());
}

#[test]
fn checkpoint_rejects_invalid_author_and_committer() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    std::fs::write(path.join("file.txt"), "dirty work\n").unwrap();
    let params = json!({
        "workspace_id": w["workspace_id"],
        "expected_generation": 1,
        "expected_head": oid(&f.base),
        "quiescent": true,
        "mode": "all",
        "commit": {
            "name": "Worker",
            "email": "worker@example.test",
            "message": "checkpoint",
            "author": {"name": "Task Author", "email": "task@example.test"}
        }
    });
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

#[test]
fn rejects_unquiescent_changed_head_and_managed_metadata() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    let r=f.service.execute(request(&f.repository,"workspace.checkpoint",json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":false})));
    assert_eq!(r.error.unwrap().code, "workspace_changed");
    std::fs::write(path.join(".git/config"), "[core]\nworktree=/tmp\n").unwrap();
    let r = f.service.execute(request(
        &f.repository,
        "workspace.inspect",
        json!({"workspace_id":w["workspace_id"]}),
    ));
    assert_eq!(r.error.unwrap().code, "workspace_changed");
}

#[test]
fn checkpoint_recovery_removes_recorded_predecessor_after_workspace_update() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let w = f.create_workspace();
    let wid = w["workspace_id"].as_str().unwrap();
    let first = success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":wid,"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let previous = first["retention_handle"].as_str().unwrap();
    let next = request(
        &f.repository,
        "workspace.checkpoint",
        json!({"workspace_id":wid,"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true}),
    );
    let next_id = next.operation_id.clone().unwrap();
    f.service
        .root
        .inject_write_failure_after(6, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(next.clone()).error.unwrap().code,
        "io_error"
    );
    let record: serde_json::Value = f
        .service
        .root
        .read_json(format!(
            "repositories/{}/workspaces/{wid}.json",
            f.repository
        ))
        .unwrap()
        .unwrap();
    assert_eq!(record["last_checkpoint"], next_id);
    assert!(
        f.service
            .root
            .retention(previous)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
    assert!(
        f.service
            .root
            .retention(&next_id)
            .unwrap()
            .unwrap()
            .dependencies
            .contains(wid)
    );
    let path = Path::new(w["path"].as_str().unwrap());
    std::fs::write(path.join("later-work"), "preserved staged content").unwrap();
    git(path, &["add", "later-work"]);
    let staged = git(path, &["write-tree"]);
    let done = success(&f.service, next);
    assert_eq!(git(path, &["write-tree"]), staged);
    assert_eq!(done["workspace_generation"], 3);
    assert!(
        f.service
            .root
            .retention(previous)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
}
#[test]
fn unsupported_checkpoint_tree_preserves_primary_head_index_and_files() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    let original_index = git(path, &["write-tree"]);
    std::fs::write(path.join(".gitattributes"), "* filter=unsafe\n").unwrap();
    std::fs::write(path.join("file.txt"), "new work\n").unwrap();
    let response=f.service.execute(request(&f.repository,"workspace.checkpoint",json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true,"mode":"all","commit":{"name":"Agent","email":"a@example.test","message":"attributes"}})));
    assert_eq!(response.error.unwrap().code, "unsupported_feature");
    assert_eq!(git(path, &["rev-parse", "HEAD"]), f.base);
    assert_eq!(git(path, &["write-tree"]), original_index);
    assert_eq!(
        std::fs::read_to_string(path.join("file.txt")).unwrap(),
        "new work\n"
    );
}
#[test]
fn allocation_recovery_never_recreates_missing_recorded_pin_or_adopts_modified_config() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let create = request(
        &f.repository,
        "workspace.create",
        json!({"base":oid(&f.base)}),
    );
    let wid = create.operation_id.clone().unwrap();
    f.service
        .root
        .inject_write_failure_after(6, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(create.clone()).error.unwrap().code,
        "io_error"
    );
    let path = f.service.root.path().join("workspaces").join(&wid);
    assert!(path.exists());
    let reference = format!("refs/aq-git/workspaces/{wid}");
    f.service
        .repositories
        .git(
            &f.repository,
            vec![
                "update-ref".into(),
                "-d".into(),
                reference.clone(),
                f.base.clone(),
            ],
        )
        .unwrap();
    assert_eq!(
        f.service.execute(create.clone()).error.unwrap().code,
        "recovery_blocked"
    );
    assert!(
        !f.service
            .repositories
            .pool_path(&f.repository)
            .join(&reference)
            .exists()
    );
    f.service
        .repositories
        .git(
            &f.repository,
            vec!["update-ref".into(), reference, f.base.clone()],
        )
        .unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path.join(".git/config"))
        .unwrap()
        .write_all(b"\n[core]\nworktree=/tmp\n")
        .unwrap();
    assert_eq!(
        f.service.execute(create).error.unwrap().code,
        "workspace_changed"
    );
    assert!(path.exists());
}
#[test]
fn retirement_recovers_after_borrow_release_and_refuses_changed_pre_move_files() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let w = f.create_workspace();
    let wid = w["workspace_id"].as_str().unwrap();
    success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":wid,"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let retire = request(
        &f.repository,
        "workspace.retire",
        json!({"workspace_id":wid,"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true}),
    );
    f.service
        .root
        .inject_write_failure_after(5, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(retire.clone()).error.unwrap().code,
        "io_error"
    );
    assert_eq!(
        f.service.root.retention(wid).unwrap().unwrap().state,
        aq_git_core::state::RetentionState::Released
    );
    assert_eq!(success(&f.service, retire)["lifecycle"], "retired");
    let next = f.create_workspace();
    let next_id = next["workspace_id"].as_str().unwrap();
    success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":next_id,"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let retire = request(
        &f.repository,
        "workspace.retire",
        json!({"workspace_id":next_id,"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true}),
    );
    f.service
        .root
        .inject_write_failure_after(1, WriteFault::AfterRename);
    assert_eq!(
        f.service.execute(retire.clone()).error.unwrap().code,
        "io_error"
    );
    let path = Path::new(next["path"].as_str().unwrap());
    std::fs::write(path.join("new-work"), "preserve me").unwrap();
    assert_eq!(
        f.service.execute(retire).error.unwrap().code,
        "workspace_changed"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("new-work")).unwrap(),
        "preserve me"
    );
}
#[test]
fn pending_checkpoint_preserves_new_staging_and_can_be_explicitly_cancelled() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    std::fs::write(path.join("file.txt"), "frozen checkpoint work\n").unwrap();
    let checkpoint = request(
        &f.repository,
        "workspace.checkpoint",
        json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true,"mode":"all","commit":{"name":"Agent","email":"a@example.test","message":"frozen"}}),
    );
    let handle = checkpoint.operation_id.clone().unwrap();
    f.service
        .root
        .inject_write_failure_after(3, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(checkpoint.clone()).error.unwrap().code,
        "io_error"
    );
    let record = f.service.root.retention(&handle).unwrap().unwrap();
    assert!(record.dependencies.is_empty());
    std::fs::write(path.join("later"), "new staged work\n").unwrap();
    git(path, &["add", "later"]);
    let staged = git(path, &["write-tree"]);
    assert_eq!(
        f.service.execute(checkpoint.clone()).error.unwrap().code,
        "recovery_blocked"
    );
    assert_eq!(git(path, &["write-tree"]), staged);
    let released = success(
        &f.service,
        request(
            &f.repository,
            "retention.release",
            json!({"handle_id":handle,"expected_generation":record.generation,"refs":record.refs,"reason":"cancel interrupted local checkpoint"}),
        ),
    );
    assert_eq!(released["state"], "released");
    let spawns = f.service.repositories.runner.metrics().spawns;
    assert_eq!(
        f.service.execute(checkpoint).error.unwrap().code,
        "retention_released"
    );
    assert_eq!(f.service.repositories.runner.metrics().spawns, spawns);
    assert_eq!(git(path, &["write-tree"]), staged);
}

#[test]
fn whitespace_attributes_preserve_checkout_bytes_and_checkpoint() {
    let mut f = Fixture::new();
    let attributes = "# Evidence diagnostics only\ndocs/*.patch whitespace=-blank-at-eol\ndocs/*.log whitespace=-blank-at-eof\n";
    std::fs::write(f.source.join(".gitattributes"), attributes).unwrap();
    let bytes = b"unchanged \r\n";
    std::fs::write(f.source.join("file.txt"), bytes).unwrap();
    git(&f.source, &["add", "."]);
    git(&f.source, &["commit", "-m", "whitespace diagnostics"]);
    git(&f.source, &["push", f.remote.to_str().unwrap(), "main"]);
    f.base = git(&f.source, &["rev-parse", "HEAD"]);
    success(
        &f.service,
        request(
            &f.repository,
            "repo.sync",
            json!({"refs":["refs/heads/main"]}),
        ),
    );
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    assert_eq!(std::fs::read(path.join("file.txt")).unwrap(), bytes);
    assert_eq!(
        std::fs::read_to_string(path.join(".gitattributes")).unwrap(),
        attributes
    );
    std::fs::write(path.join("new.txt"), "work\n").unwrap();
    success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true,"mode":"all","commit":{"name":"Agent","email":"a@example.test","message":"retain metadata"}}),
        ),
    );
    assert_eq!(
        std::fs::read_to_string(path.join(".gitattributes")).unwrap(),
        attributes
    );
}

#[test]
fn workspace_snapshot_includes_untracked_and_digest_is_stable() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    std::fs::write(path.join("file.txt"), "dirty tracked edit\n").unwrap();
    std::fs::create_dir(path.join("new")).unwrap();
    std::fs::write(path.join("new/untracked.txt"), "untracked work\n").unwrap();
    std::fs::write(path.join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(path.join("ignored"), "build output").unwrap();
    let status_before = git(path, &["status", "--porcelain", "--untracked-files=all"]);
    let params = json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true});
    let first_request = request(&f.repository, "workspace.snapshot", params.clone());
    let first = success(&f.service, first_request.clone());
    assert_eq!(
        first,
        success(&f.service, first_request),
        "replay is idempotent"
    );
    let second = success(
        &f.service,
        request(&f.repository, "workspace.snapshot", params),
    );
    assert_ne!(first["artifact_handle"], second["artifact_handle"]);
    let digest = first["bundle_sha256"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert_eq!(digest, second["bundle_sha256"].as_str().unwrap());
    assert_eq!(first["snapshot"], second["snapshot"]);
    assert_eq!(first["tree"], second["tree"]);
    assert_eq!(first["bundle_size"], second["bundle_size"]);
    let state = f.temp.path().join("state");
    let bundle = |result: &serde_json::Value| state.join(result["bundle"].as_str().unwrap());
    let bytes = std::fs::read(bundle(&first)).unwrap();
    assert_eq!(bytes, std::fs::read(bundle(&second)).unwrap());
    use sha2::Digest;
    assert_eq!(hex::encode(sha2::Sha256::digest(&bytes)), digest);
    // The bundle restores the dirty and untracked content on top of the base.
    let restore = f.temp.path().join("restore");
    git(
        f.temp.path(),
        &[
            "clone",
            "-q",
            f.remote.to_str().unwrap(),
            restore.to_str().unwrap(),
        ],
    );
    let bundle_path = bundle(&first);
    git(
        &restore,
        &["bundle", "verify", bundle_path.to_str().unwrap()],
    );
    git(
        &restore,
        &[
            "fetch",
            "-q",
            bundle_path.to_str().unwrap(),
            "refs/aq-git/snapshot:refs/snapshot",
        ],
    );
    let snapshot = first["snapshot"]["hex"].as_str().unwrap();
    assert_eq!(git(&restore, &["rev-parse", "refs/snapshot"]), snapshot);
    assert_eq!(
        git(
            &restore,
            &["show", &format!("{snapshot}:new/untracked.txt")]
        ),
        "untracked work"
    );
    assert_eq!(
        git(&restore, &["show", &format!("{snapshot}:file.txt")]),
        "dirty tracked edit"
    );
    assert!(git(&restore, &["ls-tree", snapshot, "ignored"]).is_empty());
    // The workspace HEAD, index and files are unchanged.
    assert_eq!(git(path, &["rev-parse", "HEAD"]), f.base);
    assert_eq!(
        git(path, &["status", "--porcelain", "--untracked-files=all"]),
        status_before
    );
    assert!(
        git(path, &["for-each-ref"])
            .lines()
            .all(|line| !line.contains("aq-git/snapshot"))
    );
}

fn retain_ref(f: &Fixture, name: &str) -> aq_git_protocol::Response {
    f.service.execute(request(
        &f.repository,
        "repo.retain_ref",
        json!({"ref":name}),
    ))
}
fn handle_state(f: &Fixture, handle: &serde_json::Value) -> serde_json::Value {
    success(
        &f.service,
        request(
            &f.repository,
            "retention.inspect",
            json!({"handle_id":handle}),
        ),
    )
}

#[test]
fn retain_ref_pins_synced_head_replays_and_reuses_unchanged_handle() {
    let f = Fixture::new();
    let first_request = request(
        &f.repository,
        "repo.retain_ref",
        json!({"ref":"refs/heads/main"}),
    );
    let first = success(&f.service, first_request.clone());
    assert_eq!(first["ref"], "refs/heads/main");
    assert_eq!(first["head"], oid(&f.base));
    assert!(first["synced_at_ms"].as_i64().unwrap() > 0);
    let handle = first["retention_handle"].as_str().unwrap();
    let record = handle_state(&f, &first["retention_handle"]);
    assert_eq!(record["state"], "active");
    assert_eq!(record["refs"][0]["oid"], f.base);
    // The handle holds the head in the central pool.
    let pinned = f
        .service
        .repositories
        .git(
            &f.repository,
            vec![
                "rev-parse".into(),
                record["refs"][0]["name"].as_str().unwrap().into(),
            ],
        )
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&pinned).trim(), f.base);
    // Replay with the same operation id returns the same answer.
    assert_eq!(first, success(&f.service, first_request.clone()));
    // A new operation with an unchanged head reuses the live handle.
    let again_request = request(
        &f.repository,
        "repo.retain_ref",
        json!({"ref":"refs/heads/main"}),
    );
    let again = success(&f.service, again_request.clone());
    assert_eq!(again["retention_handle"], handle);
    assert_eq!(again["head"], first["head"]);
    assert!(again["synced_at_ms"].as_i64().unwrap() > first["synced_at_ms"].as_i64().unwrap());
    assert_eq!(first, success(&f.service, first_request));
    assert_eq!(again, success(&f.service, again_request));
    // A moved remote head pins a new handle; the old one stays live.
    std::fs::write(f.source.join("file.txt"), "next\n").unwrap();
    git(&f.source, &["commit", "-am", "next"]);
    git(&f.source, &["push", f.remote.to_str().unwrap(), "main"]);
    let next_head = git(&f.source, &["rev-parse", "HEAD"]);
    let moved = success(
        &f.service,
        request(
            &f.repository,
            "repo.retain_ref",
            json!({"ref":"refs/heads/main"}),
        ),
    );
    assert_eq!(moved["head"], oid(&next_head));
    assert_ne!(moved["retention_handle"], first["retention_handle"]);
    assert_eq!(
        handle_state(&f, &moved["retention_handle"])["refs"][0]["oid"],
        next_head
    );
    assert_eq!(
        handle_state(&f, &first["retention_handle"])["state"],
        "active"
    );
    // A released handle is not reused for the same head.
    let old = handle_state(&f, &moved["retention_handle"]);
    success(
        &f.service,
        request(
            &f.repository,
            "retention.release",
            json!({"handle_id":moved["retention_handle"],"expected_generation":old["generation"],"refs":old["refs"],"reason":"test"}),
        ),
    );
    let repinned = success(
        &f.service,
        request(
            &f.repository,
            "repo.retain_ref",
            json!({"ref":"refs/heads/main"}),
        ),
    );
    assert_eq!(repinned["head"], oid(&next_head));
    assert_ne!(repinned["retention_handle"], moved["retention_handle"]);
    assert_eq!(
        handle_state(&f, &repinned["retention_handle"])["state"],
        "active"
    );
}

#[test]
fn retain_ref_failed_fetch_does_not_advance_the_retained_observation() {
    let f = Fixture::new();
    let first_request = request(
        &f.repository,
        "repo.retain_ref",
        json!({"ref":"refs/heads/main"}),
    );
    let first = success(&f.service, first_request.clone());
    let retained_refs = f
        .temp
        .path()
        .join(format!("state/repositories/{}/retained-refs", f.repository));
    let state = std::fs::read_dir(retained_refs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let before = std::fs::read(&state).unwrap();
    std::fs::rename(&f.remote, f.remote.with_extension("offline")).unwrap();

    let failed = retain_ref(&f, "refs/heads/main");
    assert_eq!(failed.error.unwrap().code, "remote_unavailable");
    assert!(failed.result.is_none());
    assert_eq!(std::fs::read(&state).unwrap(), before);
    assert_eq!(success(&f.service, first_request), first);
}

#[test]
fn retain_ref_refuses_unlisted_ref_worker_binding_and_missing_remote_ref() {
    let f = Fixture::new();
    let not_allowed = retain_ref(&f, "refs/heads/other");
    assert_eq!(not_allowed.error.unwrap().code, "invalid_request");
    let malformed = retain_ref(&f, "main");
    assert_eq!(malformed.error.unwrap().code, "invalid_request");
    let unknown = f.service.execute(request(
        &f.repository,
        "repo.retain_ref",
        json!({"ref":"refs/heads/main","extra":true}),
    ));
    assert_eq!(unknown.error.unwrap().code, "invalid_request");

    let worker = uuid::Uuid::new_v4().to_string();
    let mut register = request(
        &worker,
        "repo.register",
        json!({"identity":"worker-fixture","mode":"worker","allowed_refs":["refs/heads/main"]}),
    );
    register.expected_binding_generation = Some(0);
    success(&f.service, register);
    let refused = f.service.execute(request(
        &worker,
        "repo.retain_ref",
        json!({"ref":"refs/heads/main"}),
    ));
    assert_eq!(refused.error.unwrap().code, "unsupported_feature");

    let absent = uuid::Uuid::new_v4().to_string();
    let mut register = request(
        &absent,
        "repo.register",
        json!({"binding":{"identity":"absent-fixture","mode":"central","allowed_refs":["refs/heads/missing"],"remote_url":f.remote,"allow_file_transport":true}}),
    );
    register.expected_binding_generation = Some(0);
    success(&f.service, register);
    let missing = f.service.execute(request(
        &absent,
        "repo.retain_ref",
        json!({"ref":"refs/heads/missing"}),
    ));
    assert_eq!(missing.error.unwrap().code, "object_unavailable");
}
