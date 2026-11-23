//! The `pre-receive` hook of a task push.
//!
//! `aq-git serve` installs a tiny shell script that re-runs its own executable
//! with [`HOOK_MARKER`]. Git runs `pre-receive` inside its object quarantine, so a
//! rejected push leaves no object or ref behind. The hook only checks which refs
//! the push updates; Git itself checks the objects (`receive.fsckObjects`) and the
//! size (`receive.maxInputSize`). The grant arrives in the `AQ_GIT_HTTP_GRANT`
//! environment variable of the `git receive-pack` child.
use super::{Grant, ZERO_OID};
use crate::{Error, Result};
use std::io::Read;

pub(crate) const HOOK_MARKER: &str = "__aq_git_hook_v1";
pub(crate) const GRANT_ENV: &str = "AQ_GIT_HTTP_GRANT";

/// Runs the hook when the process was started as one. Call before argument parsing.
pub fn run_hook_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != HOOK_MARKER {
        return None;
    }
    let kind = args.next().unwrap_or_default();
    let result = match kind.to_str() {
        Some("pre-receive") => pre_receive(),
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

fn pre_receive() -> Result<()> {
    let grant: Grant = std::env::var(GRANT_ENV)
        .map_err(|_| Error::new("invalid_request", "push has no grant"))
        .and_then(|raw| Ok(serde_json::from_str(&raw)?))?;
    let mut input = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)?;
    check_push(&grant, &input)
}

/// The push check. `input` is the hook's stdin (`<old> <new> <ref>` per update):
/// every update must be of the grant's task branch or of the grant's try ref,
/// and none may delete either. Non-fast-forward updates are allowed: the client's
/// lease protects the task branch, and consecutive tries are siblings.
pub(crate) fn check_push(grant: &Grant, input: &str) -> Result<()> {
    let reject = |message: String| Err(Error::new("push_rejected", message));
    for update in input.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = update.split(' ').collect();
        let [_old, new, reference] = fields[..] else {
            return reject("malformed ref update".into());
        };
        let ref_kind = if reference == grant.task_ref {
            "task"
        } else if grant.try_ref.as_deref() == Some(reference) {
            "try"
        } else {
            return reject(format!(
                "ref {reference} is not the granted task ref {} or try ref",
                grant.task_ref
            ));
        };
        if new == ZERO_OID {
            return reject(format!("deleting the {ref_kind} ref is not allowed"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grant() -> Grant {
        Grant {
            repository: "7d1f2ad4-0f0b-4c5e-9a46-2b1c1b7a9d11".into(),
            base: "a".repeat(40),
            task_ref: "refs/heads/aq/task-1".into(),
            try_ref: None,
            max_push_bytes: 1000,
            operation: "0b0f1c2e-3a4b-4c5d-8e9f-a0b1c2d3e4f5".into(),
        }
    }
    #[test]
    fn only_updates_of_the_task_ref_pass() {
        let (old, new) = ("b".repeat(40), "c".repeat(40));
        let line = |reference: &str, new: &str| format!("{old} {new} {reference}\n");
        assert!(check_push(&grant(), &line("refs/heads/aq/task-1", &new)).is_ok());
        assert!(check_push(&grant(), &line("refs/heads/aq/task-1", ZERO_OID)).is_err());
        for other in ["refs/heads/main", "refs/heads/aq/task-2", "refs/tags/x"] {
            assert!(check_push(&grant(), &line(other, &new)).is_err(), "{other}");
        }
        let both = line("refs/heads/aq/task-1", &new) + &line("refs/heads/aq/task-2", &new);
        assert!(check_push(&grant(), &both).is_err());
        assert!(check_push(&grant(), "garbage").is_err());
    }
    #[test]
    fn only_the_granted_try_ref_may_be_created_or_force_updated() {
        let try_ref = "refs/heads/aq-try/5e3c2b1a-9d8e-4f7a-b6c5-d4e3f2a1b0c9";
        let other = "refs/heads/aq-try/0b0f1c2e-3a4b-4c5d-8e9f-a0b1c2d3e4f5";
        let granted = Grant {
            try_ref: Some(try_ref.into()),
            ..grant()
        };
        let new = "c".repeat(40);
        let line = |old: &str, new: &str, reference: &str| format!("{old} {new} {reference}\n");
        for old in [ZERO_OID.to_owned(), "b".repeat(40), "d".repeat(40)] {
            assert!(
                check_push(&granted, &line(&old, &new, try_ref)).is_ok(),
                "{old}"
            );
        }
        let error = check_push(&granted, &line(&new, ZERO_OID, try_ref)).unwrap_err();
        assert_eq!(error.code, "push_rejected");
        assert_eq!(error.message, "deleting the try ref is not allowed");
        for bad in [
            other.to_owned(),
            format!("{try_ref}/x"),
            "refs/heads/main".into(),
        ] {
            let error = check_push(&granted, &line(ZERO_OID, &new, &bad)).unwrap_err();
            assert_eq!(error.code, "push_rejected", "{bad}");
        }
        let task = "refs/heads/aq/task-1";
        assert!(check_push(&granted, &line(&new, &"e".repeat(40), task)).is_ok());
        assert!(check_push(&granted, &line(&new, ZERO_OID, task)).is_err());
        assert!(check_push(&grant(), &line(ZERO_OID, &new, try_ref)).is_err());
    }
}
