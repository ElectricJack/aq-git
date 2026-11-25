use super::*;
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
// The spawn counter is bumped after `spawn()` returns, so a child can announce
// itself before the parent has counted it.
fn wait_for_spawns(runner: &GitRunner, count: u64) {
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while runner.metrics().spawns != count {
        assert!(
            std::time::Instant::now() < end,
            "spawns did not reach {count}: {:?}",
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
    wait_for_spawns(runner, 1);
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
    assert_eq!((metrics.queued, metrics.peak_waiters), (3, 3));
    assert_eq!((metrics.active, metrics.waiting), (0, 0));
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
    let metrics = runner.metrics();
    assert_eq!((metrics.spawns, metrics.queue_timeouts), (1, 1));
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
    let metrics = runner.metrics();
    assert_eq!((metrics.active, metrics.waiting), (0, 0));
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
    let metrics = runner.metrics();
    assert_eq!((metrics.active, metrics.waiting), (0, 0));
}
#[test]
fn persistent_reader_eviction_wakes_waiters_and_small_limits_keep_progress() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("objects.git");
    let commit = create_commit(&repository);
    let readers = aq_git_core::git::objects::ObjectReaders::default();
    assert_single_slot_progress(&readers, &repository, commit.trim());
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
fn create_commit(repository: &std::path::Path) -> String {
    std::fs::create_dir(repository).unwrap();
    let setup = GitRunner::new("/usr/bin/git".into());
    assert!(
        setup
            .run(repository, &["init".into(), "--bare".into(), ".".into()])
            .unwrap()
            .status
            .success()
    );
    let tree = String::from_utf8(
        setup
            .run(
                repository,
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
    String::from_utf8(
        setup
            .run_with(
                repository,
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
    .unwrap()
}
fn assert_single_slot_progress(
    readers: &aq_git_core::git::objects::ObjectReaders,
    repository: &std::path::Path,
    commit: &str,
) {
    let one = GitRunner::with_limits(
        "/usr/bin/git".into(),
        GitLimits {
            max_children: 1,
            ..GitLimits::default()
        },
    );
    assert_eq!(
        readers.object(&one, repository, commit, 65536).unwrap().0,
        "commit"
    );
    assert_eq!(one.metrics().active, 0);
    assert!(
        one.run(repository, &["--version".into()])
            .unwrap()
            .status
            .success()
    );
}
