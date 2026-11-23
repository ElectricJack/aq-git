use super::*;
use aq_git_core::{
    RepositoryService,
    state::{OperationStatus, WriteFault},
};
#[test]
fn interrupted_binding_transition_fences_new_admission_and_recovers_cached_old_authority() {
    for (point, writes, expected_disk, restart) in [
        (WriteFault::AfterRename, 1, 1, false),
        (WriteFault::BeforeRename, 3, 1, true),
        (WriteFault::AfterRename, 3, 4, false),
        (WriteFault::AfterRename, 4, 4, false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut service = RepositoryService::open(temp.path().join("state")).unwrap();
        let repo = uuid::Uuid::new_v4().to_string();
        let (register, update) = observed_transition(&service, &repo);
        let operation = update.operation_id.clone().unwrap();
        service.root.inject_write_failure_after(writes, point);
        assert_failed_transition(&service, &update, expected_disk);
        assert_admission_fenced(&service, &repo, register);
        if restart {
            let root = service.root.as_ref().clone();
            drop(service);
            service = RepositoryService::from_state_root(root).unwrap();
            assert_eq!(
                service.root.operation(&operation).unwrap().unwrap().status,
                OperationStatus::Complete
            );
        }
        assert_transition_replay(&service, update);
        assert_next_transition(&service, &repo);
    }
}
fn observed_transition(service: &RepositoryService, repo: &str) -> (Request, Request) {
    let register = request(
        repo,
        "repo.register",
        json!({"identity":"fenced-worker","mode":"worker","allowed_refs":["refs/heads/main"]}),
    );
    let registered = service.execute(register.clone());
    assert!(registered.ok, "{:?}", registered.error);
    let mut replacement = registered.result.unwrap();
    service.root.write_json(format!("repositories/{repo}/observation.json"),&json!({"binding_generation":1,"covered_refs":["refs/heads/main"],"refs":{},"completed_at_ms":chrono::Utc::now().timestamp_millis()})).unwrap();
    assert!(
        service
            .execute(request(repo, "repo.observation", json!({})))
            .ok
    );
    replacement["binding_generation"] = json!(4);
    let update = request(
        repo,
        "repo.binding-update",
        json!({"binding":replacement,"reason":"interrupted drained generation adoption"}),
    );
    (register, update)
}
fn assert_failed_transition(service: &RepositoryService, update: &Request, expected_disk: u64) {
    let repo = update.repository_id.as_ref().unwrap();
    let operation = update.operation_id.as_ref().unwrap();
    let failed = service.execute(update.clone());
    assert_eq!(failed.error.unwrap().code, "io_error");
    assert_eq!(
        service.root.operation(operation).unwrap().unwrap().status,
        OperationStatus::Pending
    );
    let disk: Value = service
        .root
        .read_json(format!("repositories/{repo}/binding.json"))
        .unwrap()
        .unwrap();
    assert_eq!(disk["binding_generation"], expected_disk);
}
fn assert_admission_fenced(service: &RepositoryService, repo: &str, register: Request) {
    let spawns = service.repositories.runner.metrics().spawns;
    for generation in [1, 4] {
        let mut refused = request(
            repo,
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
}
fn assert_transition_replay(service: &RepositoryService, update: Request) {
    let operation = update.operation_id.clone().unwrap();
    let replay = service.execute(update);
    assert!(replay.ok, "{:?}", replay.error);
    assert_eq!(replay.result.unwrap()["binding_generation"], 4);
    assert_eq!(
        service.root.operation(&operation).unwrap().unwrap().status,
        OperationStatus::Complete
    );
}
fn assert_next_transition(service: &RepositoryService, repo: &str) {
    let mut show = request(repo, "repo.show", json!({}));
    show.expected_binding_generation = Some(4);
    let current = service.execute(show);
    assert!(current.ok);
    let mut next = current.result.unwrap();
    let mut observation = request(repo, "repo.observation", json!({}));
    observation.expected_binding_generation = Some(4);
    assert_eq!(
        service.execute(observation).error.unwrap().code,
        "observation_unavailable"
    );
    next["binding_generation"] = json!(8);
    let mut later = request(
        repo,
        "repo.binding-update",
        json!({"binding":next,"reason":"later unrelated transition must not inherit old fence"}),
    );
    later.expected_binding_generation = Some(4);
    let done = service.execute(later);
    assert!(done.ok, "{:?}", done.error);
    assert_eq!(done.result.unwrap()["binding_generation"], 8);
}
#[test]
fn binding_transition_pre_intent_failure_proves_old_authority_and_keeps_retry_id() {
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
