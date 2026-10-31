//! Behavioral checks against private disposable repositories, never a bound remote.
use super::GitRunner;
use crate::{Error, Result, state::StateRoot};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};
const PROBE_VERSION: u32 = 1;
const FEATURES: &[&str] = &[
    "sha1",
    "batch_command",
    "nul_tree_diff",
    "merge_tree_write_tree",
    "bundle_verify",
    "ref_transactions",
    "explicit_expected_old_lease",
];

pub fn executable_fingerprint(runner: &GitRunner) -> Result<Value> {
    if !runner.git.is_absolute() {
        return Err(Error::new(
            "invalid_git",
            "configured Git path must be absolute",
        ));
    }
    let metadata = fs::metadata(&runner.git)?;
    if !metadata.is_file() {
        return Err(Error::new(
            "unsupported_feature",
            "configured Git is not a regular executable",
        ));
    }
    Ok(
        json!({"path":runner.git,"device":metadata.dev(),"inode":metadata.ino(),"bytes":metadata.len(),"mtime":metadata.mtime(),"mtime_nsec":metadata.mtime_nsec(),"ctime":metadata.ctime(),"ctime_nsec":metadata.ctime_nsec()}),
    )
}
fn supported_version(version: &str) -> bool {
    let mut parts = version
        .strip_prefix("git version ")
        .unwrap_or("")
        .split('.');
    matches!((parts.next().and_then(|s|s.parse::<u64>().ok()),parts.next().and_then(|s|s.parse::<u64>().ok())),(Some(major),Some(minor)) if major>2||(major==2&&minor>=43))
}
pub fn cached_report(root: &StateRoot, runner: &GitRunner) -> Result<Option<Value>> {
    let fingerprint = executable_fingerprint(runner)?;
    let report: Option<Value> = root.read_json("git-capabilities.json")?;
    Ok(report.filter(|report| {
        report["probe_version"] == PROBE_VERSION
            && report["fingerprint"] == fingerprint
            && report["git_version"]
                .as_str()
                .is_some_and(supported_version)
            && report["verified_at"].is_string()
            && FEATURES.iter().all(|feature| {
                report["features"]
                    .as_array()
                    .is_some_and(|features| features.contains(&json!(feature)))
            })
    }))
}
fn probe_directory(root: &StateRoot, operation: &str) -> PathBuf {
    root.path().join("probes").join(operation)
}
fn clean_owned(root: &StateRoot, operation: &str) -> Result<()> {
    let directory = probe_directory(root, operation);
    match directory.symlink_metadata() {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    root.create_dir("probes")?;
    crate::operations::checked_directory(&directory)?;
    let owner: Option<Value> = root.read_json(format!("probes/{operation}.json"))?;
    if owner != Some(json!({"probe_version":PROBE_VERSION,"operation_id":operation})) {
        return Err(Error::new(
            "recovery_blocked",
            "probe directory has no matching durable owner",
        ));
    }
    fs::remove_dir_all(&directory)?;
    crate::operations::sync_dir(directory.parent().unwrap())
}
struct Cleanup<'a> {
    root: &'a StateRoot,
    operation: &'a str,
}
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = clean_owned(self.root, self.operation);
    }
}
fn unsupported(message: &str) -> Error {
    Error::new("unsupported_feature", message)
}
fn oid(output: &[u8]) -> Result<String> {
    let value = std::str::from_utf8(output)
        .map_err(|_| unsupported("Git probe returned non-text object ID"))?
        .trim();
    aq_git_protocol::ObjectId {
        algorithm: "sha1".into(),
        hex: value.into(),
    }
    .validate()
    .map_err(|_| unsupported("Git probe returned an invalid SHA-1 object ID"))?;
    Ok(value.into())
}

pub fn probe(root: &StateRoot, runner: &GitRunner, operation: &str) -> Result<Value> {
    crate::operations::id(operation)?;
    // This also removes a probe left after its report was durable but cleanup was interrupted.
    clean_owned(root, operation)?;
    if let Some(report) = cached_report(root, runner)? {
        return Ok(report);
    }
    let fingerprint = executable_fingerprint(runner)?;
    root.write_json(
        format!("probes/{operation}.json"),
        &json!({"probe_version":PROBE_VERSION,"operation_id":operation}),
    )?;
    let directory = root.create_dir(format!("probes/{operation}"))?;
    let _cleanup = Cleanup { root, operation };
    let source = root.create_dir(format!("probes/{operation}/source.git"))?;
    let remote = root.create_dir(format!("probes/{operation}/remote.git"))?;
    let run = |path: &Path,
               args: Vec<String>,
               input: &[u8],
               env: &[(String, String)]|
     -> Result<Vec<u8>> {
        let output = runner.run_with(path, &args, input, env, Duration::from_secs(30))?;
        if !output.status.success() {
            return Err(
                unsupported("installed Git failed a required behavior probe")
                    .details(json!({"command":args.first(),"exit_code":output.status.code()})),
            );
        }
        Ok(output.stdout)
    };
    let version = String::from_utf8_lossy(&run(root.path(), vec!["--version".into()], &[], &[])?)
        .trim()
        .to_owned();
    if !supported_version(&version) {
        return Err(unsupported("Git 2.43 or newer is required"));
    }
    for path in [&source, &remote] {
        run(
            path,
            vec![
                "init".into(),
                "--bare".into(),
                "--object-format=sha1".into(),
                ".".into(),
            ],
            &[],
            &[],
        )?;
        if run(
            path,
            vec!["rev-parse".into(), "--show-object-format".into()],
            &[],
            &[],
        )? != b"sha1\n"
        {
            return Err(unsupported("Git SHA-1 object format probe failed"));
        }
    }
    let empty = oid(&run(
        &source,
        vec![
            "hash-object".into(),
            "-w".into(),
            "-t".into(),
            "tree".into(),
            "--stdin".into(),
        ],
        &[],
        &[],
    )?)?;
    let content = b"probe content\n";
    let blob = oid(&run(
        &source,
        vec!["hash-object".into(), "-w".into(), "--stdin".into()],
        content,
        &[],
    )?)?;
    let filename = b"probe\tline\nname-\xff";
    let entry = |name: &[u8]| {
        let mut entry = format!("100644 blob {blob}\t").into_bytes();
        entry.extend_from_slice(name);
        entry.push(0);
        entry
    };
    let tree = oid(&run(
        &source,
        vec!["mktree".into(), "-z".into()],
        &entry(filename),
        &[],
    )?)?;
    let other = oid(&run(
        &source,
        vec!["mktree".into(), "-z".into()],
        &entry(b"other"),
        &[],
    )?)?;
    let mut combined_entries = entry(b"other");
    combined_entries.extend(entry(filename));
    let combined = oid(&run(
        &source,
        vec!["mktree".into(), "-z".into()],
        &combined_entries,
        &[],
    )?)?;
    let env = vec![
        ("GIT_AUTHOR_NAME".into(), "aq-git probe".into()),
        ("GIT_AUTHOR_EMAIL".into(), "probe@example.invalid".into()),
        ("GIT_COMMITTER_NAME".into(), "aq-git probe".into()),
        ("GIT_COMMITTER_EMAIL".into(), "probe@example.invalid".into()),
        ("GIT_AUTHOR_DATE".into(), "@1700000000 +0000".into()),
        ("GIT_COMMITTER_DATE".into(), "@1700000000 +0000".into()),
    ];
    let commit = |tree: &str, parents: &[String], message: &str| -> Result<String> {
        let mut args = vec![
            "commit-tree".into(),
            tree.into(),
            "-m".into(),
            message.into(),
        ];
        for parent in parents {
            args.extend(["-p".into(), parent.clone()]);
        }
        oid(&run(&source, args, &[], &env)?)
    };
    let first = commit(&empty, &[], "probe base")?;
    let second = commit(&tree, std::slice::from_ref(&first), "probe child")?;
    let third = commit(&other, std::slice::from_ref(&first), "probe branch")?;
    let successor = commit(&combined, std::slice::from_ref(&second), "probe successor")?;
    let first_body = run(
        &source,
        vec!["cat-file".into(), "commit".into(), first.clone()],
        &[],
        &[],
    )?;
    let second_body = run(
        &source,
        vec!["cat-file".into(), "commit".into(), second.clone()],
        &[],
        &[],
    )?;
    let batch = run(
        &source,
        vec!["cat-file".into(), "--batch-command".into()],
        format!("info {first}\ncontents {second}\ncontents {blob}\n").as_bytes(),
        &[],
    )?;
    let mut expected = format!(
        "{first} commit {}\n{second} commit {}\n",
        first_body.len(),
        second_body.len()
    )
    .into_bytes();
    expected.extend(&second_body);
    expected.push(b'\n');
    expected.extend(format!("{blob} blob {}\n", content.len()).as_bytes());
    expected.extend(content);
    expected.push(b'\n');
    if batch != expected {
        return Err(unsupported("Git batch object framing probe failed"));
    }
    if run(
        &source,
        vec!["ls-tree".into(), "-rz".into(), second.clone()],
        &[],
        &[],
    )? != entry(filename)
    {
        return Err(unsupported("Git NUL tree byte preservation probe failed"));
    }
    let mut expected_diff = format!(":000000 100644 {} {blob} A\0", "0".repeat(40)).into_bytes();
    expected_diff.extend(filename);
    expected_diff.push(0);
    if run(
        &source,
        vec![
            "diff".into(),
            "--raw".into(),
            "--no-abbrev".into(),
            "-z".into(),
            first.clone(),
            second.clone(),
        ],
        &[],
        &[],
    )? != expected_diff
    {
        return Err(unsupported("Git NUL diff byte preservation probe failed"));
    }
    if run(
        &source,
        vec![
            "merge-tree".into(),
            "--write-tree".into(),
            "--name-only".into(),
            "--no-messages".into(),
            "-z".into(),
            second.clone(),
            third,
        ],
        &[],
        &[],
    )? != format!("{combined}\0").as_bytes()
    {
        return Err(unsupported(
            "Git bare ordered merge probe produced an unexpected tree",
        ));
    }
    let zero = "0".repeat(40);
    let transaction = format!(
        "start\nupdate refs/heads/probe {second} {zero}\nupdate refs/heads/peer {first} {zero}\nprepare\ncommit\n"
    );
    run(
        &source,
        vec!["update-ref".into(), "--stdin".into()],
        transaction.as_bytes(),
        &[],
    )?;
    let rejected=runner.run_with(&source,&["update-ref".into(),"--stdin".into()],format!("start\nupdate refs/heads/probe {successor} {second}\nupdate refs/heads/peer {second} {zero}\nprepare\ncommit\n").as_bytes(),&[],Duration::from_secs(30))?;
    if rejected.status.success()
        || oid(&run(
            &source,
            vec!["rev-parse".into(), "refs/heads/probe".into()],
            &[],
            &[],
        )?)? != second
        || oid(&run(
            &source,
            vec!["rev-parse".into(), "refs/heads/peer".into()],
            &[],
            &[],
        )?)? != first
    {
        return Err(unsupported(
            "Git expected-old atomic ref transaction probe failed",
        ));
    }
    let bundle = run(
        &source,
        vec![
            "bundle".into(),
            "create".into(),
            "-".into(),
            "refs/heads/probe".into(),
        ],
        &[],
        &[],
    )?;
    let bundle_path = directory.join("probe.bundle");
    root.write_bytes(format!("probes/{operation}/probe.bundle"), &bundle)?;
    if run(
        &source,
        vec![
            "bundle".into(),
            "list-heads".into(),
            bundle_path.to_string_lossy().into_owned(),
        ],
        &[],
        &[],
    )? != format!("{second} refs/heads/probe\n").as_bytes()
    {
        return Err(unsupported("Git bundle advertised ref probe failed"));
    }
    run(
        &remote,
        vec![
            "bundle".into(),
            "verify".into(),
            bundle_path.to_string_lossy().into_owned(),
        ],
        &[],
        &[],
    )?;
    run(
        &remote,
        vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            bundle_path.to_string_lossy().into_owned(),
            "refs/heads/probe:refs/heads/probe".into(),
        ],
        &[],
        &[],
    )?;
    let target = "refs/heads/probe";
    let wrong = runner.run(
        &source,
        &[
            "push".into(),
            "--porcelain".into(),
            format!("--force-with-lease={target}:{first}"),
            remote.to_string_lossy().into_owned(),
            format!("{successor}:{target}"),
        ],
    )?;
    if wrong.status.success()
        || oid(&run(
            &remote,
            vec!["rev-parse".into(), "--verify".into(), target.into()],
            &[],
            &[],
        )?)? != second
    {
        return Err(unsupported("Git ignored explicit expected-old push lease"));
    }
    run(
        &source,
        vec![
            "push".into(),
            "--porcelain".into(),
            format!("--force-with-lease={target}:{second}"),
            remote.to_string_lossy().into_owned(),
            format!("{successor}:{target}"),
        ],
        &[],
        &[],
    )?;
    if oid(&run(
        &remote,
        vec!["rev-parse".into(), "--verify".into(), target.into()],
        &[],
        &[],
    )?)? != successor
    {
        return Err(unsupported(
            "Git conditional push failed to update its private destination",
        ));
    }
    if executable_fingerprint(runner)? != fingerprint {
        return Err(Error::new(
            "recovery_blocked",
            "configured Git executable changed during capability probes",
        ));
    }
    let report = json!({"probe_version":PROBE_VERSION,"fingerprint":fingerprint,"git_version":version,"features":FEATURES,"verified_at":chrono::Utc::now().to_rfc3339()});
    root.write_json("git-capabilities.json", &report)?;
    clean_owned(root, operation)?;
    Ok(report)
}
