use aq_git_core::git::{GitLimits, GitRunner};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
const MARKER: &str = "__aq_git_spawn_v1";
fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_aq-git"))
}
fn identity(path: &Path) -> Value {
    let m = std::fs::metadata(path).unwrap();
    json!({"device":m.dev(),"inode":m.ino(),"bytes":m.len(),"mtime":m.mtime(),"mtime_nsec":m.mtime_nsec(),"ctime":m.ctime(),"ctime_nsec":m.ctime_nsec()})
}
fn hidden(git: &Path, expected: Value) -> Command {
    let mut command = Command::new(helper());
    command
        .arg(MARKER)
        .arg((64 * 1024 * 1024u64).to_string())
        .arg("none")
        .arg(expected.to_string())
        .arg(uuid::Uuid::new_v4().to_string())
        .arg(git);
    command
}
#[test]
fn helper_preserves_raw_os_arguments_without_lossy_conversion() {
    let git = Path::new("/usr/bin/git");
    let argument = OsString::from_vec(b"space quote' newline\nnon-utf8-\xff".to_vec());
    let reference = Command::new(git)
        .args(["rev-parse", "--sq-quote"])
        .arg(&argument)
        .output()
        .unwrap();
    assert!(reference.status.success());
    let output = hidden(git, identity(git))
        .args(["rev-parse", "--sq-quote"])
        .arg(argument)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, reference.stdout);
}
#[test]
fn factory_applies_memory_and_file_caps_before_non_utf8_git_exec() {
    let temp = tempfile::tempdir().unwrap();
    let git = temp
        .path()
        .join(OsString::from_vec(b"git-backend-\xff".to_vec()));
    std::fs::write(&git,br#"#!/bin/sh
while IFS= read -r line; do printf '%s\n' "$line"; done < /proc/self/limits
printf '<PROC>\n'
IFS= read -r process < /proc/self/stat
printf '%s\n' "$process"
printf '<ENV>\nHOME=%s\nGLOBAL=%s\nREPLACE=%s\n' "$HOME" "$GIT_CONFIG_GLOBAL" "$GIT_NO_REPLACE_OBJECTS"
printf '<ARGV>\n'
printf '%s\0' "$@"
"#).unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let limits = GitLimits {
        max_address_space_bytes: 64 * 1024 * 1024,
        ..GitLimits::default()
    };
    let runner = GitRunner::with_limits(git, limits)
        .with_spawn_helper(helper())
        .unwrap();
    let output = runner
        .run_with_disk_budget(
            temp.path(),
            &["first space".into(), "quote'line\n".into()],
            temp.path(),
            1024 * 1024,
        )
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let text = String::from_utf8_lossy(&output.stdout);
    for (name, expected) in [
        ("Max address space", 64 * 1024 * 1024u64),
        ("Max file size", 1024 * 1024u64),
    ] {
        let line = text.lines().find(|line| line.starts_with(name)).unwrap();
        let mut parts = line.strip_prefix(name).unwrap().split_whitespace();
        assert_eq!(parts.next().unwrap().parse::<u64>().unwrap(), expected);
        assert_eq!(parts.next().unwrap().parse::<u64>().unwrap(), expected);
    }
    assert!(text.contains("HOME=/nonexistent\nGLOBAL=/dev/null\nREPLACE=1\n"));
    let process = text
        .split("<PROC>\n")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    let (pid, rest) = process.split_once(" (").unwrap();
    let fields = rest
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    assert_eq!(
        fields[2], pid,
        "helper and Git must retain their owned process group"
    );
    assert!(output.stdout.ends_with(b"first space\0quote'line\n\0"));
    assert!(!text.contains(MARKER));
    assert_eq!(runner.metrics().active, 0);
}
#[test]
fn helper_setup_identity_and_exec_failures_are_closed() {
    let invalid = Command::new(helper()).arg(MARKER).output().unwrap();
    assert_eq!(invalid.status.code(), Some(126));
    assert!(invalid.stdout.is_empty());
    let git = Path::new("/usr/bin/git");
    let mut changed = identity(git);
    changed["bytes"] = json!(changed["bytes"].as_u64().unwrap() + 1);
    let refused = hidden(git, changed).arg("--version").output().unwrap();
    assert_eq!(refused.status.code(), Some(126));
    assert!(refused.stdout.is_empty());
    let temp = tempfile::tempdir().unwrap();
    let invalid_executable = temp.path().join("not-an-image");
    std::fs::write(&invalid_executable, b"not an executable image").unwrap();
    std::fs::set_permissions(&invalid_executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runner = GitRunner::new(invalid_executable)
        .with_spawn_helper(helper())
        .unwrap();
    let error = runner
        .run_with(
            temp.path(),
            &["--version".into()],
            &[],
            &[],
            Duration::from_secs(2),
        )
        .unwrap_err();
    assert_eq!(error.code, "git_spawn");
    assert!(!error.message.contains("not-an-image"));
    assert_eq!(runner.metrics().active, 0);
}

#[test]
fn replaced_helper_is_fenced_before_any_child_is_started() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let copy = temp.path().join("owned-helper");
    std::fs::copy(helper(), &copy).unwrap();
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runner = GitRunner::new("/usr/bin/git".into())
        .with_spawn_helper(copy.clone())
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(copy)
        .unwrap()
        .write_all(b"changed helper identity")
        .unwrap();
    let error = runner.run(temp.path(), &["--version".into()]).unwrap_err();
    assert_eq!(error.code, "unsupported_feature");
    assert_eq!(runner.metrics().spawns, 0);
    assert_eq!(runner.metrics().active, 0);
}

#[test]
fn persistent_reader_preserves_authenticated_helper_failure_classifiers() {
    use aq_git_core::git::objects::ObjectReaders;
    for persistent in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let git = temp.path().join("selected-git");
        let reached = temp.path().join("git-executed");
        let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        std::fs::write(
            &git,
            format!("#!/bin/sh\nprintf reached > {}\n", quote(&reached)),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let wrapper = temp.path().join("changing-helper");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '\\n# changed' >> {}\nexec {} \\\"$@\\\"\n",
                quote(&git),
                quote(&helper())
            )
            .replace("\\\"", "\""),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = GitRunner::new(git).with_spawn_helper(wrapper).unwrap();
        let error = if persistent {
            ObjectReaders::default()
                .object(&runner, temp.path(), &"a".repeat(40), 1024)
                .unwrap_err()
        } else {
            runner.run(temp.path(), &["--version".into()]).unwrap_err()
        };
        assert_eq!(error.code, "unsupported_feature");
        assert_eq!(error.details["restart_required"], true);
        assert!(!reached.exists());
        assert_eq!(runner.metrics().active, 0);
    }
    let temp = tempfile::tempdir().unwrap();
    let invalid = temp.path().join("not-an-image");
    std::fs::write(&invalid, b"not executable format").unwrap();
    std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runner = GitRunner::new(invalid).with_spawn_helper(helper()).unwrap();
    assert_eq!(
        ObjectReaders::default()
            .object(&runner, temp.path(), &"a".repeat(40), 1024)
            .unwrap_err()
            .code,
        "git_spawn"
    );
    assert_eq!(runner.metrics().active, 0);
}
#[test]
fn git_stderr_with_foreign_nonce_and_reserved_status_is_not_a_setup_failure() {
    use aq_git_core::git::objects::ObjectReaders;
    let temp = tempfile::tempdir().unwrap();
    let git = temp.path().join("foreign-nonce-git");
    std::fs::write(&git,b"#!/bin/sh\nprintf '\\000aq-git-spawn-v1\\000' >&2\ni=0; while [ $i -lt 16 ]; do printf '\\000' >&2; i=$((i+1)); done\nprintf 2 >&2\nexit 126\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runner = GitRunner::new(git).with_spawn_helper(helper()).unwrap();
    let ordinary = runner.run(temp.path(), &["--version".into()]).unwrap();
    assert_eq!(ordinary.status.code(), Some(126));
    let error = ObjectReaders::default()
        .object(&runner, temp.path(), &"a".repeat(40), 1024)
        .unwrap_err();
    assert_ne!(error.code, "unsupported_feature");
    assert_ne!(error.code, "git_spawn");
    assert_eq!(runner.metrics().active, 0);
}
