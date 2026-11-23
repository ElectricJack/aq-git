use super::*;
use std::time::{Duration, SystemTime};
/// Every file and directory below `root` whose name starts with one of `prefixes`.
fn entries_with_prefix(root: &Path, prefixes: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if prefixes.iter().any(|prefix| name.starts_with(prefix)) {
                found.push(entry.path());
            }
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            }
        }
    }
    found
}

#[test]
fn import_killed_mid_index_pack_leaves_no_temporary_pack_in_pool() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let marker = f._temp.path().join("index-pack.tmp-path");
    let script = f._temp.path().join("git-stalled-index-pack");
    // For `bundle unbundle`, start a real index-pack whose input never ends, wait
    // until it has created its temporary pack, record that path and block until
    // aq-git kills the process group at the request deadline.
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = unbundle ]; then\n    objects=\"${{GIT_OBJECT_DIRECTORY:-$PWD/objects}}\"\n    sleep 600 | /usr/bin/git index-pack --stdin &\n    i=0\n    while [ $i -lt 2000 ]; do\n      for tmp in \"$objects\"/pack/tmp_pack_*; do\n        if [ -f \"$tmp\" ]; then printf '%s\\n' \"$tmp\" >'{}'; wait; exit 1; fi\n      done\n      sleep 0.01; i=$((i+1))\n    done\n    exit 1\n  fi\ndone\nexec /usr/bin/git \"$@\"\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let state = f.receiver.root.as_ref().clone();
    f.receiver = RepositoryService::from_state_root_with_git(state.clone(), script).unwrap();
    let export = f.export(json!([]), false);
    let (handle, _) = f.stage(&export);
    let mut import = f.import(&handle);
    import.deadline_ms = Some(3_000);
    let error = f.receiver.execute(import.clone()).error.unwrap();
    assert_eq!(error.code, "deadline_exceeded", "{error:?}");
    let temporary =
        fs::read_to_string(&marker).expect("index-pack never created its temporary pack");
    let temporary = PathBuf::from(temporary.trim());
    assert!(!temporary.exists(), "killed child's {temporary:?} remains");
    let pool = f.receiver.repositories.pool_path(&f.repo);
    assert_eq!(
        entries_with_prefix(&pool, &["tmp_", "incoming-aq-"]),
        Vec::<PathBuf>::new()
    );
    // The pending operation replays through a fresh staging directory.
    f.receiver = RepositoryService::from_state_root_with_git(state, "/usr/bin/git".into()).unwrap();
    import.deadline_ms = None;
    assert_eq!(run(&f.receiver, import)["source"], oid(&f.second));
    assert_eq!(
        entries_with_prefix(&pool, &["tmp_", "incoming-aq-"]),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn temp_pack_sweep_removes_only_stale_unopened_temporary_files() {
    use aq_git_core::temp_packs::TempPackSweepConfig;
    let f = Fixture::new();
    assert_eq!(TempPackSweepConfig::default().max_age_secs, 3600);
    let objects = f.source.repositories.pool_path(&f.repo).join("objects");
    let pack = objects.join("pack");
    fs::create_dir_all(&pack).unwrap();
    let hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
    let age = |path: &Path, time: SystemTime| {
        fs::File::open(path).unwrap().set_modified(time).unwrap();
    };
    let write = |name: &str, bytes: &[u8], time: SystemTime| {
        let path = pack.join(name);
        fs::write(&path, bytes).unwrap();
        age(&path, time);
        path
    };
    let old = write("tmp_pack_OldOld", b"partial pack bytes", hours_ago);
    let fresh = write(
        "tmp_pack_Fresh1",
        b"in flight",
        SystemTime::now() - Duration::from_secs(600),
    );
    let held = write("tmp_idx_Held01", b"held open", hours_ago);
    let real = [
        write(&format!("pack-{}.pack", "a".repeat(40)), b"PACK", hours_ago),
        write(&format!("pack-{}.idx", "a".repeat(40)), b"IDX", hours_ago),
        write(&format!("pack-{}.keep", "a".repeat(40)), b"", hours_ago),
    ];
    let staging = objects.join(format!("incoming-aq-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(staging.join("pack")).unwrap();
    fs::write(staging.join("pack/tmp_pack_Crash1"), b"orphan").unwrap();
    age(&staging, hours_ago);
    let mut holder = hold_stale_index(&held);
    configure_sweep(&f.source, 60);
    let report = f.source.sweep_stale_temp_packs().unwrap();
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(!old.exists());
    assert!(!staging.exists());
    assert!(fresh.exists() && held.exists());
    for path in &real {
        assert!(path.exists(), "{path:?} was removed");
    }
    assert_eq!(
        (report.files_removed, report.bytes_removed),
        (1, 18 + 6),
        "{report:?}"
    );
    assert_eq!(
        (report.files_kept_open, report.staging_directories_removed),
        (1, 1),
        "{report:?}"
    );
    assert_eq!(report.errors, 0, "{report:?}");
    configure_sweep(&f.source, 0);
    assert_eq!(
        f.source.sweep_stale_temp_packs().unwrap_err().code,
        "invalid_configuration"
    );
}

fn hold_stale_index(held: &Path) -> std::process::Child {
    let holder = Command::new("/bin/sh")
        .args(["-c", "exec 3<\"$0\"; exec sleep 60"])
        .arg(held)
        .spawn()
        .unwrap();
    let descriptor = PathBuf::from(format!("/proc/{}/fd/3", holder.id()));
    for _ in 0..500 {
        if fs::read_link(&descriptor).is_ok_and(|target| target == held) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    holder
}

fn configure_sweep(service: &RepositoryService, interval_secs: u64) {
    service
        .root
        .write_json(
            "temp_pack_sweep.json",
            &json!({"interval_secs": interval_secs,"max_age_secs":1800}),
        )
        .unwrap();
}
