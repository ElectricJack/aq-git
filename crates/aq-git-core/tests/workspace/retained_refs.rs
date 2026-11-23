use super::*;

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

fn assert_pinned_head(f: &Fixture, first: &serde_json::Value) {
    assert_eq!(first["ref"], "refs/heads/main");
    assert_eq!(first["head"], oid(&f.base));
    assert!(first["synced_at_ms"].as_i64().unwrap() > 0);
    let record = handle_state(f, &first["retention_handle"]);
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
}

fn assert_repinned_after_release(f: &Fixture, moved: &serde_json::Value, next_head: &str) {
    // A released handle is not reused for the same head.
    let old = handle_state(f, &moved["retention_handle"]);
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
    assert_eq!(repinned["head"], oid(next_head));
    assert_ne!(repinned["retention_handle"], moved["retention_handle"]);
    assert_eq!(
        handle_state(f, &repinned["retention_handle"])["state"],
        "active"
    );
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
    assert_pinned_head(&f, &first);
    let handle = first["retention_handle"].as_str().unwrap();
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
    assert_repinned_after_release(&f, &moved, &next_head);
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
    assert_eq!(
        (failed.error.unwrap().code.as_str(), failed.result),
        ("remote_unavailable", None)
    );
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
