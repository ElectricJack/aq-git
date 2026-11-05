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
