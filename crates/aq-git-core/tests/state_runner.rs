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
#[path = "state_runner/admission.rs"]
mod admission;
#[path = "state_runner/durability.rs"]
mod durability;
#[test]
fn real_git_scrubbed_and_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    let out = runner.run(dir.path(), &["--version".into()]).unwrap();
    assert!(out.status.success());
    assert!(out.stdout.starts_with(b"git version"));
    let metrics = runner.metrics();
    assert_eq!((metrics.spawns, metrics.active), (1, 0));
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
    let metrics = runner.metrics();
    assert_eq!((metrics.spawns, metrics.active), (before, 0));
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
    let (zeros, echo) = output.stdout.split_at(128 * 1024);
    assert_eq!(zeros, vec![0; 128 * 1024]);
    assert_eq!(echo, input);
    assert_eq!(output.stderr, vec![0; 128 * 1024]);
    let metrics = runner.metrics();
    assert_eq!((metrics.waiter_fallback_runs, metrics.pidfd_runs), (1, 0));
    assert_eq!(metrics.active, 0);
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
