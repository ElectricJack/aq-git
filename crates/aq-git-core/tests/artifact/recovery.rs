use super::*;
use aq_git_core::state::{OperationStatus, WriteFault};
#[test]
fn interrupted_receipt_and_export_manifest_resume_exact_intent() {
    let f = Fixture::new();
    let (export_id, export) = resume_interrupted_export(&f);
    let (handle, _) = f.stage(&export);
    let import_request = f.import(&handle);
    let import_id = import_request.operation_id.clone().unwrap();
    assert_import_interrupted(&f, &import_request, &export_id, &import_id);
    let receipt = run(&f.receiver, import_request.clone());
    assert_eq!(receipt["transfer_id"], export_id);
    assert_eq!(run(&f.receiver, import_request), receipt);
}
fn resume_interrupted_export(f: &Fixture) -> (String, Value) {
    let export_request = request(
        &f.repo,
        "artifact.export",
        json!({"source":oid(&f.second),"base":oid(&f.first),"retention_handle":f.source_handle}),
    );
    let export_id = export_request.operation_id.clone().unwrap();
    f.source
        .root
        .inject_write_failure_after(4, WriteFault::BeforeRename);
    assert_eq!(
        f.source.execute(export_request.clone()).error.unwrap().code,
        "io_error"
    );
    assert_eq!(
        f.source.root.operation(&export_id).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Pending
    );
    let retained = f.source.root.retention(&f.source_handle).unwrap().unwrap();
    assert!(!retained.dependencies.contains(&export_id));
    assert!(f.source.root.retention(&export_id).unwrap().is_some());
    let export = run(&f.source, export_request);
    (export_id, export)
}
fn assert_import_interrupted(
    f: &Fixture,
    import_request: &Request,
    export_id: &str,
    import_id: &str,
) {
    f.receiver
        .root
        .inject_write_failure_after(4, WriteFault::BeforeRename);
    assert_eq!(
        f.receiver
            .execute(import_request.clone())
            .error
            .unwrap()
            .code,
        "io_error"
    );
    assert_eq!(
        f.receiver.root.operation(import_id).unwrap().unwrap().phase,
        "artifact_validated"
    );
    assert!(
        f.receiver.root.retention(export_id).unwrap().is_some(),
        "import state: {:?}; files {:?}",
        f.receiver.root.operation(import_id).unwrap(),
        fs::read_dir(f.receiver.root.path().join("retention"))
            .map(|r| r.map(|e| e.unwrap().path()).collect::<Vec<_>>())
    );
    assert!(
        !f.receiver
            .root
            .path()
            .join(format!(
                "repositories/{}/transfers/{export_id}.json",
                f.repo
            ))
            .exists()
    );
}
#[test]
fn interrupted_eviction_fences_import_before_intent_and_preserves_generations() {
    for (writes, point, expected_phase) in [
        (2, WriteFault::AfterRename, "evicting"),
        (3, WriteFault::BeforeRename, "evicting"),
        (4, WriteFault::BeforeRename, "absent"),
    ] {
        let f = Fixture::new();
        let export = f.export(json!([]), false);
        let (handle, _) = f.stage(&export);
        let evict = request(
            &f.repo,
            "repo.cache-evict",
            json!({"expected_storage_generation":1,"reconstructible":true,"reason":"interrupted eviction before new import"}),
        );
        let eviction = evict.operation_id.clone().unwrap();
        f.receiver.root.inject_write_failure_after(writes, point);
        assert_eq!(
            f.receiver.execute(evict.clone()).error.unwrap().code,
            "io_error"
        );
        let before: Value = f
            .receiver
            .root
            .read_json(format!("repositories/{}/pool.json", f.repo))
            .unwrap()
            .unwrap();
        assert_eq!(before["lifecycle"], expected_phase);
        assert_eq!(before["operation_id"], eviction);
        let import = f.import(&handle);
        assert_fenced_import(&f, &import, &eviction, &before);
        let done = run(&f.receiver, evict);
        assert_eq!(done["storage_generation"], 2);
        assert_eq!(done["lifecycle"], "absent");
        let imported = run(&f.receiver, import);
        assert_eq!(imported["source"], oid(&f.second));
        let pool: Value = f
            .receiver
            .root
            .read_json(format!("repositories/{}/pool.json", f.repo))
            .unwrap()
            .unwrap();
        assert_eq!(pool["storage_generation"], 3);
        assert_eq!(pool["lifecycle"], "present");
    }
}
fn assert_fenced_import(f: &Fixture, import: &Request, eviction: &str, before: &Value) {
    let pool_existed = f.receiver.repositories.pool_path(&f.repo).exists();
    let import_id = import.operation_id.clone().unwrap();
    let spawns = f.receiver.repositories.runner.metrics().spawns;
    let response = f.receiver.execute(import.clone());
    let error = response.error.unwrap();
    assert_eq!(error.code, "recovery_blocked");
    assert_eq!(error.details["blocking_operation_id"], eviction);
    assert_eq!(f.receiver.repositories.runner.metrics().spawns, spawns);
    assert!(f.receiver.root.operation(&import_id).unwrap().is_none());
    assert_eq!(
        f.receiver.repositories.pool_path(&f.repo).exists(),
        pool_existed
    );
    assert_eq!(
        f.receiver
            .root
            .read_json::<Value>(format!("repositories/{}/pool.json", f.repo))
            .unwrap()
            .unwrap(),
        *before
    );
    assert_eq!(
        f.receiver.root.operation(eviction).unwrap().unwrap().status,
        OperationStatus::Pending
    );
}
#[test]
fn malformed_or_missing_pool_state_is_not_silently_rehydrated() {
    let f = Fixture::new();
    let export = f.export(json!([]), false);
    let (handle, _) = f.stage(&export);
    let path = format!("repositories/{}/pool.json", f.repo);
    for malformed in [
        json!({"format_version":99,"storage_generation":1,"lifecycle":"present"}),
        json!({"format_version":1,"storage_generation":0,"lifecycle":"present"}),
        json!({"format_version":1,"storage_generation":"one","lifecycle":"present"}),
        json!({"format_version":1,"storage_generation":1,"lifecycle":"future"}),
    ] {
        f.receiver.root.write_json(&path, &malformed).unwrap();
        let import = f.import(&handle);
        let operation = import.operation_id.clone().unwrap();
        let before = f.receiver.repositories.runner.metrics().spawns;
        assert_eq!(
            f.receiver.execute(import).error.unwrap().code,
            "state_incompatible"
        );
        let after = f.receiver.repositories.runner.metrics().spawns;
        assert_eq!(after, before);
        assert!(f.receiver.root.operation(&operation).unwrap().is_none());
        assert_eq!(
            f.receiver.root.read_json::<Value>(&path).unwrap().unwrap(),
            malformed
        );
    }
    std::fs::remove_file(f.receiver.root.path().join(&path)).unwrap();
    let import = f.import(&handle);
    let operation = import.operation_id.clone().unwrap();
    assert_eq!(
        f.receiver.execute(import).error.unwrap().code,
        "recovery_blocked"
    );
    assert!(f.receiver.root.operation(&operation).unwrap().is_none());
    assert!(f.receiver.root.read_json::<Value>(&path).unwrap().is_none());
}
