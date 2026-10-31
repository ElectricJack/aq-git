use aq_git_core::{
    git::{GitLimits, GitRunner},
    state::{RetentionRecord, RetentionRef, RetentionState, StateRoot},
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    os::unix::fs::{PermissionsExt, symlink},
    time::Duration,
};
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
fn real_git_scrubbed_and_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    let out = runner.run(dir.path(), &["--version".into()]).unwrap();
    assert!(out.status.success());
    assert!(out.stdout.starts_with(b"git version"));
    assert_eq!(runner.metrics().spawns, 1);
    assert_eq!(runner.metrics().active, 0);
}
#[test]
fn bounded_runner_timeout_output_and_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-git");
    std::fs::write(
        &script,
        "#!/bin/sh\nwhile :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let limits = GitLimits {
        max_children: 1,
        max_address_space_bytes: 1024 * 1024 * 1024,
        timeout: Duration::from_millis(100),
        max_output_bytes: 1024,
    };
    let runner = GitRunner::with_limits(script.clone(), limits.clone());
    assert_eq!(
        runner.run(dir.path(), &[]).unwrap_err().code,
        "output_limit"
    );
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    let runner = GitRunner::with_limits(script.clone(), limits);
    assert_eq!(runner.run(dir.path(), &[]).unwrap_err().code, "git_timeout");
    assert_eq!(runner.metrics().active, 0);
    let missing = GitRunner::new(dir.path().join("missing"));
    assert_eq!(missing.run(dir.path(), &[]).unwrap_err().code, "git_spawn");
    assert_eq!(missing.metrics().active, 0);
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
fn git_input_and_environment_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    let out = runner
        .run_with(
            dir.path(),
            &["hash-object".into(), "--stdin".into()],
            b"hello\n",
            &[],
            Duration::from_secs(1),
        )
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"ce013625030ba8dba906f756967f9e9ca394464a\n");
    assert_eq!(
        runner
            .run_with(
                dir.path(),
                &[],
                &[],
                &[("GIT_CONFIG_COUNT".into(), "99".into())],
                Duration::from_secs(1)
            )
            .unwrap_err()
            .code,
        "invalid_environment"
    );
}
#[test]
fn streamed_output_and_managed_disk_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-git");
    std::fs::write(
        &script,
        "#!/bin/sh\nwhile :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = GitRunner::new(script.clone());
    let sink_path = dir.path().join("sink");
    let sink = std::fs::File::create(&sink_path).unwrap();
    assert_eq!(
        runner
            .run_to_file(dir.path(), &[], sink, 128)
            .unwrap_err()
            .code,
        "output_limit"
    );
    assert_eq!(std::fs::metadata(sink_path).unwrap().len(), 128);
    let storage = dir.path().join("storage");
    std::fs::create_dir(&storage).unwrap();
    std::fs::write(&script,"#!/bin/sh\nfor n in 1 2 3 4 5 6 7 8 9 10; do dd if=/dev/zero of=file$n bs=4096 count=1 2>/dev/null; done\n").unwrap();
    let runner = GitRunner::new(script.clone());
    assert_eq!(
        runner
            .run_with_disk_budget(&storage, &[], &storage, 8192)
            .unwrap_err()
            .code,
        "storage_limit"
    );
    assert_eq!(runner.metrics().active, 0);
}
#[test]
fn request_deadlines_nest_and_refuse_before_spawn() {
    use aq_git_core::git::{DeadlineScope, remaining_deadline};
    let dir = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    assert!(remaining_deadline().is_none());
    {
        let _outer = DeadlineScope::new(Duration::ZERO);
        let _inner = DeadlineScope::new(Duration::from_secs(60));
        assert_eq!(remaining_deadline(), Some(Duration::ZERO));
        assert_eq!(
            runner
                .run(dir.path(), &["--version".into()])
                .unwrap_err()
                .code,
            "deadline_exceeded"
        );
        assert_eq!(runner.metrics().spawns, 0);
    }
    assert!(remaining_deadline().is_none());
    assert!(
        runner
            .run(dir.path(), &["--version".into()])
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn coordinated_lock_waits_obey_deadlines_and_preserve_poison() {
    use aq_git_core::git::{
        DeadlineScope, lock_with_deadline, read_with_deadline, write_with_deadline,
    };
    let mutex = std::sync::Mutex::new(1);
    let held = mutex.lock().unwrap();
    {
        let _deadline = DeadlineScope::new(Duration::from_millis(2));
        assert_eq!(
            lock_with_deadline(&mutex).unwrap_err().code,
            "deadline_exceeded"
        );
    }
    drop(held);
    assert_eq!(*lock_with_deadline(&mutex).unwrap(), 1);
    let lock = std::sync::RwLock::new(2);
    let held = write_with_deadline(&lock).unwrap();
    {
        let _deadline = DeadlineScope::new(Duration::ZERO);
        assert_eq!(
            read_with_deadline(&lock).unwrap_err().code,
            "deadline_exceeded"
        );
    }
    drop(held);
    assert_eq!(*read_with_deadline(&lock).unwrap(), 2);
    let poisoned = std::sync::Arc::new(std::sync::Mutex::new(0));
    let copy = poisoned.clone();
    assert!(
        std::thread::spawn(move || {
            let _guard = copy.lock().unwrap();
            panic!("fixture poison")
        })
        .join()
        .is_err()
    );
    assert_eq!(
        lock_with_deadline(&poisoned).unwrap_err().code,
        "recovery_blocked"
    );
}
#[test]
fn memory_allocation_helper() {
    if std::env::var_os("AQ_GIT_ALLOCATION_HELPER").is_some() {
        let mut bytes = Vec::<u8>::new();
        assert!(
            bytes.try_reserve_exact(512 * 1024 * 1024).is_err(),
            "configured address-space ceiling must reject allocation"
        );
    }
}
#[test]
fn child_address_space_budget_rejects_large_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-git");
    let test_binary = std::env::current_exe().unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    std::fs::write(&script,format!("#!/bin/sh\nexport AQ_GIT_ALLOCATION_HELPER=1\nexec {} --exact memory_allocation_helper --test-threads 1\n",quote(test_binary.to_str().unwrap()))).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let limits = GitLimits {
        max_address_space_bytes: 128 * 1024 * 1024,
        ..GitLimits::default()
    };
    let runner = GitRunner::with_limits(script, limits);
    let output = runner.run(dir.path(), &[]).unwrap();
    assert!(
        output.status.success(),
        "allocation helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(runner.metrics().active, 0);
}
#[test]
fn executable_replacement_requires_fresh_runner_and_behavior_reprobe() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("git");
    std::fs::write(
        &script,
        "#!/bin/sh\n# initial installation\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = GitRunner::new(script.clone());
    assert!(
        runner
            .run(temp.path(), &["--version".into()])
            .unwrap()
            .status
            .success()
    );
    let replacement = temp.path().join("replacement");
    std::fs::write(
        &replacement,
        "#!/bin/sh\n# updated installation\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::rename(replacement, &script).unwrap();
    let before = runner.metrics().spawns;
    let error = runner.run(temp.path(), &["--version".into()]).unwrap_err();
    assert_eq!(error.code, "unsupported_feature");
    assert!(error.message.contains("restart"));
    assert_eq!(runner.metrics().spawns, before);
    assert_eq!(runner.metrics().active, 0);
    let replacement_runner = GitRunner::new(script);
    let root = StateRoot::open(temp.path().join("state")).unwrap();
    let report = aq_git_core::git::capabilities::probe(
        &root,
        &replacement_runner,
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    assert_eq!(report["features"].as_array().unwrap().len(), 7);
    let missing = temp.path().join("initially-missing");
    let missing_runner = GitRunner::new(missing.clone());
    std::fs::write(&missing, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&missing, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        missing_runner.run(temp.path(), &[]).unwrap_err().code,
        "git_spawn"
    );
    assert_eq!(missing_runner.metrics().spawns, 0);
}

fn fifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}
fn receive_ready(path: &std::path::Path) {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"ready\n");
}
fn release_gate(path: &std::path::Path) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .write_all(b"go\n")
        .unwrap();
}
fn wait_for_queue(runner: &GitRunner, count: usize) {
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while runner.metrics().waiting != count {
        assert!(
            std::time::Instant::now() < end,
            "queue did not reach {count}: {:?}",
            runner.metrics()
        );
        std::thread::yield_now();
    }
}
fn gated_runner(path: &std::path::Path, max_children: usize) -> GitRunner {
    let script = path.join("fake-git");
    std::fs::write(&script,"#!/bin/sh\nwhile [ \"${1-}\" = -c ]; do shift 2; done\ncase \"$1\" in hold) printf 'ready\\n' >\"$2\"; IFS= read -r token <\"$3\"; printf 'done\\n';; *) printf '%s\\n' \"$1\" >>\"$2\"; printf '%s' \"$1\";; esac\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    GitRunner::with_limits(
        script,
        GitLimits {
            max_children,
            timeout: Duration::from_secs(10),
            ..GitLimits::default()
        },
    )
}
fn start_held(
    runner: &GitRunner,
    path: &std::path::Path,
) -> (
    std::thread::JoinHandle<aq_git_core::Result<aq_git_core::git::GitOutput>>,
    std::path::PathBuf,
) {
    let ready = path.join("ready");
    let gate = path.join("gate");
    fifo(&ready);
    fifo(&gate);
    let run = runner.clone();
    let cwd = path.to_path_buf();
    let gate_arg = gate.clone();
    let handle = std::thread::spawn(move || {
        run.run(
            &cwd,
            &[
                "hold".into(),
                ready.to_string_lossy().into_owned(),
                gate_arg.to_string_lossy().into_owned(),
            ],
        )
    });
    receive_ready(&path.join("ready"));
    (handle, gate)
}
#[test]
fn saturated_child_admission_waits_in_fifo_order() {
    let temp = tempfile::tempdir().unwrap();
    let runner = gated_runner(temp.path(), 1);
    let (held, gate) = start_held(&runner, temp.path());
    let log = temp.path().join("order");
    let mut jobs = Vec::new();
    for name in ["a", "b", "c"] {
        let run = runner.clone();
        let cwd = temp.path().to_path_buf();
        let output = log.clone();
        jobs.push(std::thread::spawn(move || {
            run.run(&cwd, &[name.into(), output.to_string_lossy().into_owned()])
        }));
        wait_for_queue(&runner, jobs.len());
    }
    assert_eq!(runner.metrics().active, 1);
    assert_eq!(runner.metrics().spawns, 1);
    release_gate(&gate);
    assert!(held.join().unwrap().unwrap().status.success());
    for job in jobs {
        assert!(job.join().unwrap().unwrap().status.success());
    }
    assert_eq!(std::fs::read_to_string(log).unwrap(), "a\nb\nc\n");
    let metrics = runner.metrics();
    assert_eq!(metrics.queued, 3);
    assert_eq!(metrics.peak_waiters, 3);
    assert_eq!(metrics.active, 0);
    assert_eq!(metrics.waiting, 0);
    assert!(metrics.queue_wait_micros > 0);
}
#[test]
fn expired_waiter_never_spawns_and_unblocks_next_ticket() {
    let temp = tempfile::tempdir().unwrap();
    let runner = gated_runner(temp.path(), 1);
    let (held, gate) = start_held(&runner, temp.path());
    let log = temp.path().join("order");
    let short = runner.clone();
    let cwd = temp.path().to_path_buf();
    let short_log = log.clone();
    let expired = std::thread::spawn(move || {
        short.run_with(
            &cwd,
            &["expired".into(), short_log.to_string_lossy().into_owned()],
            &[],
            &[],
            Duration::from_millis(40),
        )
    });
    assert_eq!(
        expired.join().unwrap().unwrap_err().code,
        "deadline_exceeded"
    );
    assert_eq!(runner.metrics().spawns, 1);
    assert_eq!(runner.metrics().queue_timeouts, 1);
    assert!(!log.exists());
    let next = runner.clone();
    let cwd = temp.path().to_path_buf();
    let next_log = log.clone();
    let successful = std::thread::spawn(move || {
        next.run(
            &cwd,
            &["next".into(), next_log.to_string_lossy().into_owned()],
        )
    });
    wait_for_queue(&runner, 1);
    release_gate(&gate);
    assert!(held.join().unwrap().unwrap().status.success());
    assert!(successful.join().unwrap().unwrap().status.success());
    assert_eq!(std::fs::read_to_string(log).unwrap(), "next\n");
    assert_eq!(runner.metrics().active, 0);
    assert_eq!(runner.metrics().waiting, 0);
}
#[test]
fn child_wait_queue_has_a_fixed_sixty_four_request_bound() {
    let temp = tempfile::tempdir().unwrap();
    let runner = gated_runner(temp.path(), 1);
    let (held, gate) = start_held(&runner, temp.path());
    let mut jobs = Vec::new();
    for _ in 0..aq_git_core::git::MAX_CHILD_WAITERS {
        let run = runner.clone();
        let cwd = temp.path().to_path_buf();
        jobs.push(std::thread::spawn(move || {
            run.run(
                &cwd,
                &[
                    "job".into(),
                    cwd.join("jobs").to_string_lossy().into_owned(),
                ],
            )
        }));
    }
    wait_for_queue(&runner, 64);
    assert_eq!(
        runner
            .run(
                temp.path(),
                &[
                    "overflow".into(),
                    temp.path().join("jobs").to_string_lossy().into_owned()
                ]
            )
            .unwrap_err()
            .code,
        "overloaded"
    );
    assert_eq!(runner.metrics().spawns, 1);
    assert_eq!(runner.metrics().active, 1);
    release_gate(&gate);
    assert!(held.join().unwrap().unwrap().status.success());
    for job in jobs {
        assert!(job.join().unwrap().unwrap().status.success());
    }
    assert_eq!(runner.metrics().peak_waiters, 64);
    assert_eq!(runner.metrics().active, 0);
    assert_eq!(runner.metrics().waiting, 0);
}
#[test]
fn event_driven_fallback_handles_bidirectional_pipe_pressure_and_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-git");
    std::fs::write(&script,"#!/bin/sh\ndd if=/dev/zero bs=4096 count=32 2>/dev/null\ndd if=/dev/zero bs=4096 count=32 1>&2 2>/dev/null\ncat\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = GitRunner::new(script.clone()).without_pidfd_for_testing();
    let input = vec![b'x'; 512 * 1024];
    let output = runner
        .run_with(temp.path(), &[], &input, &[], Duration::from_secs(3))
        .unwrap();
    assert!(output.status.success());
    assert_eq!(&output.stdout[..128 * 1024], vec![0; 128 * 1024]);
    assert_eq!(&output.stdout[128 * 1024..], input);
    assert_eq!(output.stderr, vec![0; 128 * 1024]);
    assert_eq!(runner.metrics().waiter_fallback_runs, 1);
    assert_eq!(runner.metrics().pidfd_runs, 0);
    assert_eq!(runner.metrics().active, 0);
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let runner = GitRunner::new(script).without_pidfd_for_testing();
    assert_eq!(
        runner
            .run_with(temp.path(), &[], &input, &[], Duration::from_millis(50))
            .unwrap_err()
            .code,
        "git_timeout"
    );
    assert_eq!(runner.metrics().waiter_fallback_runs, 1);
    assert_eq!(runner.metrics().active, 0);
}
#[test]
fn controlled_pack_profile_is_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    for (name, value) in [
        ("pack.threads", "2\n"),
        ("pack.windowMemory", "16m\n"),
        ("pack.deltaCacheSize", "16m\n"),
    ] {
        let output = runner
            .run(temp.path(), &["config".into(), "--get".into(), name.into()])
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, value.as_bytes());
    }
}
#[test]
fn persistent_reader_eviction_wakes_waiters_and_small_limits_keep_progress() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("objects.git");
    std::fs::create_dir(&repository).unwrap();
    let setup = GitRunner::new("/usr/bin/git".into());
    assert!(
        setup
            .run(&repository, &["init".into(), "--bare".into(), ".".into()])
            .unwrap()
            .status
            .success()
    );
    let tree = String::from_utf8(
        setup
            .run(
                &repository,
                &[
                    "hash-object".into(),
                    "-w".into(),
                    "-t".into(),
                    "tree".into(),
                    "--stdin".into(),
                ],
            )
            .unwrap()
            .stdout,
    )
    .unwrap();
    let env = vec![
        ("GIT_AUTHOR_NAME".into(), "Fixture".into()),
        ("GIT_AUTHOR_EMAIL".into(), "f@example.test".into()),
        ("GIT_COMMITTER_NAME".into(), "Fixture".into()),
        ("GIT_COMMITTER_EMAIL".into(), "f@example.test".into()),
        ("GIT_AUTHOR_DATE".into(), "@1700000000 +0000".into()),
        ("GIT_COMMITTER_DATE".into(), "@1700000000 +0000".into()),
    ];
    let commit = String::from_utf8(
        setup
            .run_with(
                &repository,
                &[
                    "commit-tree".into(),
                    tree.trim().into(),
                    "-m".into(),
                    "fixture".into(),
                ],
                &[],
                &env,
                Duration::from_secs(3),
            )
            .unwrap()
            .stdout,
    )
    .unwrap();
    let one = GitRunner::with_limits(
        "/usr/bin/git".into(),
        GitLimits {
            max_children: 1,
            ..GitLimits::default()
        },
    );
    let readers = aq_git_core::git::objects::ObjectReaders::default();
    assert_eq!(
        readers
            .object(&one, &repository, commit.trim(), 65536)
            .unwrap()
            .0,
        "commit"
    );
    assert_eq!(one.metrics().active, 0);
    assert!(
        one.run(&repository, &["--version".into()])
            .unwrap()
            .status
            .success()
    );
    let script = temp.path().join("mixed-git");
    std::fs::write(&script,"#!/bin/sh\nwhile [ \"${1-}\" = -c ]; do shift 2; done\ncase \"$1\" in hold) printf 'ready\\n' >\"$2\"; IFS= read -r token <\"$3\"; printf 'done\\n';; *) exec /usr/bin/git \"$@\";; esac\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = GitRunner::with_limits(
        script,
        GitLimits {
            max_children: 2,
            timeout: Duration::from_secs(10),
            ..GitLimits::default()
        },
    );
    assert_eq!(
        readers
            .object(&runner, &repository, commit.trim(), 65536)
            .unwrap()
            .0,
        "commit"
    );
    assert_eq!(runner.metrics().active, 1);
    let (held, gate) = start_held(&runner, temp.path());
    let queued = runner.clone();
    let cwd = temp.path().to_path_buf();
    let (send, receive) = std::sync::mpsc::channel();
    let waiter =
        std::thread::spawn(move || send.send(queued.run(&cwd, &["--version".into()])).unwrap());
    wait_for_queue(&runner, 1);
    readers.clear();
    assert!(
        receive
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(runner.metrics().active, 1);
    release_gate(&gate);
    assert!(held.join().unwrap().unwrap().status.success());
    waiter.join().unwrap();
    assert_eq!(runner.metrics().active, 0);
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
