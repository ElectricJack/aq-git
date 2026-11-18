//! The `pre-receive` / `post-receive` hooks of a task push.
//!
//! `aq-git serve` installs two tiny shell scripts that re-run its own executable
//! with [`HOOK_MARKER`]. Git runs `pre-receive` inside its object quarantine, so a
//! rejected push leaves no object or ref behind. The grant and limits arrive in the
//! `AQ_GIT_HTTP_GRANT` environment variable of the `git receive-pack` child.
use super::{Grant, ZERO_OID};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) const HOOK_MARKER: &str = "__aq_git_hook_v1";
pub(crate) const GRANT_ENV: &str = "AQ_GIT_HTTP_GRANT";
pub(crate) const ACCEPTED_ENV: &str = "AQ_GIT_HTTP_ACCEPTED";

/// Everything the hook needs; serialized into [`GRANT_ENV`] by the server.
#[derive(Serialize, Deserialize)]
pub(crate) struct HookConfig {
    pub grant: Grant,
    pub git: PathBuf,
    pub max_expanded_bytes: u64,
}

/// Runs a hook when the process was started as one. Call before argument parsing.
pub fn run_hook_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != HOOK_MARKER {
        return None;
    }
    let kind = args.next().unwrap_or_default();
    let result = match kind.to_str() {
        Some("pre-receive") => pre_receive(),
        Some("post-receive") => post_receive(),
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
fn post_receive() -> Result<()> {
    let input = read_stdin()?;
    if let Some(path) = std::env::var_os(ACCEPTED_ENV) {
        std::fs::write(path, input)?;
    }
    Ok(())
}
fn pre_receive() -> Result<()> {
    let config: HookConfig = std::env::var(GRANT_ENV)
        .map_err(|_| Error::new("invalid_request", "push has no grant"))
        .and_then(|raw| Ok(serde_json::from_str(&raw)?))?;
    let input = read_stdin()?;
    let quarantine = std::env::var_os("GIT_QUARANTINE_PATH").map(PathBuf::from);
    check_push(&config, &input, quarantine.as_deref(), &mut |args| {
        run_git(&config.git, args)
    })
}

fn run_git(git: &Path, args: Vec<String>) -> Result<Vec<u8>> {
    // The environment (quarantine object directory included) is inherited from
    // `git receive-pack`, which already scrubbed it.
    let output = Command::new(git)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "gc.auto=0"])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(Error::new(
            "git_failed",
            "Git operation failed during push check",
        ));
    }
    Ok(output.stdout)
}

/// The push checks. `input` is the hook's stdin (`<old> <new> <ref>` per update).
/// `git` runs one Git command against the quarantined repository.
pub(crate) fn check_push(
    config: &HookConfig,
    input: &str,
    quarantine: Option<&Path>,
    git: &mut dyn FnMut(Vec<String>) -> Result<Vec<u8>>,
) -> Result<()> {
    let grant = &config.grant;
    let reject = |message: String| Err(Error::new("push_rejected", message));
    let updates: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
    if updates.len() != 1 {
        return reject(format!(
            "a push must update exactly one ref, got {}",
            updates.len()
        ));
    }
    let fields: Vec<&str> = updates[0].split(' ').collect();
    let [old, new, reference] = fields[..] else {
        return reject("malformed ref update".into());
    };
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
    let object_type = |oid: &str, git: &mut dyn FnMut(Vec<String>) -> Result<Vec<u8>>| {
        git(vec!["cat-file".into(), "-t".into(), oid.into()]).ok()
    };
    if object_type(new, git).as_deref() != Some(b"commit\n") {
        return reject(format!("{new} is not a commit"));
    }
    if object_type(&grant.base, git).as_deref() != Some(b"commit\n") {
        return reject(format!("base commit {} is unavailable", grant.base));
    }
    // Every new commit must descend from the base: each commit of `new ^base` needs
    // a parent that is the base or another commit of that set, so every chain ends
    // at the base.
    let listed = git(vec![
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
    let members: std::collections::BTreeSet<&str> = rows.iter().map(|r| r[0]).collect();
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
    crate::operations::validate_tree_with(git, new)
        .map_err(|e| Error::new("push_rejected", format!("{}: {}", e.code, e.message)))?;
    if let Some(quarantine) = quarantine
        && crate::git::directory_bytes(quarantine, config.max_expanded_bytes)?
            > config.max_expanded_bytes
    {
        return reject("push exceeded the storage quota".into());
    }
    Ok(())
}
