//! Central-mode branch deletion after a pull request merges or closes, and the
//! local-only deletion of an attempt's try ref (`refs/heads/aq-try/<attempt-id>`).
use crate::{Error, Result, operations, repository::Binding, service::RepositoryService};
use aq_git_protocol::{ObjectId, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
pub const OPERATIONS: &[&str] = &["ref.delete"];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    #[serde(rename = "ref")]
    name: String,
    /// `None` deletes whichever head is observed; otherwise an exact lease.
    expected_old: Option<ObjectId>,
    remote: bool,
}
impl Delete {
    fn push_arguments(&self, b: &Binding, lease: &str) -> Vec<String> {
        let name = &self.name;
        vec![
            "push".into(),
            "--porcelain".into(),
            "--no-verify".into(),
            format!("--force-with-lease={name}:{lease}"),
            b.remote_url.clone().unwrap(),
            format!(":{name}"),
        ]
    }
    fn validate_scope(&self, b: &Binding) -> Result<()> {
        let name = self.name.as_str();
        // A try ref lives only in the central repository and is outside every
        // publication scope: it is deleted locally, never on the remote.
        if crate::git_http::is_try_ref(name) {
            return if self.remote {
                Err(Error::new(
                    "unsupported_feature",
                    "a try ref exists only in the central repository",
                ))
            } else {
                Ok(())
            };
        }
        // Only prefix-scoped branches are deletable; exact targets and refs never are.
        if b.target_scope(name) != Some(crate::repository::TargetScope::Prefix)
            || b.allowed_refs.contains(&self.name)
        {
            return Err(Error::new(
                "unsupported_feature",
                "ref is not under an allowed target prefix or is a protected target",
            ));
        }
        if self.remote && b.remote_url.is_none() {
            return Err(Error::new(
                "unsupported_feature",
                "remote deletion requires a configured authoritative remote",
            ));
        }
        Ok(())
    }
}

fn check_lease(side: &str, observed: Option<&str>, expected: Option<&str>) -> Result<()> {
    if let (Some(have), Some(want)) = (observed, expected)
        && have != want
    {
        return Err(
            Error::new("target_moved", "branch head differs from expected_old")
                .details(json!({"side": side, "observed": operations::wire_oid(have)})),
        );
    }
    Ok(())
}

fn delete_push_error(error: Error) -> Error {
    if error.code == "remote_auth_failed" {
        return error.details(json!({"remote_effect_unknown": true, "dispatched": true}));
    }
    // A replay observes the remote first, so retrying the same operation is safe.
    Error::new(
        "operation_indeterminate",
        "remote deletion outcome is unknown; retry the same operation",
    )
}

fn check_delete_reply(name: &str, output: &crate::git::GitOutput) -> Result<&'static str> {
    // Git prints a rejected deletion as `(delete):<ref>`.
    let expected_refspec = [format!(":{name}"), format!("(delete):{name}")];
    let flags: Vec<u8> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|byte| *byte == b'\t');
            let flag = fields.next()?;
            let refspec = fields.next()?;
            (flag.len() == 1 && expected_refspec.iter().any(|e| e.as_bytes() == refspec))
                .then_some(flag[0])
        })
        .collect();
    match flags.as_slice() {
        [b'-'] if output.status.success() => Ok("deleted"),
        [b'!'] => Err(Error::new(
            "target_moved",
            "remote rejected the deletion; the branch head moved",
        )
        .details(json!({"side":"remote"}))),
        _ => Err(Error::new(
            "operation_indeterminate",
            "push did not provide an unambiguous deletion acknowledgement",
        )),
    }
}

fn parse_advertised_ref(advertised: crate::git::GitOutput, name: &str) -> Result<Option<String>> {
    let unavailable = || Error::new("remote_unavailable", "remote ref could not be observed");
    if !advertised.status.success() {
        return Err(unavailable());
    }
    if advertised.stdout.is_empty() {
        return Ok(None);
    }
    // Never interpret a malformed advertisement as an absent ref.
    let text = std::str::from_utf8(&advertised.stdout).map_err(|_| unavailable())?;
    let mut lines = text.lines();
    let (oid, found) = lines
        .next()
        .and_then(|line| line.split_once('\t'))
        .ok_or_else(unavailable)?;
    if found != name || lines.next().is_some() {
        return Err(unavailable());
    }
    operations::oid(&ObjectId {
        algorithm: "sha1".into(),
        hex: oid.into(),
    })?;
    Ok(Some(oid.into()))
}

impl RepositoryService {
    /// Value of `name` in the central pool, or `None` when the ref (or an evicted pool) is absent.
    pub(crate) fn pool_ref(&self, repo: &str, name: &str) -> Result<Option<String>> {
        if !self.repositories.pool_path(repo).exists() {
            return Ok(None);
        }
        let out = self.repositories.runner.run(
            &self.repositories.pool_path(repo),
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                name.into(),
            ],
        )?;
        if out.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ));
        }
        if out.status.code() == Some(1) {
            return Ok(None);
        }
        Err(Error::new("git_failed", "Git command failed")
            .details(json!({"exit_code":out.status.code()})))
    }
    /// Value of `name` on the authoritative remote, or `None` when it is not advertised.
    fn remote_ref(&self, b: &Binding, name: &str) -> Result<Option<String>> {
        let advertised = self.repositories.remote_run(
            b,
            &[
                "ls-remote".into(),
                "--refs".into(),
                "--".into(),
                b.remote_url.clone().unwrap(),
                name.into(),
            ],
            Duration::from_secs(300),
        )?;
        parse_advertised_ref(advertised, name)
    }
    pub(crate) fn ref_delete_handle(&self, r: &Request) -> Result<Value> {
        let b = self.repositories.checked_binding(r)?;
        if b.mode != "central" {
            return Err(Error::new(
                "unsupported_feature",
                "worker repositories cannot delete branches",
            ));
        }
        let params: Delete = serde_json::from_value(r.params.clone())?;
        aq_git_protocol::validate_ref(&params.name).map_err(|e| Error::new(e.code, e.message))?;
        let expected = params
            .expected_old
            .as_ref()
            .map(operations::oid)
            .transpose()?;
        let name = params.name.as_str();
        params.validate_scope(&b)?;
        // Check both leases before any effect so a mismatch leaves nothing half deleted.
        let repo = &b.repository_id;
        let local = self.pool_ref(repo, name)?;
        check_lease("local", local.as_deref(), expected)?;
        let remote = if params.remote {
            self.remote_ref(&b, name)?
        } else {
            None
        };
        check_lease("remote", remote.as_deref(), expected)?;
        let mut remote_outcome = if params.remote {
            "already_absent"
        } else {
            "not_requested"
        };
        if let Some(lease) = &remote {
            let args = params.push_arguments(&b, lease);
            let timeout = Duration::from_millis(r.deadline_ms.unwrap_or(300_000).min(300_000));
            let output = self
                .repositories
                .remote_run(&b, &args, timeout)
                .map_err(delete_push_error)?;
            remote_outcome = check_delete_reply(name, &output)?;
        }
        // Retention handles only hold private refs/aq-git refs, never branch heads.
        let mut local_outcome = "already_absent";
        if let Some(have) = &local {
            self.repositories.git(
                repo,
                vec!["update-ref".into(), "-d".into(), name.into(), have.clone()],
            )?;
            local_outcome = "deleted";
        }
        let old = local.or(remote);
        Ok(json!({
            "outcome":if local_outcome == "deleted" || remote_outcome == "deleted" {"deleted"} else {"already_absent"},
            "ref":params.name,
            "old":old.as_deref().map(operations::wire_oid),
            "remote":{"requested":params.remote,"outcome":remote_outcome},
            "local":{"outcome":local_outcome},
            "acknowledged_at":chrono::Utc::now().to_rfc3339(),
        }))
    }
}
