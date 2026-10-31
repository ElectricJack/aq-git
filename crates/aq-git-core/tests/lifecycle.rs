mod support;
use serde_json::json;
use support::*;

#[test]
fn retirement_release_replay_and_generation_transition_preserve_history() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let checkpoint_request = request(
        &f.repository,
        "workspace.checkpoint",
        json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
    );
    let checkpoint = success(&f.service, checkpoint_request.clone());
    let handle = checkpoint["retention_handle"].as_str().unwrap();
    let before = success(
        &f.service,
        request(
            &f.repository,
            "retention.inspect",
            json!({"handle_id":handle}),
        ),
    );
    let blocked=f.service.execute(request(&f.repository,"retention.release",json!({"handle_id":handle,"expected_generation":before["generation"],"refs":before["refs"],"reason":"test release"})));
    assert_eq!(blocked.error.unwrap().code, "retention_in_use");
    success(
        &f.service,
        request(
            &f.repository,
            "workspace.retire",
            json!({"workspace_id":w["workspace_id"],"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let current = success(
        &f.service,
        request(
            &f.repository,
            "retention.inspect",
            json!({"handle_id":handle}),
        ),
    );
    let release = request(
        &f.repository,
        "retention.release",
        json!({"handle_id":handle,"expected_generation":current["generation"],"refs":current["refs"],"reason":"central receipt accepted"}),
    );
    let released = success(&f.service, release.clone());
    assert_eq!(released["state"], "released");
    assert_eq!(released, success(&f.service, release));
    let replay = f.service.execute(checkpoint_request.clone());
    assert!(replay.ok);
    assert_eq!(replay.result.unwrap(), checkpoint);
    assert_eq!(replay.resource_status.unwrap()["state"], "released");
    let mut binding = success(&f.service, request(&f.repository, "repo.show", json!({})));
    binding["binding_generation"] = json!(2);
    let updated = success(
        &f.service,
        request(
            &f.repository,
            "repo.binding-update",
            json!({"binding":binding,"reason":"next generation"}),
        ),
    );
    assert!(updated.to_string().contains('2'));
    let replay = f.service.execute(checkpoint_request);
    assert!(
        replay.ok,
        "historical replay must not use current binding precondition"
    );
    assert_eq!(replay.resource_status.unwrap()["state"], "released");
}

#[test]
fn pending_use_blocks_release_and_changed_refs_fail_closed() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let cp = success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let handle = cp["retention_handle"].as_str().unwrap();
    let record = f.service.root.retention(handle).unwrap().unwrap();
    let wrong=f.service.execute(request(&f.repository,"retention.release",json!({"handle_id":handle,"expected_generation":record.generation,"refs":[],"reason":"incorrect expectation"})));
    assert_eq!(wrong.error.unwrap().code, "retention_conflict");
    let forbidden = f.service.execute(request(
        &f.repository,
        "repo.cache-evict",
        json!({"expected_storage_generation":1,"reconstructible":true,"reason":"disk pressure"}),
    ));
    assert_eq!(forbidden.error.unwrap().code, "unsupported_feature");
    let unknown=f.service.execute(request(&f.repository,"workspace.retire",json!({"workspace_id":w["workspace_id"],"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true,"force":true})));
    assert!(!unknown.ok);
}
fn register_worker(f: &Fixture) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let mut register = request(
        &id,
        "repo.register",
        json!({"identity":"worker-fixture","mode":"worker","binding_generation":1}),
    );
    register.expected_binding_generation = Some(0);
    success(&f.service, register);
    f.service
        .repositories
        .git(
            &id,
            vec![
                "fetch".into(),
                "--no-tags".into(),
                f.remote.to_string_lossy().into_owned(),
                f.base.clone(),
            ],
        )
        .unwrap();
    id
}
#[test]
fn nested_release_fields_and_missing_active_refs_fail_without_release_intent() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let cp = success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let handle = cp["retention_handle"].as_str().unwrap();
    let mut record = f.service.root.retention(handle).unwrap().unwrap();
    let mut refs = serde_json::to_value(&record.refs).unwrap();
    refs[0]["force"] = json!(true);
    let unknown=f.service.execute(request(&f.repository,"retention.release",json!({"handle_id":handle,"expected_generation":record.generation,"refs":refs,"reason":"must reject unknown nested field"})));
    assert_eq!(unknown.error.unwrap().code, "invalid_request");
    success(
        &f.service,
        request(
            &f.repository,
            "workspace.retire",
            json!({"workspace_id":w["workspace_id"],"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    record = f.service.root.retention(handle).unwrap().unwrap();
    f.service
        .repositories
        .git(
            &f.repository,
            vec![
                "update-ref".into(),
                "-d".into(),
                record.refs[0].name.clone(),
                record.refs[0].oid.clone(),
            ],
        )
        .unwrap();
    let missing=f.service.execute(request(&f.repository,"retention.release",json!({"handle_id":handle,"expected_generation":record.generation,"refs":record.refs,"reason":"must preserve corruption evidence"})));
    assert_eq!(missing.error.unwrap().code, "recovery_blocked");
    assert_eq!(f.service.root.retention(handle).unwrap().unwrap(), record);
    let report = success(&f.service, request(&f.repository, "doctor", json!({})));
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "retention_ref_mismatch")
    );
}
#[test]
fn release_recovers_missing_refs_only_under_matching_durable_intent() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let w = f.create_workspace();
    let cp = success(
        &f.service,
        request(
            &f.repository,
            "workspace.checkpoint",
            json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    success(
        &f.service,
        request(
            &f.repository,
            "workspace.retire",
            json!({"workspace_id":w["workspace_id"],"expected_generation":2,"expected_head":oid(&f.base),"quiescent":true}),
        ),
    );
    let handle = cp["retention_handle"].as_str().unwrap();
    let record = f.service.root.retention(handle).unwrap().unwrap();
    let release = request(
        &f.repository,
        "retention.release",
        json!({"handle_id":handle,"expected_generation":record.generation,"refs":record.refs,"reason":"interrupted release"}),
    );
    f.service
        .root
        .inject_write_failure_after(2, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(release.clone()).error.unwrap().code,
        "io_error"
    );
    assert_eq!(
        f.service.root.retention(handle).unwrap().unwrap().state,
        aq_git_core::state::RetentionState::Releasing
    );
    let blob = git(&f.source, &["rev-parse", "HEAD:file.txt"]);
    f.service
        .repositories
        .git(
            &f.repository,
            vec![
                "update-ref".into(),
                record.refs[0].name.clone(),
                blob.clone(),
            ],
        )
        .unwrap();
    assert_eq!(
        f.service.execute(release.clone()).error.unwrap().code,
        "recovery_blocked"
    );
    assert_eq!(
        f.service
            .repositories
            .git(
                &f.repository,
                vec!["rev-parse".into(), record.refs[0].name.clone()]
            )
            .unwrap(),
        format!("{blob}\n").as_bytes()
    );
    f.service
        .repositories
        .git(
            &f.repository,
            vec![
                "update-ref".into(),
                "-d".into(),
                record.refs[0].name.clone(),
                blob,
            ],
        )
        .unwrap();
    assert_eq!(success(&f.service, release)["state"], "released");
}
#[test]
fn eviction_blocks_borrowing_workspaces_even_when_handle_record_is_lost() {
    let f = Fixture::new();
    let worker = register_worker(&f);
    let w = success(
        &f.service,
        request(&worker, "workspace.create", json!({"base":oid(&f.base)})),
    );
    let wid = w["workspace_id"].as_str().unwrap();
    std::fs::remove_file(f.service.root.path().join(format!("retention/{wid}.json"))).unwrap();
    let response=f.service.execute(request(&worker,"repo.cache-evict",json!({"expected_storage_generation":1,"reconstructible":true,"reason":"must not remove borrowed objects"})));
    let error = response.error.unwrap();
    assert_eq!(error.code, "retention_in_use");
    assert!(
        error.details["blockers"]
            .as_array()
            .unwrap()
            .contains(&json!(wid))
    );
    assert!(f.service.repositories.pool_path(&worker).exists());
}
#[test]
fn eviction_recovers_exact_directory_and_preserves_replacement_trash() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let worker = register_worker(&f);
    let evict = request(
        &worker,
        "repo.cache-evict",
        json!({"expected_storage_generation":1,"reconstructible":true,"reason":"crash ordering"}),
    );
    let op = evict.operation_id.clone().unwrap();
    f.service
        .root
        .inject_write_failure_after(3, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(evict.clone()).error.unwrap().code,
        "io_error"
    );
    let trash = f
        .service
        .root
        .path()
        .join(format!("trash/pool-{worker}-1-{op}"));
    assert!(trash.exists());
    assert!(!f.service.repositories.pool_path(&worker).exists());
    let done = success(&f.service, evict);
    assert_eq!(done["storage_generation"], 2);
    assert!(!trash.exists());
    let another = register_worker(&f);
    let evict = request(
        &another,
        "repo.cache-evict",
        json!({"expected_storage_generation":1,"reconstructible":true,"reason":"replacement safety"}),
    );
    let op = evict.operation_id.clone().unwrap();
    f.service
        .root
        .inject_write_failure_after(3, WriteFault::BeforeRename);
    assert_eq!(
        f.service.execute(evict.clone()).error.unwrap().code,
        "io_error"
    );
    let trash = f
        .service
        .root
        .path()
        .join(format!("trash/pool-{another}-1-{op}"));
    std::fs::remove_dir_all(&trash).unwrap();
    std::fs::create_dir(&trash).unwrap();
    std::fs::write(trash.join("preserve"), "other generation").unwrap();
    assert!(!f.service.execute(evict).ok);
    assert_eq!(
        std::fs::read_to_string(trash.join("preserve")).unwrap(),
        "other generation"
    );
}
