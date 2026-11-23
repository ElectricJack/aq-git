use super::*;
// Parallel subprocess tests can briefly inherit a CLOEXEC flock descriptor between
// fork and exec. Wait for that legitimate transient owner after dropping our root.
fn after_execs_release<T>(
    mut action: impl FnMut() -> aq_git_core::Result<T>,
) -> aq_git_core::Result<T> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match action() {
            Err(error) if error.code == "state_busy" && std::time::Instant::now() < deadline => {
                std::thread::yield_now()
            }
            result => return result,
        }
    }
}
#[test]
fn journal_lock_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let root = StateRoot::open(dir.path()).unwrap();
    assert_eq!(
        StateRoot::open(dir.path()).err().unwrap().code,
        "state_busy"
    );
    let a = root.begin_operation("id", &json!({"b":2,"a":1})).unwrap();
    assert_eq!(
        root.begin_operation("id", &json!({"a":1,"b":2}))
            .unwrap()
            .payload_hash,
        a.payload_hash
    );
    assert_eq!(
        root.begin_operation("id", &json!({"a":2}))
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    let b = root
        .advance_operation("id", a.generation, "prepared", json!({"x":3}))
        .unwrap();
    assert_eq!(
        root.complete_operation("id", a.generation, json!(true))
            .unwrap_err()
            .code,
        "generation_conflict"
    );
    root.complete_operation("id", b.generation, json!({"answer":42}))
        .unwrap();
    drop(root);
    let root = after_execs_release(|| StateRoot::open(dir.path())).unwrap();
    assert_eq!(
        root.begin_operation("id", &json!({"b":2,"a":1}))
            .unwrap()
            .result,
        Some(json!({"answer":42}))
    );
}
#[test]
fn future_format_and_symlinks_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("format.json"), "{\"format_version\":99}").unwrap();
    assert_eq!(
        StateRoot::open(dir.path()).err().unwrap().code,
        "state_incompatible"
    );
    std::fs::remove_file(dir.path().join("format.json")).unwrap();
    let root = after_execs_release(|| StateRoot::open(dir.path())).unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join("redirect")).unwrap();
    assert!(root.write_json("redirect/data.json", &json!(true)).is_err());
    assert!(!outside.path().join("data.json").exists());
    assert!(root.write_json("../escape", &json!(true)).is_err());
}
#[test]
fn incarnation_fence() {
    let dir = tempfile::tempdir().unwrap();
    let root = StateRoot::open(dir.path()).unwrap();
    root.mark_active("old").unwrap();
    drop(root);
    assert_eq!(
        after_execs_release(|| StateRoot::open(dir.path()))
            .err()
            .unwrap()
            .code,
        "recovery_blocked"
    );
    let root = after_execs_release(|| StateRoot::open_for_recovery(dir.path())).unwrap();
    assert!(root.mark_clean("wrong").is_err());
    root.mark_clean("old").unwrap();
    drop(root);
    assert!(after_execs_release(|| StateRoot::open(dir.path())).is_ok());
}
fn record(id: &str) -> RetentionRecord {
    RetentionRecord {
        version: 1,
        id: id.into(),
        repository_id: "repo".into(),
        binding_generation: 1,
        generation: 1,
        resource: "checkpoint".into(),
        refs: vec![RetentionRef {
            name: format!("refs/aq/retention/{id}"),
            oid: "a".repeat(40),
        }],
        state: RetentionState::Active,
        dependencies: BTreeSet::new(),
        release_operation: None,
        release_reason: None,
    }
}
#[test]
fn retention_dependencies_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let root = StateRoot::open(dir.path()).unwrap();
    root.create_retention(&record("a")).unwrap();
    root.create_retention(&record("b")).unwrap();
    let a = root.acquire_dependency("a", "b").unwrap();
    assert_eq!(
        root.begin_release("a", a.generation).unwrap_err().code,
        "retention_in_use"
    );
    assert_eq!(
        root.acquire_dependency("b", "a").unwrap_err().code,
        "retention_cycle"
    );
    let a = root.release_dependency("a", "b").unwrap();
    let a = root.begin_release("a", a.generation).unwrap();
    assert_eq!(
        root.acquire_dependency("a", "consumer").unwrap_err().code,
        "retention_released"
    );
    let a = root.finish_release("a", a.generation).unwrap();
    assert_eq!(a.state, RetentionState::Released);
}
#[test]
fn durable_boundary_failures_keep_recoverable_snapshots() {
    use aq_git_core::state::WriteFault;
    let dir = tempfile::tempdir().unwrap();
    let root = StateRoot::open(dir.path()).unwrap();
    root.write_json("record.json", &json!({"generation":1}))
        .unwrap();
    root.inject_write_failure_once(WriteFault::BeforeRename);
    assert_eq!(
        root.write_json("record.json", &json!({"generation":2}))
            .unwrap_err()
            .code,
        "io_error"
    );
    assert_eq!(
        root.read_json::<serde_json::Value>("record.json")
            .unwrap()
            .unwrap()["generation"],
        1
    );
    root.inject_write_failure_once(WriteFault::AfterRename);
    assert_eq!(
        root.write_json("record.json", &json!({"generation":2}))
            .unwrap_err()
            .code,
        "io_error"
    );
    assert_eq!(
        root.read_json::<serde_json::Value>("record.json")
            .unwrap()
            .unwrap()["generation"],
        2
    );
}
#[test]
fn live_storage_sampling_does_not_follow_transient_links_and_rejects_residual_links() {
    let temp = tempfile::tempdir().unwrap();
    let managed = temp.path().join("managed");
    std::fs::create_dir(&managed).unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("large"), vec![0u8; 8192]).unwrap();
    let script = temp.path().join("git");
    // Hold the same kind of temporary symlink Git init uses across many quota
    // samples, pointing at an over-budget directory to prove it is not followed.
    std::fs::write(
        &script,
        "#!/bin/sh\nln -s ../outside tProbe\nsleep 0.05\nrm tProbe\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = GitRunner::new(script.clone());
    let output = runner
        .run_with_disk_budget(&managed, &[], &managed, 1024)
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        aq_git_core::git::runner::directory_bytes(&managed, 1024).unwrap(),
        0
    );
    let error = runner
        .run_with_disk_budget(&managed, &[], &managed, 1)
        .unwrap_err();
    assert_eq!(
        error.code, "storage_limit",
        "link bytes must count toward quota"
    );
    std::fs::remove_file(managed.join("tProbe")).unwrap();
    // Residual links never pass final validation, even if their name resembles
    // Git's probe and their target is inside the managed directory.
    std::fs::write(&script, "#!/bin/sh\nln -s testing tProbe\nsleep 0.05\n").unwrap();
    let runner = GitRunner::new(script);
    let error = runner
        .run_with_disk_budget(&managed, &[], &managed, 1024)
        .unwrap_err();
    assert_eq!(error.code, "invalid_path");
    assert_eq!(error.details["entry_type"], "symlink");
    assert_eq!(runner.metrics().active, 0);
    let spawns = runner.metrics().spawns;
    let error = runner
        .run_with_disk_budget(&managed, &[], &managed, 1024)
        .unwrap_err();
    assert_eq!(error.code, "invalid_path");
    assert_eq!(
        runner.metrics().spawns,
        spawns,
        "pre-existing links must fail before spawn"
    );
    std::fs::remove_file(managed.join("tProbe")).unwrap();
    std::fs::write(managed.join("oversize"), vec![0u8; 2048]).unwrap();
    let error = runner
        .run_with_disk_budget(&managed, &[], &managed, 1024)
        .unwrap_err();
    assert_eq!(error.code, "storage_limit");
    assert_eq!(
        runner.metrics().spawns,
        spawns,
        "pre-existing over-quota bytes must fail before spawn"
    );
}
