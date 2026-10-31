use aq_git_core::{
    git::{
        GitRunner,
        capabilities::{cached_report, probe},
    },
    state::StateRoot,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn operation() -> String {
    uuid::Uuid::new_v4().to_string()
}
#[test]
fn real_git_probes_cache_exact_fingerprint_and_clean_acknowledged_crash_remnants() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::open(temp.path().join("state")).unwrap();
    let executable_path = temp.path().join("git");
    executable(&executable_path, "#!/bin/sh\nexec /usr/bin/git \"$@\"\n");
    let runner = GitRunner::new(executable_path.clone());
    let op = operation();
    let report = probe(&root, &runner, &op).unwrap();
    assert_eq!(report["features"].as_array().unwrap().len(), 7);
    let spawns = runner.metrics().spawns;
    assert!(spawns > 20);
    assert_eq!(cached_report(&root, &runner).unwrap().unwrap(), report);
    assert_eq!(probe(&root, &runner, &operation()).unwrap(), report);
    assert_eq!(runner.metrics().spawns, spawns);
    let leftover = root.create_dir(format!("probes/{op}/source.git")).unwrap();
    fs::write(leftover.join("leftover"), "crash after report persistence").unwrap();
    assert_eq!(probe(&root, &runner, &op).unwrap(), report);
    assert!(!root.path().join(format!("probes/{op}")).exists());
    assert_eq!(runner.metrics().spawns, spawns);
    executable(
        &executable_path,
        "#!/bin/sh\nfor arg do if [ \"$arg\" = --version ]; then printf 'git version 2.42.9\\n'; exit 0; fi; done\nexec /usr/bin/git \"$@\"\n",
    );
    assert!(cached_report(&root, &runner).unwrap().is_none());
    assert_eq!(
        probe(&root, &runner, &operation()).unwrap_err().code,
        "unsupported_feature"
    );
}
#[test]
fn incomplete_git_and_ignored_expected_old_leases_are_not_advertised() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::open(temp.path().join("state")).unwrap();
    let fake = temp.path().join("incomplete-git");
    executable(
        &fake,
        "#!/bin/sh\nfor arg do if [ \"$arg\" = --batch-command ]; then printf 'incomplete framing\\n'; exit 0; fi; done\nexec /usr/bin/git \"$@\"\n",
    );
    let runner = GitRunner::new(fake);
    assert_eq!(
        probe(&root, &runner, &operation()).unwrap_err().code,
        "unsupported_feature"
    );
    assert!(
        root.read_json::<serde_json::Value>("git-capabilities.json")
            .unwrap()
            .is_none()
    );
    let fake = temp.path().join("lease-ignoring-git");
    executable(
        &fake,
        "#!/bin/bash\nargs=()\nfor arg in \"$@\"; do case \"$arg\" in --force-with-lease=*) ;; *) args+=(\"$arg\");; esac; done\nexec /usr/bin/git \"${args[@]}\"\n",
    );
    let runner = GitRunner::new(fake);
    let error = probe(&root, &runner, &operation()).unwrap_err();
    assert_eq!(error.code, "unsupported_feature");
    assert!(error.message.contains("expected-old"));
    assert!(
        root.read_json::<serde_json::Value>("git-capabilities.json")
            .unwrap()
            .is_none()
    );
}
#[test]
fn unowned_probe_directories_and_invalid_cached_reports_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::open(temp.path().join("state")).unwrap();
    let runner = GitRunner::new("/usr/bin/git".into());
    let op = operation();
    let unowned = root.create_dir(format!("probes/{op}")).unwrap();
    fs::write(unowned.join("preserve"), "unrelated private files").unwrap();
    assert_eq!(
        probe(&root, &runner, &op).unwrap_err().code,
        "recovery_blocked"
    );
    assert!(unowned.join("preserve").exists());
    assert_eq!(runner.metrics().spawns, 0);
    let report = serde_json::json!({"probe_version":1,"fingerprint":aq_git_core::git::capabilities::executable_fingerprint(&runner).unwrap(),"git_version":"git version 2.43.0","verified_at":"fixture","features":["sha1"]});
    root.write_json("git-capabilities.json", &report).unwrap();
    assert!(cached_report(&root, &runner).unwrap().is_none());
}
