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
#[test]
fn exact_reads_observations_and_warm_batches() {
    let t = tempfile::tempdir().unwrap();
    let remote = t.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("a"), "one").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "one"]);
    let first = git(&remote, &["rev-parse", "HEAD"]);
    std::fs::write(remote.join("a"), "two").unwrap();
    git(&remote, &["commit", "-am", "two"]);
    let second = git(&remote, &["rev-parse", "HEAD"]);
    let root = Arc::new(StateRoot::open(t.path().join("state")).unwrap());
    let runner = GitRunner::new("/usr/bin/git".into());
    let m = RepositoryManager::new(root, runner.clone());
    let id = uuid::Uuid::new_v4().to_string();
    m.handle(&request(&id,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/missing"]}))).unwrap();
    assert!(runner.metrics().spawns >= 1);
    let sync = request(&id, "repo.sync", json!({}));
    let obs = m.handle(&sync).unwrap();
    assert_eq!(obs["commit_graph_available"], true);
    let before = runner.metrics().spawns;
    assert_eq!(m.handle(&sync).unwrap(), obs);
    assert_eq!(runner.metrics().spawns, before);
    m.git(&id, vec!["commit-graph".into(), "verify".into()])
        .unwrap();
    let containment = request(
        &id,
        "contains",
        json!({"ancestor":oid(&first),"descendant":oid(&second)}),
    );
    let indexed = m.handle(&containment).unwrap();
    let graph_dir = m.pool_path(&id).join("objects/info/commit-graphs");
    std::fs::remove_dir_all(&graph_dir).unwrap();
    m.clear_cache();
    assert_eq!(m.handle(&containment).unwrap(), indexed);
    assert!(
        !graph_dir.exists(),
        "exact reads must not rebuild a derived index"
    );
    m.handle(&request(&id, "repo.sync", json!({}))).unwrap();
    m.git(&id, vec!["commit-graph".into(), "verify".into()])
        .unwrap();
    assert!(graph_dir.exists());
    m.clear_cache();
    assert_eq!(m.handle(&containment).unwrap(), indexed);
    assert_eq!(obs["refs"]["refs/heads/main"], second);
    assert!(obs["refs"].get("refs/heads/missing").is_none());
    let before = runner.metrics().spawns;
    m.handle(&request(
        &id,
        "repo.observation",
        json!({"refs":["refs/heads/missing"]}),
    ))
    .unwrap();
    assert_eq!(before, runner.metrics().spawns);
    let mut q = request(
        &id,
        "query.batch",
        json!({"items":[{"operation":"inspect","params":{"oid":oid(&first)}},{"operation":"contains","params":{"ancestor":oid(&first),"descendant":oid(&second)}},{"operation":"tree","params":{"oid":oid(&second)}},{"operation":"diff","params":{"base":oid(&first),"target":oid(&second)}}]}),
    );
    q.expected_binding_generation = None;
    let result = m.handle(&q).unwrap();
    assert_eq!(result["items"][1]["result"]["contains"], true);
    assert_eq!(
        result["items"][2]["result"]["entries"][0]["path"]["value"],
        "a"
    );
    let before = runner.metrics().spawns;
    let exchanges = m.query_metrics()["object_exchanges"].clone();
    assert_eq!(result, m.handle(&q).unwrap());
    assert_eq!(before, runner.metrics().spawns);
    assert_eq!(exchanges, m.query_metrics()["object_exchanges"]);
    m.clear_cache();
    let shared = Arc::new(m.clone());
    let barrier = Arc::new(std::sync::Barrier::new(32));
    let exchanges = m.query_metrics()["object_exchanges"].as_u64().unwrap();
    let handles = (0..32)
        .map(|_| {
            let manager = shared.clone();
            let barrier = barrier.clone();
            let query = request(&id, "inspect", json!({"oid":oid(&second)}));
            std::thread::spawn(move || {
                barrier.wait();
                manager.handle(&query).unwrap()
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        assert_eq!(handle.join().unwrap()["oid"], oid(&second));
    }
    assert_eq!(
        m.query_metrics()["object_exchanges"].as_u64().unwrap() - exchanges,
        1
    );
    let before = runner.metrics().spawns;
    let before_exchanges = m.query_metrics()["object_exchanges"].clone();
    let strict_batch=m.handle(&request(&id,"query.batch",json!({"items":[{"operation":"inspect","params":{"oid":oid(&second)},"unexpected":true},{"operation":"inspect","params":{"oid":oid(&second)}}]}))).unwrap();
    assert_eq!(strict_batch["items"][0]["ok"], false);
    assert_eq!(strict_batch["items"][0]["error"]["code"], "invalid_request");
    assert_eq!(strict_batch["items"][1]["result"]["oid"], oid(&second));
    assert_eq!(runner.metrics().spawns, before);
    assert_eq!(m.query_metrics()["object_exchanges"], before_exchanges);
    m.clear_cache();
    let duplicates = request(
        &id,
        "query.batch",
        json!({"items":vec![json!({"operation":"inspect","params":{"oid":oid(&second)}});100]}),
    );
    let initial_exchanges = m.query_metrics()["object_exchanges"].as_u64().unwrap();
    let duplicated = m.handle(&duplicates).unwrap();
    assert_eq!(duplicated["items"].as_array().unwrap().len(), 100);
    assert_eq!(
        m.query_metrics()["object_exchanges"].as_u64().unwrap() - initial_exchanges,
        1
    );
    assert_eq!(m.query_metrics()["cache_misses"], 1);
    assert_eq!(m.query_metrics()["cache_hits"], 99);
    let before = runner.metrics().spawns;
    let exchanges = m.query_metrics()["object_exchanges"].clone();
    assert_eq!(m.handle(&duplicates).unwrap(), duplicated);
    assert_eq!(runner.metrics().spawns, before);
    assert_eq!(m.query_metrics()["object_exchanges"], exchanges);
    assert_eq!(m.query_metrics()["cache_hits"], 199);
    let missing = request(
        &id,
        "contains",
        json!({"ancestor":oid(&"a".repeat(40)),"descendant":oid(&second)}),
    );
    assert_eq!(m.handle(&missing).unwrap_err().code, "object_unavailable");
    assert_eq!(m.handle(&missing).unwrap_err().code, "object_unavailable");
    let stale = request(&id, "repo.observation", json!({"max_age_ms":0}));
    assert_eq!(m.handle(&stale).unwrap_err().code, "observation_stale");
}
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

#[test]
fn interrupted_binding_transition_fences_new_admission_and_recovers_cached_old_authority() {
    use aq_git_core::{
        RepositoryService,
        state::{OperationStatus, WriteFault},
    };
    for (point, writes, expected_disk, restart) in [
        (WriteFault::AfterRename, 1, 1, false),
        (WriteFault::BeforeRename, 3, 1, true),
        (WriteFault::AfterRename, 3, 4, false),
        (WriteFault::AfterRename, 4, 4, false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut service = RepositoryService::open(temp.path().join("state")).unwrap();
        let repo = uuid::Uuid::new_v4().to_string();
        let register = request(
            &repo,
            "repo.register",
            json!({"identity":"fenced-worker","mode":"worker","allowed_refs":["refs/heads/main"]}),
        );
        let registered = service.execute(register.clone());
        assert!(registered.ok, "{:?}", registered.error);
        let mut replacement = registered.result.unwrap();
        service.root.write_json(format!("repositories/{repo}/observation.json"),&json!({"binding_generation":1,"covered_refs":["refs/heads/main"],"refs":{},"completed_at_ms":chrono::Utc::now().timestamp_millis()})).unwrap();
        assert!(
            service
                .execute(request(&repo, "repo.observation", json!({})))
                .ok
        );
        replacement["binding_generation"] = json!(4);
        let update = request(
            &repo,
            "repo.binding-update",
            json!({"binding":replacement,"reason":"interrupted drained generation adoption"}),
        );
        let operation = update.operation_id.clone().unwrap();
        service.root.inject_write_failure_after(writes, point);
        let failed = service.execute(update.clone());
        assert_eq!(failed.error.unwrap().code, "io_error");
        assert_eq!(
            service.root.operation(&operation).unwrap().unwrap().status,
            OperationStatus::Pending
        );
        let disk: Value = service
            .root
            .read_json(format!("repositories/{repo}/binding.json"))
            .unwrap()
            .unwrap();
        assert_eq!(disk["binding_generation"], expected_disk);
        let spawns = service.repositories.runner.metrics().spawns;
        for generation in [1, 4] {
            let mut refused = request(
                &repo,
                "workspace.create",
                json!({"base":oid(&"a".repeat(40))}),
            );
            refused.expected_binding_generation = Some(generation);
            let id = refused.operation_id.clone().unwrap();
            let response = service.execute(refused);
            assert_eq!(response.error.unwrap().code, "recovery_blocked");
            assert!(
                service.root.operation(&id).unwrap().is_none(),
                "rejected admission must not create a pending operation"
            );
        }
        let historical = service.execute(register);
        assert!(historical.ok);
        assert_eq!(historical.result.unwrap()["binding_generation"], 1);
        assert_eq!(service.repositories.runner.metrics().spawns, spawns);
        let unrelated = uuid::Uuid::new_v4().to_string();
        assert!(
            service
                .execute(request(
                    &unrelated,
                    "repo.register",
                    json!({"identity":"unrelated-worker","mode":"worker"})
                ))
                .ok
        );
        if restart {
            let root = service.root.as_ref().clone();
            drop(service);
            service = RepositoryService::from_state_root(root).unwrap();
            assert_eq!(
                service.root.operation(&operation).unwrap().unwrap().status,
                OperationStatus::Complete
            );
        }
        let replay = service.execute(update.clone());
        assert!(replay.ok, "{:?}", replay.error);
        assert_eq!(replay.result.unwrap()["binding_generation"], 4);
        assert_eq!(
            service.root.operation(&operation).unwrap().unwrap().status,
            OperationStatus::Complete
        );
        let mut show = request(&repo, "repo.show", json!({}));
        show.expected_binding_generation = Some(4);
        let current = service.execute(show);
        assert!(current.ok);
        let mut next = current.result.unwrap();
        let mut observation = request(&repo, "repo.observation", json!({}));
        observation.expected_binding_generation = Some(4);
        assert_eq!(
            service.execute(observation).error.unwrap().code,
            "observation_unavailable"
        );
        next["binding_generation"] = json!(8);
        let mut later = request(
            &repo,
            "repo.binding-update",
            json!({"binding":next,"reason":"later unrelated transition must not inherit old fence"}),
        );
        later.expected_binding_generation = Some(4);
        let done = service.execute(later);
        assert!(done.ok, "{:?}", done.error);
        assert_eq!(done.result.unwrap()["binding_generation"], 8);
    }
}
#[test]
fn binding_transition_pre_intent_failure_proves_old_authority_and_keeps_retry_id() {
    use aq_git_core::{
        RepositoryService,
        state::{OperationStatus, WriteFault},
    };
    let temp = tempfile::tempdir().unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    let registered = service.execute(request(
        &repo,
        "repo.register",
        json!({"identity":"preintent-worker","mode":"worker"}),
    ));
    assert!(registered.ok);
    let mut binding = registered.result.unwrap();
    binding["binding_generation"] = json!(2);
    let update = request(
        &repo,
        "repo.binding-update",
        json!({"binding":binding,"reason":"retry unpublished intent"}),
    );
    let operation = update.operation_id.clone().unwrap();
    service
        .root
        .inject_write_failure_after(1, WriteFault::BeforeRename);
    assert_eq!(
        service.execute(update.clone()).error.unwrap().code,
        "io_error"
    );
    assert!(
        service
            .root
            .read_json::<Value>(format!(
                "repositories/{repo}/binding-transitions/{operation}.json"
            ))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        service.root.operation(&operation).unwrap().unwrap().status,
        OperationStatus::Pending
    );
    let old = service.execute(request(&repo, "repo.show", json!({})));
    assert!(old.ok);
    assert_eq!(old.result.unwrap()["binding_generation"], 1);
    let retried = service.execute(update);
    assert!(retried.ok, "{:?}", retried.error);
    assert_eq!(retried.result.unwrap()["binding_generation"], 2);
}
