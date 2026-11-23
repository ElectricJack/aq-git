use super::*;
#[test]
fn lost_output_after_real_push_never_redispatches_and_reconciles_presence() {
    let mut f = Fixture::new();
    f.suppress_push_output();
    let publish = f.publish();
    let id = publish.operation_id.clone().unwrap();
    let unknown = f.service.execute(publish.clone());
    assert_eq!(unknown.error.unwrap().code, "operation_indeterminate");
    assert_eq!(git(&f.remote, &["rev-parse", "refs/heads/main"]), f.source);
    let before = f.service.repositories.runner.metrics().spawns;
    assert_indeterminate(&f, publish.clone());
    assert_eq!(before, f.service.repositories.runner.metrics().spawns);
    assert_indeterminate(&f, f.publish());
    let target = f.service.root.operation(&id).unwrap().unwrap();
    let result = success(f.service.execute(request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":id,"expected_generation":target.generation}),
    )));
    assert_already_present(&result);
    assert_eq!(
        success(f.service.execute(publish))["outcome"],
        "already_present"
    );
    assert!(
        f.service
            .root
            .retention(&f.candidate)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
}
#[test]
fn unchanged_old_after_uncertainty_stays_unknown_until_audited_resolution() {
    let mut f = Fixture::new();
    f.suppress_push_output();
    let publish = f.publish();
    let id = publish.operation_id.clone().unwrap();
    assert_indeterminate(&f, publish);
    git(&f.remote, &["update-ref", "refs/heads/main", &f.base]);
    let target = f.service.root.operation(&id).unwrap().unwrap();
    let reconciled = success(f.service.execute(request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":id,"expected_generation":target.generation}),
    )));
    assert_eq!(reconciled["outcome"], "indeterminate");
    assert_eq!(
        f.service.root.operation(&id).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Pending
    );
    let mut resolve = request(
        &f.repo,
        "operation.resolve",
        json!({"target_operation_id":id,"expected_generation":target.generation,"disposition":"permit_new_operation","reason":"fixture stopped old publisher and inspected remote","old_publishers_stopped":false,"evidence":{"fixture":"old engine stopped"}}),
    );
    assert_eq!(
        f.service.execute(resolve.clone()).error.unwrap().code,
        "invalid_request"
    );
    resolve.operation_id = Some(uuid::Uuid::new_v4().to_string());
    resolve.params["old_publishers_stopped"] = json!(true);
    let resolved = success(f.service.execute(resolve.clone()));
    assert_eq!(
        resolved["resolution"]["historical_remote_effect"],
        "unknown"
    );
    assert_eq!(resolved, success(f.service.execute(resolve)));
    f.service.repositories.runner = GitRunner::new("/usr/bin/git".into());
    assert_eq!(
        success(f.service.execute(f.publish()))["outcome"],
        "applied"
    );
}

#[test]
fn reconciliation_and_resolution_resume_between_domain_and_journal_acknowledgements() {
    for resolve in [false, true] {
        let mut f = Fixture::new();
        f.suppress_push_output();
        let publish = f.publish();
        let id = publish.operation_id.clone().unwrap();
        assert_indeterminate(&f, publish);
        let target_before = f.service.root.operation(&id).unwrap().unwrap();
        let command = if resolve {
            request(
                &f.repo,
                "operation.resolve",
                json!({"target_operation_id":id,"expected_generation":target_before.generation,"disposition":"abandon","reason":"fixture stopped publishers","old_publishers_stopped":true,"evidence":"fixture cleanup complete"}),
            )
        } else {
            request(
                &f.repo,
                "operation.reconcile",
                json!({"target_operation_id":id,"expected_generation":target_before.generation}),
            )
        };
        let own_id = command.operation_id.as_ref().unwrap();
        let own_before=f.service.root.begin_operation(own_id,&json!({"operation":command.operation,"repository_id":command.repository_id,"expected_binding_generation":command.expected_binding_generation,"params":command.params})).unwrap();
        let result = success(f.service.execute(command.clone()));
        // Reconstruct the durable crash boundary: domain result installed, target
        // and command acknowledgements not yet persisted. No Git write is retried.
        f.service
            .root
            .write_json(format!("operations/{id}.json"), &target_before)
            .unwrap();
        f.service
            .root
            .write_json(format!("operations/{own_id}.json"), &own_before)
            .unwrap();
        assert_eq!(result, success(f.service.execute(command.clone())));
        assert_eq!(
            f.service.root.operation(&id).unwrap().unwrap().status,
            aq_git_core::state::OperationStatus::Complete
        );
        f.service
            .root
            .write_json(format!("operations/{own_id}.json"), &own_before)
            .unwrap();
        assert_eq!(result, success(f.service.execute(command)));
    }
}

#[test]
fn replaying_old_acknowledgements_cannot_clear_a_successor_indeterminate_hold() {
    use sha2::{Digest, Sha256};
    for mode in ["publish", "operation.resolve", "operation.reconcile"] {
        let mut f = Fixture::new();
        let replay = prepare_acknowledgement(&mut f, mode);
        let historical = interrupt_acknowledgement(&f, &replay, mode);
        git(&f.remote, &["update-ref", "refs/heads/main", &f.base]);
        f.suppress_push_output();
        let successor = f.publish();
        let successor_id = successor.operation_id.clone().unwrap();
        assert_indeterminate(&f, successor);
        let path = format!(
            "repositories/{}/publication-targets/{}.json",
            f.repo,
            hex::encode(Sha256::digest(b"refs/heads/main"))
        );
        let before: Value = f.service.root.read_json(&path).unwrap().unwrap();
        assert_successor_hold(&before, &successor_id);
        assert_eq!(success(f.service.execute(replay)), historical, "{mode}");
        let after: Value = f.service.root.read_json(&path).unwrap().unwrap();
        assert_eq!(
            after, before,
            "{mode} replay must not modify successor owner/generation"
        );
        let blocked = f.service.execute(f.publish()).error.unwrap();
        assert_eq!(blocked.code, "operation_indeterminate");
        assert_eq!(blocked.details["blocking_operation_id"], successor_id);
        let successor = f.service.root.operation(&successor_id).unwrap().unwrap();
        assert_eq!(
            successor.status,
            aq_git_core::state::OperationStatus::Pending
        );
        assert!(
            f.service
                .root
                .retention(&f.candidate)
                .unwrap()
                .unwrap()
                .dependencies
                .contains(&successor_id)
        );
        let unchanged: Value = f.service.root.read_json(&path).unwrap().unwrap();
        assert_eq!(unchanged, before);
    }
}
fn prepare_acknowledgement(f: &mut Fixture, mode: &str) -> Request {
    let original = f.publish();
    let original_id = original.operation_id.clone().unwrap();
    if mode == "publish" {
        success(f.service.execute(original.clone()));
        original
    } else {
        f.suppress_push_output();
        assert_indeterminate(f, original);
        let target = f.service.root.operation(&original_id).unwrap().unwrap();
        let command = if mode == "operation.resolve" {
            request(
                &f.repo,
                mode,
                json!({"target_operation_id":original_id,"expected_generation":target.generation,"disposition":"abandon","reason":"fixture stopped old publishers","old_publishers_stopped":true,"evidence":"fixture cleanup complete"}),
            )
        } else {
            request(
                &f.repo,
                mode,
                json!({"target_operation_id":original_id,"expected_generation":target.generation}),
            )
        };
        success(f.service.execute(command.clone()));
        command
    }
}
fn interrupt_acknowledgement(f: &Fixture, replay: &Request, mode: &str) -> Value {
    let command_id = replay.operation_id.as_ref().unwrap();
    let mut pending = f.service.root.operation(command_id).unwrap().unwrap();
    let historical = pending.result.clone().unwrap();
    // Reconstruct the crash after domain completion/hold cleanup but before
    // this command's terminal journal acknowledgement reaches durability.
    pending.status = aq_git_core::state::OperationStatus::Pending;
    pending.result = None;
    pending.error = None;
    pending.generation -= 1;
    pending.phase = if mode == "publish" {
        "dispatched"
    } else {
        "intent"
    }
    .into();
    f.service
        .root
        .write_json(format!("operations/{command_id}.json"), &pending)
        .unwrap();
    historical
}
fn assert_successor_hold(hold: &Value, owner: &str) {
    assert_eq!(hold["active"], true);
    assert_eq!(hold["operation_id"], owner);
    assert!(hold["generation"].as_u64().unwrap() > 0);
}
#[test]
fn unavailable_remote_reconciliation_preserves_hold_then_fresh_command_recovers() {
    let mut f = Fixture::new();
    f.suppress_push_output();
    let publish = f.publish();
    let target = publish.operation_id.clone().unwrap();
    assert_indeterminate(&f, publish);
    let before = f.service.root.operation(&target).unwrap().unwrap();
    let reconcile = request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":target,"expected_generation":before.generation}),
    );
    let saved = f.temp.path().join("remote-offline.git");
    std::fs::rename(&f.remote, &saved).unwrap();
    let unavailable = success(f.service.execute(reconcile.clone()));
    assert_eq!(unavailable["outcome"], "indeterminate");
    assert_eq!(unavailable["observation_error"], "remote_unavailable");
    let after = f.service.root.operation(&target).unwrap().unwrap();
    assert_eq!(after.status, aq_git_core::state::OperationStatus::Pending);
    assert_eq!(after.generation, before.generation);
    assert!(
        f.service
            .root
            .retention(&f.candidate)
            .unwrap()
            .unwrap()
            .dependencies
            .contains(&target)
    );
    assert_indeterminate(&f, f.publish());
    std::fs::rename(&saved, &f.remote).unwrap();
    let spawns = f.service.repositories.runner.metrics().spawns;
    assert_eq!(success(f.service.execute(reconcile)), unavailable);
    assert_eq!(spawns, f.service.repositories.runner.metrics().spawns);
    let recovered = success(f.service.execute(request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":target,"expected_generation":after.generation}),
    )));
    assert_already_present(&recovered);
    assert_eq!(
        f.service.root.operation(&target).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Complete
    );
    assert!(
        f.service
            .root
            .retention(&f.candidate)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
}
fn assert_already_present(result: &Value) {
    assert_eq!(result["resolution"]["outcome"], "already_present");
    assert_eq!(result["resolution"]["caused_by_this_operation"], false);
}
