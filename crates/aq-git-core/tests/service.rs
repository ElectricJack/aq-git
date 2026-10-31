use aq_git_core::RepositoryService;
use aq_git_protocol::Request;
use serde_json::json;
use std::time::{Duration, Instant};
fn registration() -> Request {
    Request {
        protocol_version: 1,
        request_id: "service-test".into(),
        operation: "repo.register".into(),
        repository_id: Some(uuid::Uuid::new_v4().to_string()),
        operation_id: Some(uuid::Uuid::new_v4().to_string()),
        expected_binding_generation: Some(0),
        params: json!({"identity":"test","mode":"central","remote_url":"https://user:secret@example.invalid/repository.git"}),
        deadline_ms: None,
    }
}
#[test]
fn rejects_embedded_credentials_without_journaling_them() {
    let temp = tempfile::tempdir().unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let request = registration();
    let operation = request.operation_id.clone().unwrap();
    let answer = service.execute(request);
    assert!(!answer.ok);
    assert!(service.root.operation(&operation).unwrap().is_none());
    assert_eq!(service.repositories.runner.metrics().spawns, 0);
}
#[test]
fn expired_admission_creates_no_mutation_intent() {
    let temp = tempfile::tempdir().unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let mut request = registration();
    request.params = json!({"identity":"test","mode":"worker"});
    request.deadline_ms = Some(1);
    let operation = request.operation_id.clone().unwrap();
    let answer = service.execute_admitted(request, Instant::now() - Duration::from_millis(100));
    assert_eq!(answer.error.unwrap().code, "deadline_exceeded");
    assert!(service.root.operation(&operation).unwrap().is_none());
    assert_eq!(service.repositories.runner.metrics().spawns, 0);
}

#[test]
fn existing_bindings_require_matching_executable_behavior_before_admission() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let service = RepositoryService::open(&path).unwrap();
    let mut register = registration();
    register.params = json!({"identity":"test","mode":"worker"});
    assert!(service.execute(register).ok);
    let root = (*service.root).clone();
    drop(service);
    let executable = temp.path().join("old-git");
    std::fs::write(&executable, "#!/bin/sh\nprintf 'git version 2.42.0\\n'\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = match RepositoryService::from_state_root_with_git(root, executable) {
        Ok(_) => panic!("unverified replacement executable was admitted"),
        Err(error) => error,
    };
    assert_eq!(error.code, "unsupported_feature");
    let diagnostic = RepositoryService::for_diagnostics(
        aq_git_core::state::StateRoot::open_for_recovery(path).unwrap(),
    )
    .unwrap();
    let request = Request {
        protocol_version: 1,
        request_id: "doctor".into(),
        operation: "doctor".into(),
        repository_id: None,
        operation_id: None,
        expected_binding_generation: None,
        params: json!({}),
        deadline_ms: None,
    };
    assert!(diagnostic.execute(request).ok);
}

#[test]
fn executable_replacement_after_pool_creation_preserves_intent_for_same_id_restart() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("one-time-updated-git");
    std::fs::write(&executable,"#!/bin/sh\n/usr/bin/git \"$@\"\nstatus=$?\nis_init=0\nfor arg do [ \"$arg\" != init ] || is_init=1; done\ncase \"$PWD\" in */repositories/*/objects.git) if [ \"$status\" = 0 ] && [ \"$is_init\" = 1 ] && [ ! -f \"${0}.updated\" ]; then printf '# installed replacement\\n' >>\"$0\"; printf done >\"${0}.updated\"; fi;; esac\nexit \"$status\"\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = aq_git_core::state::StateRoot::open(temp.path().join("state")).unwrap();
    let service = RepositoryService::from_state_root_with_git(state, executable.clone()).unwrap();
    let mut request = registration();
    request.params = json!({"identity":"restart-fixture","mode":"worker"});
    let operation = request.operation_id.clone().unwrap();
    let repository = request.repository_id.clone().unwrap();
    let response = service.execute(request.clone());
    let error = response.error.unwrap();
    assert_eq!(error.code, "unsupported_feature");
    assert_eq!(error.details["restart_required"], true);
    assert_eq!(response.operation_state.as_deref(), Some("pending"));
    assert_eq!(
        service.root.operation(&operation).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Pending
    );
    assert!(
        service
            .repositories
            .pool_path(&repository)
            .join("config")
            .is_file()
    );
    assert!(
        service
            .root
            .read_json::<serde_json::Value>(format!("repositories/{repository}/binding.json"))
            .unwrap()
            .is_none()
    );
    assert_eq!(service.repositories.runner.metrics().spawns, 1);
    let state = service.root.as_ref().clone();
    drop(service);
    let restarted = RepositoryService::from_state_root_with_git(state, executable).unwrap();
    let replay = restarted.execute(request);
    assert!(replay.ok, "{:?}", replay.error);
    assert_eq!(replay.result.unwrap()["repository_id"], repository);
    assert_eq!(
        restarted
            .root
            .operation(&operation)
            .unwrap()
            .unwrap()
            .status,
        aq_git_core::state::OperationStatus::Complete
    );
}
#[test]
fn spawn_failure_after_local_registration_intent_remains_recoverable() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("incomplete-git-image");
    std::fs::write(&executable, "#!/nonexistent/aq-git-missing-interpreter\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = aq_git_core::state::StateRoot::open(temp.path().join("state")).unwrap();
    let service = RepositoryService::from_state_root_with_git(state, executable.clone()).unwrap();
    let mut request = registration();
    request.params = json!({"identity":"spawn-retry","mode":"worker"});
    let operation = request.operation_id.clone().unwrap();
    let response = service.execute(request.clone());
    assert_eq!(response.error.unwrap().code, "git_spawn");
    assert_eq!(
        service.root.operation(&operation).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Pending
    );
    std::fs::write(&executable, "#!/bin/sh\nexec /usr/bin/git \"$@\"\n").unwrap();
    let state = service.root.as_ref().clone();
    drop(service);
    let restarted = RepositoryService::from_state_root_with_git(state, executable).unwrap();
    let replay = restarted.execute(request);
    assert!(replay.ok, "{:?}", replay.error);
    assert_eq!(
        restarted
            .root
            .operation(&operation)
            .unwrap()
            .unwrap()
            .status,
        aq_git_core::state::OperationStatus::Complete
    );
}
#[test]
fn genuine_unsupported_feature_is_terminal_and_replays_without_git() {
    let temp = tempfile::tempdir().unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let mut request = registration();
    request.params =
        json!({"identity":"unsupported-format","mode":"worker","object_format":"sha256"});
    let operation = request.operation_id.clone().unwrap();
    let response = service.execute(request.clone());
    let error = response.error.unwrap();
    assert_eq!(error.code, "unsupported_feature");
    assert_ne!(error.details["restart_required"], true);
    assert_eq!(response.operation_state.as_deref(), Some("failed"));
    assert_eq!(
        service.root.operation(&operation).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Failed
    );
    let spawns = service.repositories.runner.metrics().spawns;
    assert_eq!(
        service.execute(request).error.unwrap().code,
        "unsupported_feature"
    );
    assert_eq!(service.repositories.runner.metrics().spawns, spawns);
}
