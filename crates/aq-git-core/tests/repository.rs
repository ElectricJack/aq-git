use aq_git_core::{git::GitRunner, repository::RepositoryManager, state::StateRoot};
use aq_git_protocol::Request;
use serde_json::{Value, json};
use std::{path::Path, process::Command, sync::Arc};
fn git(path: &Path, args: &[&str]) -> String {
    let o = Command::new("/usr/bin/git")
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "f@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "f@example.test")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().into()
}
fn request(id: &str, op: &str, params: Value) -> Request {
    Request {
        protocol_version: 1,
        request_id: "test".into(),
        operation: op.into(),
        repository_id: Some(id.into()),
        operation_id: Some(uuid::Uuid::new_v4().to_string()),
        expected_binding_generation: Some(if op == "repo.register" { 0 } else { 1 }),
        params,
        deadline_ms: None,
    }
}
fn oid(s: &str) -> Value {
    json!({"algorithm":"sha1","hex":s})
}
#[path = "repository/binding_recovery.rs"]
mod binding_recovery;
#[path = "repository/diff_patch.rs"]
mod diff_patch;
#[path = "repository/exact_reads.rs"]
mod exact_reads;
#[test]
fn registration_replay_and_binding_validation() {
    let t = tempfile::tempdir().unwrap();
    let m = RepositoryManager::new(
        Arc::new(StateRoot::open(t.path()).unwrap()),
        GitRunner::new("/usr/bin/git".into()),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let r = request(
        &id,
        "repo.register",
        json!({"identity":"fixture","mode":"worker","binding_generation":9}),
    );
    assert_eq!(m.handle(&r).unwrap()["binding_generation"], 9);
    assert_eq!(m.handle(&r).unwrap()["binding_generation"], 9);
    let mut q = request(&id, "repo.show", json!({}));
    assert_eq!(m.handle(&q).unwrap_err().code, "binding_conflict");
    q.expected_binding_generation = None;
    assert_eq!(m.handle(&q).unwrap()["binding_generation"], 9);
}
#[test]
fn drained_binding_changes_archive_and_refuse_live_work() {
    let t = tempfile::tempdir().unwrap();
    let root = Arc::new(StateRoot::open(t.path()).unwrap());
    let m = RepositoryManager::new(root.clone(), GitRunner::new("/usr/bin/git".into()));
    let id = uuid::Uuid::new_v4().to_string();
    m.handle(&request(
        &id,
        "repo.register",
        json!({"identity":"fixture","mode":"worker","binding_generation":2}),
    ))
    .unwrap();
    let mut binding = serde_json::to_value(m.binding(&id).unwrap()).unwrap();
    binding["binding_generation"] = json!(8);
    let mut update = request(
        &id,
        "repo.binding-update",
        json!({"binding":binding,"reason":"adopt assignment"}),
    );
    update.expected_binding_generation = Some(2);
    root.write_json(
        format!("repositories/{id}/workspaces/work.json"),
        &json!({"workspace_id":"work","lifecycle":"active"}),
    )
    .unwrap();
    let error = m.handle(&update).unwrap_err();
    assert_eq!(error.code, "binding_in_use");
    assert_eq!(error.details["blockers"][0]["id"], "work");
    assert_eq!(m.binding(&id).unwrap().binding_generation, 2);
    root.write_json(
        format!("repositories/{id}/workspaces/work.json"),
        &json!({"workspace_id":"work","lifecycle":"retired"}),
    )
    .unwrap();
    assert_eq!(m.handle(&update).unwrap()["binding_generation"], 8);
    assert_eq!(m.handle(&update).unwrap()["binding_generation"], 8);
    assert!(
        root.read_json::<Value>(format!("repositories/{id}/binding-history/2.json"))
            .unwrap()
            .is_some()
    );
    m.recover_bindings().unwrap();
    let mut bad = request(
        &id,
        "repo.binding-update",
        json!({"binding":m.binding(&id).unwrap(),"reason":"same generation"}),
    );
    bad.expected_binding_generation = Some(8);
    assert_eq!(m.handle(&bad).unwrap_err().code, "binding_conflict");
}
#[test]
fn tree_preserves_non_utf8_names_and_sync_cleans_staging() {
    use std::os::unix::ffi::OsStringExt;
    let t = tempfile::tempdir().unwrap();
    let remote = t.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(
        remote.join(std::ffi::OsString::from_vec(vec![b'a', 255])),
        "data",
    )
    .unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "bytes"]);
    let head = git(&remote, &["rev-parse", "HEAD"]);
    let root = Arc::new(StateRoot::open(t.path().join("state")).unwrap());
    let m = RepositoryManager::new(root, GitRunner::new("/usr/bin/git".into()));
    let id = uuid::Uuid::new_v4().to_string();
    m.handle(&request(&id,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}))).unwrap();
    let sync = request(
        &id,
        "repo.sync",
        json!({"min_started_at_ms":chrono::Utc::now().timestamp_millis()}),
    );
    m.handle(&sync).unwrap();
    assert!(git(&m.pool_path(&id), &["for-each-ref", "refs/aq-git/staging"]).is_empty());
    let q = request(&id, "tree", json!({"oid":oid(&head)}));
    assert_eq!(
        m.handle(&q).unwrap()["entries"][0]["path"],
        json!({"encoding":"hex","value":"61ff"})
    );
    let moved = t.path().join("remote-moved");
    std::fs::rename(&remote, &moved).unwrap();
    assert!(m.handle(&request(&id, "repo.sync", json!({}))).is_err());
    let observation = m
        .handle(&request(&id, "repo.observation", json!({})))
        .unwrap();
    assert_eq!(observation["refs"]["refs/heads/main"], head);
    assert!(observation["last_refresh_error"].is_object());
}
#[test]
fn equivalent_syncs_share_one_scoped_fetch() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let remote = t.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("a"), "data").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "sync"]);
    let marker = t.path().join("entered");
    let release = t.path().join("release");
    let wrapper = t.path().join("git-wrapper");
    std::fs::write(&wrapper,format!("#!/bin/sh\nstall=0\nfor arg do\nif [ \"$arg\" = '{}' ]; then stall=1; fi\ndone\nif [ \"$stall\" = 1 ]; then\n: > '{}'\nwhile [ ! -e '{}' ]; do /bin/sleep 0.01; done\nfi\nexec /usr/bin/git \"$@\"\n",remote.display(),marker.display(),release.display())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = GitRunner::new(wrapper);
    let root = Arc::new(StateRoot::open(t.path().join("state")).unwrap());
    let manager = Arc::new(RepositoryManager::new(root, runner.clone()));
    let id = uuid::Uuid::new_v4().to_string();
    manager.handle(&request(&id,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}))).unwrap();
    let before = runner.metrics().spawns;
    let barrier = Arc::new(std::sync::Barrier::new(33));
    let threads = (0..32)
        .map(|_| {
            let manager = manager.clone();
            let barrier = barrier.clone();
            let request = request(&id, "repo.sync", json!({}));
            std::thread::spawn(move || {
                barrier.wait();
                manager.handle(&request)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let entered = marker.exists();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(&release, "").unwrap();
    assert!(entered);
    let ids = threads
        .into_iter()
        .map(|thread| {
            thread.join().unwrap().unwrap()["observation_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 1);
    assert_eq!(runner.metrics().spawns - before, 4);
}
#[test]
fn unavailable_profiles_and_embedded_credentials_are_rejected_without_git() {
    let t = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    let manager =
        RepositoryManager::new(Arc::new(StateRoot::open(t.path()).unwrap()), runner.clone());
    for (extra, code) in [
        (json!({"credential_profile":"named"}), "unsupported_feature"),
        (json!({"feature_profile":"custom"}), "unsupported_feature"),
        (
            json!({"remote_url":"https://user:secret@example.test/repository"}),
            "invalid_request",
        ),
        (
            json!({"remote_url":"ssh://user:secret@example.test/repository"}),
            "invalid_request",
        ),
    ] {
        let mut params = json!({"identity":"fixture","mode":"central"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = manager
            .handle(&request(
                &uuid::Uuid::new_v4().to_string(),
                "repo.register",
                params,
            ))
            .unwrap_err();
        assert_eq!(error.code, code);
        assert!(!error.message.contains("secret"));
    }
    assert_eq!(runner.metrics().spawns, 0);
}
#[test]
fn delayed_admitted_sync_reuses_completion_and_minimum_can_require_refresh() {
    let t = tempfile::tempdir().unwrap();
    let remote = t.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("a"), "data").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "one"]);
    let runner = GitRunner::new("/usr/bin/git".into());
    let manager = RepositoryManager::new(
        Arc::new(StateRoot::open(t.path().join("state")).unwrap()),
        runner.clone(),
    );
    let id = uuid::Uuid::new_v4().to_string();
    manager.handle(&request(&id,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}))).unwrap();
    let scope = aq_git_core::repository::AdmissionScope::new();
    let first = manager
        .handle(&request(&id, "repo.sync", json!({})))
        .unwrap();
    let spawns = runner.metrics().spawns;
    let delayed = manager
        .handle(&request(&id, "repo.sync", json!({})))
        .unwrap();
    assert_eq!(first["observation_id"], delayed["observation_id"]);
    assert_eq!(spawns, runner.metrics().spawns);
    let minimum = first["started_at_ms"].as_i64().unwrap() + 1;
    let refreshed = manager
        .handle(&request(
            &id,
            "repo.sync",
            json!({"min_started_at_ms":minimum}),
        ))
        .unwrap();
    assert_ne!(first["observation_id"], refreshed["observation_id"]);
    assert_eq!(runner.metrics().spawns - spawns, 4);
    drop(scope);
    let after = manager
        .handle(&request(&id, "repo.sync", json!({})))
        .unwrap();
    assert_ne!(after["observation_id"], refreshed["observation_id"]);
}
#[test]
fn registration_does_not_contact_configured_remote_and_replay_launches_nothing() {
    let t = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    let manager = RepositoryManager::new(
        Arc::new(StateRoot::open(t.path().join("state")).unwrap()),
        runner.clone(),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let register = request(
        &id,
        "repo.register",
        json!({"identity":"fixture","mode":"central","remote_url":t.path().join("does-not-exist"),"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}),
    );
    let first = manager.handle(&register).unwrap();
    let spawns = runner.metrics().spawns;
    assert_eq!(first, manager.handle(&register).unwrap());
    assert_eq!(spawns, runner.metrics().spawns);
}
#[test]
fn failed_repository_reads_do_not_accumulate_gates_and_active_gate_stays_shared() {
    let t = tempfile::tempdir().unwrap();
    let manager = RepositoryManager::new(
        Arc::new(StateRoot::open(t.path()).unwrap()),
        GitRunner::new("/usr/bin/git".into()),
    );
    for _ in 0..256 {
        let missing = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            manager
                .handle(&request(&missing, "repo.show", json!({})))
                .unwrap_err()
                .code,
            "repository_not_found"
        );
    }
    assert!(
        manager.query_metrics()["coordination_entries"]
            .as_u64()
            .unwrap()
            <= 1
    );
    let held_id = uuid::Uuid::new_v4().to_string();
    let lifecycle = manager.repository_lifecycle(&held_id);
    let guard = lifecycle.write().unwrap();
    for _ in 0..256 {
        let missing = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            manager
                .handle(&request(&missing, "repo.show", json!({})))
                .unwrap_err()
                .code,
            "repository_not_found"
        );
    }
    assert!(
        manager.query_metrics()["coordination_entries"]
            .as_u64()
            .unwrap()
            <= 2
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reader = manager.clone();
    let query = request(&held_id, "repo.show", json!({}));
    let thread = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        result_tx.send(reader.handle(&query)).unwrap();
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err()
    );
    drop(guard);
    assert_eq!(
        result_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .code,
        "repository_not_found"
    );
    thread.join().unwrap();
}
