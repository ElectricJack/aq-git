//! The `pre-receive` / `reference-transaction` hooks of a task push.
//!
//! `aq-git serve` installs two tiny shell scripts that re-run its own executable
//! with [`HOOK_MARKER`]. Git runs `pre-receive` inside its object quarantine, so a
//! rejected push leaves no object or ref behind. The `reference-transaction` hook
//! runs after Git promoted the objects and just before it moves the ref: it
//! journals the update durably, and refuses it when the journal cannot be written.
//! The grant and limits arrive in the `AQ_GIT_HTTP_GRANT` environment variable of
//! the `git receive-pack` child.
use super::{Grant, PushJournal, ZERO_OID, journal_file};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) const HOOK_MARKER: &str = "__aq_git_hook_v2";
pub(crate) const GRANT_ENV: &str = "AQ_GIT_HTTP_GRANT";
/// Hooks this module installs, named as Git names them.
pub(crate) const HOOKS: &[&str] = &["pre-receive", "reference-transaction"];

/// Everything the hook needs; serialized into [`GRANT_ENV`] by the server.
#[derive(Serialize, Deserialize)]
pub(crate) struct HookConfig {
    pub grant: Grant,
    pub git: PathBuf,
    pub max_expanded_bytes: u64,
    /// Head of the main ref as the endpoint serves it: a base the client may have used.
    pub main_head: Option<String>,
    /// Object count in the pack header of the request, if it carried a pack.
    pub pack_objects: Option<u32>,
    pub retention_handle: String,
    pub binding_generation: u64,
    /// Absolute directory of push journals.
    pub journal_dir: PathBuf,
}

/// Runs a hook when the process was started as one. Call before argument parsing.
pub fn run_hook_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != HOOK_MARKER {
        return None;
    }
    let kind = args.next().unwrap_or_default();
    let state = args.next().unwrap_or_default();
    let result = match kind.to_str() {
        Some("pre-receive") => pre_receive(),
        Some("reference-transaction") => reference_transaction(state.to_str().unwrap_or("")),
        _ => Err(Error::new("invalid_request", "unknown hook")),
    };
    Some(match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("aq-git: push rejected: {}", error.message);
            1
        }
    })
}

fn read_stdin() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)?;
    Ok(input)
}
fn load_config() -> Result<HookConfig> {
    std::env::var(GRANT_ENV)
        .map_err(|_| Error::new("invalid_request", "push has no grant"))
        .and_then(|raw| Ok(serde_json::from_str(&raw)?))
}
fn single_update(input: &str) -> Result<[&str; 3]> {
    let updates: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
    if updates.len() != 1 {
        return Err(Error::new(
            "push_rejected",
            format!("a push must update exactly one ref, got {}", updates.len()),
        ));
    }
    let fields: Vec<&str> = updates[0].split(' ').collect();
    fields[..]
        .try_into()
        .map_err(|_| Error::new("push_rejected", "malformed ref update"))
}

/// Journals the one ref update before Git performs it (state `prepared`).
fn reference_transaction(state: &str) -> Result<()> {
    if state != "prepared" {
        return Ok(());
    }
    let config = load_config()?;
    let input = read_stdin()?;
    let [old, new, reference] = single_update(&input)?;
    let grant = &config.grant;
    let expected = grant.lease.as_deref().unwrap_or(ZERO_OID);
    if reference != grant.task_ref || old != expected || new == ZERO_OID {
        return Err(Error::new(
            "push_rejected",
            "ref transaction is not the granted update",
        ));
    }
    let journal = PushJournal {
        repository: grant.repository.clone(),
        operation_id: grant.operation.clone(),
        reference: reference.into(),
        old: old.into(),
        new: new.into(),
        base: grant.base.clone(),
        retention_handle: config.retention_handle.clone(),
        binding_generation: config.binding_generation,
    };
    let path = config.journal_dir.join(journal_file(&grant.operation));
    super::journal::write_durable(&path, &serde_json::to_vec(&journal)?)
        .map_err(|e| Error::new("push_rejected", format!("cannot journal the push: {e}")))
}

fn pre_receive() -> Result<()> {
    let config = load_config()?;
    let input = read_stdin()?;
    let quarantine = std::env::var_os("GIT_QUARANTINE_PATH").map(PathBuf::from);
    let mut env = HookGit {
        git: config.git.clone(),
        quarantine: quarantine.clone(),
    };
    check_push(&config, &input, quarantine.as_deref(), &mut env)?;
    // Git promotes the quarantine next. Say which files, so that a push that still fails
    // after promotion (a ref lock, a full disk) can have them taken back out.
    let [_, new, _] = single_update(&input)?;
    let mut files = Vec::new();
    if let Some(quarantine) = &quarantine
        && let Ok(entries) = std::fs::read_dir(quarantine.join("pack"))
    {
        files.extend(
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned()),
        );
    }
    let record = super::PushPromotion {
        repository: config.grant.repository.clone(),
        new: new.into(),
        files,
    };
    let path = config
        .journal_dir
        .join(super::promotion_file(&config.grant.operation));
    super::journal::write_durable(&path, &serde_json::to_vec(&record)?)
        .map_err(|e| Error::new("push_rejected", format!("cannot record the push: {e}")))
}

/// Git access of the push checks. Separate from the checks so they can be driven by a fake.
pub(crate) trait PushGit {
    /// One Git command against the quarantined repository (quarantine and pool objects).
    fn git(&mut self, args: Vec<String>) -> Result<Vec<u8>>;
    /// One Git command that sees only the objects the push itself carried.
    fn quarantined(&mut self, args: Vec<String>) -> Result<Vec<u8>>;
    /// The members of `wanted` that are not reachable from `tips`.
    fn closure_missing(
        &mut self,
        tips: &[String],
        wanted: BTreeSet<String>,
    ) -> Result<BTreeSet<String>>;
}

struct HookGit {
    git: PathBuf,
    quarantine: Option<PathBuf>,
}
impl HookGit {
    fn command(&self, args: &[String]) -> Command {
        // The environment (quarantine object directory included) is inherited from
        // `git receive-pack`, which already scrubbed it.
        let mut command = Command::new(&self.git);
        command
            .args(["-c", "core.hooksPath=/dev/null", "-c", "gc.auto=0"])
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command
    }
    fn output(mut command: Command) -> Result<Vec<u8>> {
        let output = command.output()?;
        if !output.status.success() {
            return Err(Error::new(
                "git_failed",
                "Git operation failed during push check",
            ));
        }
        Ok(output.stdout)
    }
    /// Reads OIDs one per line from `args`, striking them from `wanted`; stops once none is left.
    fn strike(&self, args: Vec<String>, wanted: &mut BTreeSet<String>) -> Result<()> {
        let mut command = self.command(&args);
        command.stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().unwrap();
        for line in std::io::BufReader::new(stdout).lines() {
            wanted.remove(line?.trim());
            if wanted.is_empty() {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }
        if !child.wait()?.success() {
            return Err(Error::new(
                "git_failed",
                "Git operation failed during push check",
            ));
        }
        Ok(())
    }
}
impl PushGit for HookGit {
    fn git(&mut self, args: Vec<String>) -> Result<Vec<u8>> {
        Self::output(self.command(&args))
    }
    fn quarantined(&mut self, args: Vec<String>) -> Result<Vec<u8>> {
        let Some(quarantine) = &self.quarantine else {
            return Ok(Vec::new());
        };
        let mut command = self.command(&args);
        command
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env("GIT_OBJECT_DIRECTORY", quarantine);
        Self::output(command)
    }
    fn closure_missing(
        &mut self,
        tips: &[String],
        mut wanted: BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        // Cheap first: the trees of the tips hold what a client can use as a thin base.
        for tip in tips {
            wanted.remove(tip.as_str());
        }
        for tip in tips {
            if wanted.is_empty() {
                return Ok(wanted);
            }
            self.strike(
                vec![
                    "ls-tree".into(),
                    "-r".into(),
                    "-t".into(),
                    "--object-only".into(),
                    tip.clone(),
                ],
                &mut wanted,
            )?;
        }
        if wanted.is_empty() {
            return Ok(wanted);
        }
        let mut walk = vec![
            "rev-list".into(),
            "--objects".into(),
            "--no-object-names".into(),
        ];
        walk.extend(tips.iter().cloned());
        self.strike(walk, &mut wanted)?;
        Ok(wanted)
    }
}

/// Offsets-ordered object IDs of the one pack in `quarantine`, with the count of
/// objects the client declared. The extra tail is what `index-pack --fix-thin`
/// appended from the repository: thin-pack delta bases the client did not send.
fn appended_bases(quarantine: &Path, declared: Option<u32>) -> Result<BTreeSet<String>> {
    let mut indexes = Vec::new();
    if let Ok(entries) = std::fs::read_dir(quarantine.join("pack")) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "idx") {
                indexes.push(entry.path());
            }
        }
    }
    let rejected = |message: &str| Error::new("push_rejected", message.to_owned());
    let Some(index) = indexes.pop() else {
        return Ok(BTreeSet::new());
    };
    if !indexes.is_empty() {
        return Err(rejected("push produced more than one pack"));
    }
    let bytes = std::fs::read(index)?;
    let malformed = || rejected("pack index is malformed");
    if bytes.len() < 8 + 256 * 4 || bytes[..8] != [0xff, b't', b'O', b'c', 0, 0, 0, 2] {
        return Err(malformed());
    }
    let word = |at: usize| -> Result<u32> {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
            .ok_or_else(malformed)
    };
    let count = word(8 + 255 * 4)? as usize;
    let names = 8 + 256 * 4;
    let small = names + count * 24;
    let large = small + count * 4;
    let mut entries: Vec<(u64, usize)> = Vec::with_capacity(count);
    for i in 0..count {
        let raw = word(small + i * 4)?;
        let offset = if raw & 0x8000_0000 == 0 {
            raw as u64
        } else {
            let at = large + (raw & 0x7fff_ffff) as usize * 8;
            bytes
                .get(at..at + 8)
                .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
                .ok_or_else(malformed)?
        };
        entries.push((offset, i));
    }
    entries.sort_unstable();
    let declared = declared.map_or(0, |n| n as usize);
    let mut appended = BTreeSet::new();
    for (_, i) in entries.into_iter().skip(declared) {
        let name = bytes
            .get(names + i * 20..names + i * 20 + 20)
            .ok_or_else(malformed)?;
        appended.insert(hex::encode(name));
    }
    Ok(appended)
}

/// The push checks. `input` is the hook's stdin (`<old> <new> <ref>` per update).
pub(crate) fn check_push(
    config: &HookConfig,
    input: &str,
    quarantine: Option<&Path>,
    env: &mut dyn PushGit,
) -> Result<()> {
    let grant = &config.grant;
    let reject = |message: String| Err(Error::new("push_rejected", message));
    let [old, new, reference] = single_update(input)?;
    if reference != grant.task_ref {
        return reject(format!(
            "ref {reference} is not the granted task ref {}",
            grant.task_ref
        ));
    }
    let expected = grant.lease.as_deref().unwrap_or(ZERO_OID);
    if old != expected {
        return reject(format!(
            "wrong lease: {reference} is at {old}, the grant expects {expected}"
        ));
    }
    if new == ZERO_OID {
        return reject("deleting the task ref is not allowed".into());
    }
    // Git compares the client's `old` with the real value only after it promoted the
    // objects. Compare here, in quarantine, so a stale claim leaves nothing behind.
    let listed = env.git(vec![
        "for-each-ref".into(),
        "--format=%(refname) %(objectname)".into(),
        reference.into(),
    ])?;
    let listed = String::from_utf8_lossy(&listed);
    let actual = listed
        .lines()
        .find_map(|l| {
            l.strip_prefix(reference)?
                .strip_prefix(' ')
                .map(str::to_owned)
        })
        .unwrap_or_else(|| ZERO_OID.to_owned());
    // A ref below, or a ref that is a parent directory of, the task ref makes Git's own
    // update fail after promotion. Refuse it here, while a rejection costs nothing.
    let descendants = listed.lines().filter(|l| {
        l.strip_prefix(reference)
            .is_some_and(|rest| rest.starts_with('/'))
    });
    if let Some(conflict) = descendants
        .map(|l| l.split(' ').next().unwrap_or(""))
        .next()
    {
        return reject(format!(
            "{reference} conflicts with the existing ref {conflict}"
        ));
    }
    let mut ancestors = Vec::new();
    let mut prefix = String::new();
    for part in reference.split('/').take(reference.split('/').count() - 1) {
        prefix = if prefix.is_empty() {
            part.into()
        } else {
            format!("{prefix}/{part}")
        };
        if prefix.matches('/').count() >= 2 {
            ancestors.push(prefix.clone());
        }
    }
    if !ancestors.is_empty() {
        let mut args = vec!["for-each-ref".into(), "--format=%(refname)".into()];
        args.extend(ancestors.iter().cloned());
        let found = env.git(args)?;
        if let Some(conflict) = String::from_utf8_lossy(&found)
            .lines()
            .find(|l| ancestors.iter().any(|a| a == l))
        {
            return reject(format!(
                "{reference} conflicts with the existing ref {conflict}"
            ));
        }
    }
    if actual != old {
        return reject(format!(
            "wrong lease: {reference} is at {actual}, the push claims {old}"
        ));
    }
    let object_type = |oid: &str, env: &mut dyn PushGit| {
        env.git(vec!["cat-file".into(), "-t".into(), oid.into()])
            .ok()
    };
    if object_type(new, env).as_deref() != Some(b"commit\n") {
        return reject(format!("{new} is not a commit"));
    }
    if object_type(&grant.base, env).as_deref() != Some(b"commit\n") {
        return reject(format!("base commit {} is unavailable", grant.base));
    }
    // Every new commit must descend from the base: each commit of `new ^base` needs
    // a parent that is the base or another commit of that set, so every chain ends
    // at the base.
    let listed = env
        .git(vec![
            "rev-list".into(),
            "--parents".into(),
            new.into(),
            "--not".into(),
            grant.base.clone(),
        ])
        .map_err(|_| {
            Error::new(
                "push_rejected",
                "history of the pushed commit is incomplete",
            )
        })?;
    let listed = String::from_utf8_lossy(&listed);
    let rows: Vec<Vec<&str>> = listed.lines().map(|l| l.split(' ').collect()).collect();
    let members: BTreeSet<&str> = rows.iter().map(|r| r[0]).collect();
    for row in &rows {
        if !row[1..]
            .iter()
            .any(|p| *p == grant.base || members.contains(p))
        {
            return reject(format!(
                "commit {} does not descend from base {}",
                row[0], grant.base
            ));
        }
    }
    if rows.is_empty() && new != grant.base {
        return reject(format!(
            "commit {new} does not descend from base {}",
            grant.base
        ));
    }
    // Same feature profile and quota rules as artifact.import.
    crate::operations::validate_tree_with(&mut |args| env.git(args), new)
        .map_err(|e| Error::new("push_rejected", format!("{}: {}", e.code, e.message)))?;
    if let Some(quarantine) = quarantine
        && crate::git::directory_bytes(quarantine, config.max_expanded_bytes)?
            > config.max_expanded_bytes
    {
        return reject("push exceeded the storage quota".into());
    }
    isolate_objects(config, new, quarantine, env)
}

/// Objects the push depends on must come from its own pack or from the closure of the
/// granted base, the lease and the served main head; objects the push carries must
/// belong to its history. Anything else would pull in (or hand out) another task's data.
fn isolate_objects(
    config: &HookConfig,
    new: &str,
    quarantine: Option<&Path>,
    env: &mut dyn PushGit,
) -> Result<()> {
    let grant = &config.grant;
    let mut tips = vec![grant.base.clone()];
    tips.extend(grant.lease.clone());
    tips.extend(config.main_head.clone());
    let mut seen = BTreeSet::new();
    tips.retain(|tip| seen.insert(tip.clone()));
    let list = |output: Vec<u8>| -> BTreeSet<String> {
        String::from_utf8_lossy(&output)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_owned)
            .collect()
    };
    let mut walk = vec![
        "rev-list".into(),
        "--objects".into(),
        "--no-object-names".into(),
        new.to_owned(),
        "--not".into(),
    ];
    walk.extend(tips.iter().cloned());
    let history = list(env.git(walk).map_err(|_| {
        Error::new(
            "push_rejected",
            "history of the pushed commit is incomplete",
        )
    })?);
    let carried = list(env.quarantined(vec![
        "cat-file".into(),
        "--batch-all-objects".into(),
        "--batch-check=%(objectname)".into(),
    ])?);
    let appended = match quarantine {
        Some(quarantine) => appended_bases(quarantine, config.pack_objects)?,
        None => BTreeSet::new(),
    };
    let mut unproven: BTreeSet<String> = history.difference(&carried).cloned().collect();
    unproven.extend(carried.difference(&history).cloned());
    unproven.extend(appended);
    if unproven.is_empty() {
        return Ok(());
    }
    let outside = env.closure_missing(&tips, unproven)?;
    if let Some(first) = outside.iter().next() {
        return Err(Error::new(
            "push_rejected",
            format!(
                "push depends on or carries {} object(s) outside the task history and its base, for example {first}",
                outside.len()
            ),
        ));
    }
    Ok(())
}
