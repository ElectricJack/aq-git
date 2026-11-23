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

const PROBE_CONTENT: &[u8] = b"probe content\n";
const PROBE_FILENAME: &[u8] = b"probe\tline\nname-\xff";

fn tree_entry(blob: &str, name: &[u8]) -> Vec<u8> {
    let mut entry = format!("100644 blob {blob}\t").into_bytes();
    entry.extend_from_slice(name);
    entry.push(0);
    entry
}

struct ProbeTrees {
    empty: String,
    blob: String,
    tree: String,
    other: String,
    combined: String,
}
struct ProbeObjects {
    first: String,
    second: String,
    third: String,
    successor: String,
    combined: String,
    blob: String,
}
struct ProbeRepositories<'a> {
    runner: &'a GitRunner,
    source: PathBuf,
    remote: PathBuf,
}
fn checked_output(args: &[String], output: super::runner::GitOutput) -> Result<Vec<u8>> {
    if !output.status.success() {
        return Err(
            unsupported("installed Git failed a required behavior probe")
                .details(json!({"command":args.first(),"exit_code":output.status.code()})),
        );
    }
    Ok(output.stdout)
}

impl ProbeRepositories<'_> {
    fn run(&self, path: &Path, args: Vec<String>, input: &[u8]) -> Result<Vec<u8>> {
        let output = self
            .runner
            .run_with(path, &args, input, &[], Duration::from_secs(30))?;
        checked_output(&args, output)
    }
    fn check_version(&self, root: &Path) -> Result<String> {
        let version = String::from_utf8_lossy(&self.run(root, vec!["--version".into()], &[])?)
            .trim()
            .to_owned();
        if !supported_version(&version) {
            return Err(unsupported("Git 2.43 or newer is required"));
        }
        Ok(version)
    }
    fn init_sha1(&self) -> Result<()> {
        for path in [&self.source, &self.remote] {
            self.run(
                path,
                vec![
                    "init".into(),
                    "--bare".into(),
                    "--object-format=sha1".into(),
                    ".".into(),
                ],
                &[],
            )?;
            if self.run(
                path,
                vec!["rev-parse".into(), "--show-object-format".into()],
                &[],
            )? != b"sha1\n"
            {
                return Err(unsupported("Git SHA-1 object format probe failed"));
            }
        }
        Ok(())
    }
    fn make_trees(&self) -> Result<ProbeTrees> {
        let empty = oid(&self.run(
            &self.source,
            vec![
                "hash-object".into(),
                "-w".into(),
                "-t".into(),
                "tree".into(),
                "--stdin".into(),
            ],
            &[],
        )?)?;
        let blob = oid(&self.run(
            &self.source,
            vec!["hash-object".into(), "-w".into(), "--stdin".into()],
            PROBE_CONTENT,
        )?)?;
        let tree = oid(&self.run(
            &self.source,
            vec!["mktree".into(), "-z".into()],
            &tree_entry(&blob, PROBE_FILENAME),
        )?)?;
        let other = oid(&self.run(
            &self.source,
            vec!["mktree".into(), "-z".into()],
            &tree_entry(&blob, b"other"),
        )?)?;
        let mut combined_entries = tree_entry(&blob, b"other");
        combined_entries.extend(tree_entry(&blob, PROBE_FILENAME));
        let combined = oid(&self.run(
            &self.source,
            vec!["mktree".into(), "-z".into()],
            &combined_entries,
        )?)?;
        Ok(ProbeTrees {
            empty,
            blob,
            tree,
            other,
            combined,
        })
    }
    fn make_commits(&self, trees: ProbeTrees) -> Result<ProbeObjects> {
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
            let output =
                self.runner
                    .run_with(&self.source, &args, &[], &env, Duration::from_secs(30))?;
            oid(&checked_output(&args, output)?)
        };
        let first = commit(&trees.empty, &[], "probe base")?;
        let second = commit(&trees.tree, std::slice::from_ref(&first), "probe child")?;
        let third = commit(&trees.other, std::slice::from_ref(&first), "probe branch")?;
        let successor = commit(
            &trees.combined,
            std::slice::from_ref(&second),
            "probe successor",
        )?;
        Ok(ProbeObjects {
            first,
            second,
            third,
            successor,
            blob: trees.blob,
            combined: trees.combined,
        })
    }
    fn check_batch(&self, objects: &ProbeObjects) -> Result<()> {
        let ProbeObjects {
            first,
            second,
            blob,
            ..
        } = objects;
        let first_body = self.run(
            &self.source,
            vec!["cat-file".into(), "commit".into(), objects.first.clone()],
            &[],
        )?;
        let second_body = self.run(
            &self.source,
            vec!["cat-file".into(), "commit".into(), objects.second.clone()],
            &[],
        )?;
        let batch = self.run(
            &self.source,
            vec!["cat-file".into(), "--batch-command".into()],
            format!("info {first}\ncontents {second}\ncontents {blob}\n").as_bytes(),
        )?;
        let mut expected = format!(
            "{first} commit {}\n{second} commit {}\n",
            first_body.len(),
            second_body.len()
        )
        .into_bytes();
        expected.extend(&second_body);
        expected.push(b'\n');
        expected.extend(format!("{blob} blob {}\n", PROBE_CONTENT.len()).as_bytes());
        expected.extend(PROBE_CONTENT);
        expected.push(b'\n');
        if batch != expected {
            return Err(unsupported("Git batch object framing probe failed"));
        }
        Ok(())
    }
    fn check_tree_diff_merge(&self, objects: &ProbeObjects) -> Result<()> {
        let ProbeObjects { blob, combined, .. } = objects;
        if self.run(
            &self.source,
            vec!["ls-tree".into(), "-rz".into(), objects.second.clone()],
            &[],
        )? != tree_entry(blob, PROBE_FILENAME)
        {
            return Err(unsupported("Git NUL tree byte preservation probe failed"));
        }
        let mut expected_diff =
            format!(":000000 100644 {} {blob} A\0", "0".repeat(40)).into_bytes();
        expected_diff.extend(PROBE_FILENAME);
        expected_diff.push(0);
        if self.run(
            &self.source,
            vec![
                "diff".into(),
                "--raw".into(),
                "--no-abbrev".into(),
                "-z".into(),
                objects.first.clone(),
                objects.second.clone(),
            ],
            &[],
        )? != expected_diff
        {
            return Err(unsupported("Git NUL diff byte preservation probe failed"));
        }
        if self.run(
            &self.source,
            vec![
                "merge-tree".into(),
                "--write-tree".into(),
                "--name-only".into(),
                "--no-messages".into(),
                "-z".into(),
                objects.second.clone(),
                objects.third.clone(),
            ],
            &[],
        )? != format!("{combined}\0").as_bytes()
        {
            return Err(unsupported(
                "Git bare ordered merge probe produced an unexpected tree",
            ));
        }
        Ok(())
    }
    fn check_ref_transactions(&self, objects: &ProbeObjects) -> Result<()> {
        let ProbeObjects {
            first,
            second,
            successor,
            ..
        } = objects;
        let zero = "0".repeat(40);
        let transaction = format!(
            "start\nupdate refs/heads/probe {second} {zero}\nupdate refs/heads/peer {first} {zero}\nprepare\ncommit\n"
        );
        self.run(
            &self.source,
            vec!["update-ref".into(), "--stdin".into()],
            transaction.as_bytes(),
        )?;
        let rejected=self.runner.run_with(&self.source,&["update-ref".into(),"--stdin".into()],format!("start\nupdate refs/heads/probe {successor} {second}\nupdate refs/heads/peer {second} {zero}\nprepare\ncommit\n").as_bytes(),&[],Duration::from_secs(30))?;
        let unchanged = |name: &str, expected: &str| -> Result<bool> {
            let actual = self.run(&self.source, vec!["rev-parse".into(), name.into()], &[])?;
            Ok(oid(&actual)? == expected)
        };
        if rejected.status.success() {
            return Err(unsupported(
                "Git expected-old atomic ref transaction probe failed",
            ));
        }
        if !unchanged("refs/heads/probe", second)? || !unchanged("refs/heads/peer", first)? {
            return Err(unsupported(
                "Git expected-old atomic ref transaction probe failed",
            ));
        }
        Ok(())
    }
    fn check_bundle(&self, root: &StateRoot, relative: String, second: &str) -> Result<()> {
        let bundle = self.run(
            &self.source,
            vec![
                "bundle".into(),
                "create".into(),
                "-".into(),
                "refs/heads/probe".into(),
            ],
            &[],
        )?;
        let bundle_path = root.path().join(&relative);
        root.write_bytes(relative, &bundle)?;
        if self.run(
            &self.source,
            vec![
                "bundle".into(),
                "list-heads".into(),
                bundle_path.to_string_lossy().into_owned(),
            ],
            &[],
        )? != format!("{second} refs/heads/probe\n").as_bytes()
        {
            return Err(unsupported("Git bundle advertised ref probe failed"));
        }
        self.run(
            &self.remote,
            vec![
                "bundle".into(),
                "verify".into(),
                bundle_path.to_string_lossy().into_owned(),
            ],
            &[],
        )?;
        self.run(
            &self.remote,
            vec![
                "fetch".into(),
                "--no-tags".into(),
                "--no-write-fetch-head".into(),
                bundle_path.to_string_lossy().into_owned(),
                "refs/heads/probe:refs/heads/probe".into(),
            ],
            &[],
        )?;
        Ok(())
    }
    fn check_push_lease(&self, objects: &ProbeObjects) -> Result<()> {
        let ProbeObjects {
            first,
            second,
            successor,
            ..
        } = objects;
        let target = "refs/heads/probe";
        let wrong = self.runner.run(
            &self.source,
            &[
                "push".into(),
                "--porcelain".into(),
                format!("--force-with-lease={target}:{first}"),
                self.remote.to_string_lossy().into_owned(),
                format!("{successor}:{target}"),
            ],
        )?;
        if wrong.status.success()
            || oid(&self.run(
                &self.remote,
                vec!["rev-parse".into(), "--verify".into(), target.into()],
                &[],
            )?)? != *second
        {
            return Err(unsupported("Git ignored explicit expected-old push lease"));
        }
        self.run(
            &self.source,
            vec![
                "push".into(),
                "--porcelain".into(),
                format!("--force-with-lease={target}:{second}"),
                self.remote.to_string_lossy().into_owned(),
                format!("{successor}:{target}"),
            ],
            &[],
        )?;
        if oid(&self.run(
            &self.remote,
            vec!["rev-parse".into(), "--verify".into(), target.into()],
            &[],
        )?)? != *successor
        {
            return Err(unsupported(
                "Git conditional push failed to update its private destination",
            ));
        }
        Ok(())
    }
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
    root.create_dir(format!("probes/{operation}"))?;
    let _cleanup = Cleanup { root, operation };
    let source = root.create_dir(format!("probes/{operation}/source.git"))?;
    let remote = root.create_dir(format!("probes/{operation}/remote.git"))?;
    let repos = ProbeRepositories {
        runner,
        source,
        remote,
    };
    let version = repos.check_version(root.path())?;
    repos.init_sha1()?;
    let trees = repos.make_trees()?;
    let objects = repos.make_commits(trees)?;
    repos.check_batch(&objects)?;
    repos.check_tree_diff_merge(&objects)?;
    repos.check_ref_transactions(&objects)?;
    repos.check_bundle(
        root,
        format!("probes/{operation}/probe.bundle"),
        &objects.second,
    )?;
    repos.check_push_lease(&objects)?;
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
