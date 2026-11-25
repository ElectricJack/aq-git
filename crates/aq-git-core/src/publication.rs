//! Expected-old, fast-forward publication and explicit uncertainty reconciliation.
mod attempt;
pub mod rejection;
use attempt::PublicationAttempt;
use rejection::RejectionClass;

use crate::{
    Error, Result, operations,
    repository::{Binding, TargetScope},
    service::RepositoryService,
    state::OperationStatus,
};
use aq_git_protocol::{ObjectId, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
pub const OPERATIONS: &[&str] = &["publish", "operation.reconcile", "operation.resolve"];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Publish {
    #[serde(default)]
    candidate_id: String,
    /// A task branch of the central repository published instead of a candidate.
    #[serde(default)]
    source_ref: Option<String>,
    expected_source: Option<ObjectId>,
    target: String,
    expected_old: ObjectId,
    #[serde(default)]
    update: UpdateMode,
}
/// `fast_forward` requires the candidate to contain the expected old commit.
/// `replace` (prefix-scoped targets only) swaps the branch to the candidate
/// under the same exact expected-old lease, e.g. a rebuilt pull-request head.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    #[default]
    FastForward,
    Replace,
}
impl UpdateMode {
    fn is_fast_forward(&self) -> bool {
        *self == Self::FastForward
    }
}
impl Publish {
    fn check_expected_source(&self, commit: &str) -> Result<()> {
        if let Some(source) = &self.expected_source {
            let expected_source = operations::oid(source)?;
            if absent(expected_source) {
                return Err(Error::new(
                    "invalid_request",
                    "expected_source must name a commit",
                ));
            }
            if commit != expected_source {
                return Err(Error::new(
                    "source_moved",
                    "source_ref no longer holds the submitted commit",
                ));
            }
        }
        Ok(())
    }
    /// Whether the source is a branch of the central repository rather than a candidate.
    fn has_source_ref(&self) -> bool {
        self.source_ref.is_some()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    target_operation_id: String,
    expected_generation: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    target_operation_id: String,
    expected_generation: u64,
    disposition: String,
    reason: String,
    old_publishers_stopped: bool,
    evidence: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicationRecord {
    pub version: u32,
    pub operation_id: String,
    pub repository_id: String,
    pub binding_generation: u64,
    pub candidate_id: String,
    pub candidate: String,
    pub target: String,
    pub expected_old: String,
    #[serde(default, skip_serializing_if = "UpdateMode::is_fast_forward")]
    pub update: UpdateMode,
    pub state: String,
    pub result: Option<Value>,
    pub resolved_by: Option<String>,
    /// Set when the publication's source is this branch of the central repository;
    /// `candidate` then holds the branch's commit and `candidate_id` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}
impl PublicationRecord {
    /// The candidate whose retention this publication depends on, if any.
    fn retained_candidate(&self) -> Option<&str> {
        if self.source_ref.is_some() {
            return None;
        }
        Some(&self.candidate_id)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TargetHold {
    #[serde(default = "hold_version")]
    version: u32,
    #[serde(default)]
    generation: u64,
    active: bool,
    operation_id: String,
    target: String,
}
fn hold_version() -> u32 {
    1
}
impl RepositoryService {
    /// Whether an observed target already satisfies the publication: containment
    /// for fast-forward, exact identity for replacement.
    fn publication_present(&self, p: &PublicationRecord, observed: &str) -> Result<bool> {
        if absent(observed) {
            return Ok(false);
        }
        match p.update {
            UpdateMode::Replace => Ok(observed == p.candidate),
            UpdateMode::FastForward => self.ancestor_at(
                &self.repositories.pool_path(&p.repository_id),
                &p.candidate,
                observed,
            ),
        }
    }
    fn publication_record(&self, repo: &str, operation: &str) -> Result<PublicationRecord> {
        operations::id(operation)?;
        self.root
            .read_json(format!("repositories/{repo}/publications/{operation}.json"))?
            .ok_or_else(|| Error::new("operation_not_found", "publication record not found"))
    }
    fn save_publication(&self, p: &PublicationRecord) -> Result<()> {
        self.root.write_json(
            format!(
                "repositories/{}/publications/{}.json",
                p.repository_id, p.operation_id
            ),
            p,
        )
    }
    fn target_path(repo: &str, target: &str) -> String {
        format!(
            "repositories/{repo}/publication-targets/{}.json",
            hex::encode(Sha256::digest(target.as_bytes()))
        )
    }
    fn publication_binding(&self, r: &Request) -> Result<Binding> {
        let b = self.repositories.checked_binding(r)?;
        if b.mode != "central" {
            return Err(Error::new(
                "unsupported_feature",
                "worker repositories cannot publish or reconcile remote publication",
            ));
        }
        if b.remote_url.is_none() {
            return Err(Error::new(
                "unsupported_feature",
                "publication requires a configured authoritative remote",
            ));
        }
        Ok(b)
    }
    fn read_target_hold(&self, p: &PublicationRecord) -> Result<Option<TargetHold>> {
        let hold: Option<TargetHold> = self
            .root
            .read_json(Self::target_path(&p.repository_id, &p.target))?;
        if let Some(hold) = &hold {
            if hold.version != 1 {
                return Err(Error::new(
                    "state_incompatible",
                    "unsupported publication hold format",
                ));
            }
            if hold.target != p.target || operations::id(&hold.operation_id).is_err() {
                return Err(Error::new(
                    "state_corrupt",
                    "publication hold identity does not match its destination",
                ));
            }
        }
        Ok(hold)
    }
    fn replace_target_hold(
        &self,
        p: &PublicationRecord,
        expected: Option<&TargetHold>,
        next: &TargetHold,
    ) -> Result<()> {
        let _guard = crate::git::lock_with_deadline(&self.root.inner.mutex)?;
        if self.read_target_hold(p)?.as_ref() != expected {
            return Err(Error::new(
                "generation_conflict",
                "publication hold owner or generation changed",
            ));
        }
        self.root
            .write_json(Self::target_path(&p.repository_id, &p.target), next)
    }
    fn acquire_target(&self, p: &PublicationRecord) -> Result<()> {
        let hold = self.read_target_hold(p)?;
        if let Some(hold) = &hold
            && hold.active
        {
            if hold.operation_id == p.operation_id {
                return Ok(());
            }
            self.check_previous_holder(p, hold)?;
        }
        let generation = hold
            .as_ref()
            .map_or(0, |h| h.generation)
            .checked_add(1)
            .ok_or_else(|| {
                Error::new(
                    "state_incompatible",
                    "publication hold generation exhausted",
                )
            })?;
        self.replace_target_hold(
            p,
            hold.as_ref(),
            &TargetHold {
                version: 1,
                generation,
                active: true,
                operation_id: p.operation_id.clone(),
                target: p.target.clone(),
            },
        )
    }
    fn check_previous_holder(&self, p: &PublicationRecord, hold: &TargetHold) -> Result<()> {
        let previous = self.publication_record(&p.repository_id, &hold.operation_id)?;
        let owner = (
            &previous.operation_id,
            &previous.repository_id,
            &previous.target,
        );
        let expected = (&hold.operation_id, &p.repository_id, &p.target);
        if previous.version != 1 || owner != expected {
            return Err(Error::new(
                "state_corrupt",
                "publication hold owner record does not match its destination",
            ));
        }
        if !matches!(
            previous.state.as_str(),
            "applied" | "already_present" | "rejected" | "resolved"
        ) || previous.result.is_none()
        {
            return Err(Error::new(
                "operation_indeterminate",
                "destination has an unresolved publication",
            )
            .details(json!({"blocking_operation_id":hold.operation_id,"target":p.target})));
        }
        Ok(())
    }
    fn release_target_hold(&self, p: &PublicationRecord) -> Result<()> {
        let Some(hold) = self.read_target_hold(p)? else {
            return Ok(());
        };
        // Historical receipt/reconciliation cleanup may run after another
        // publication acquired this destination. It has no authority over that
        // successor's hold, even if the old command journal is still Pending.
        if hold.operation_id != p.operation_id || !hold.active {
            return Ok(());
        }
        let generation = hold.generation.checked_add(1).ok_or_else(|| {
            Error::new(
                "state_incompatible",
                "publication hold generation exhausted",
            )
        })?;
        self.replace_target_hold(
            p,
            Some(&hold),
            &TargetHold {
                version: 1,
                generation,
                active: false,
                operation_id: p.operation_id.clone(),
                target: p.target.clone(),
            },
        )
    }
    fn finish_publication(
        &self,
        p: &mut PublicationRecord,
        state: &str,
        result: Value,
    ) -> Result<Value> {
        p.state = state.into();
        p.result = Some(result.clone());
        self.save_publication(p)?;
        if let Some(candidate) = p.retained_candidate() {
            self.root.release_dependency(candidate, &p.operation_id)?;
        }
        self.release_target_hold(p)?;
        Ok(result)
    }
    fn publication_observe(
        &self,
        b: &Binding,
        p: &PublicationRecord,
        operation: &str,
    ) -> Result<(String, String)> {
        let advertised = self.repositories.remote_run(
            b,
            &[
                "ls-remote".into(),
                "--refs".into(),
                "--".into(),
                b.remote_url.clone().unwrap(),
                p.target.clone(),
            ],
            Duration::from_secs(300),
        )?;
        if !advertised.status.success() {
            return Err(Error::new(
                "remote_unavailable",
                "authoritative target could not be observed",
            ));
        }
        if advertised.stdout.is_empty() {
            return Ok(("0".repeat(40), chrono::Utc::now().to_rfc3339()));
        }
        validate_target_advertisement(&advertised.stdout, &p.target)?;
        let reference = format!("refs/aq-git/publication-observations/{operation}");
        let args = vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            b.remote_url.clone().unwrap(),
            format!("+{}:{reference}", p.target),
        ];
        let output = self
            .repositories
            .remote_run(b, &args, Duration::from_secs(300))?;
        if !output.status.success() {
            return Err(Error::new(
                "remote_unavailable",
                "authoritative target could not be observed",
            ));
        }
        let oid = String::from_utf8_lossy(&self.repositories.git(
            &p.repository_id,
            vec!["rev-parse".into(), "--verify".into(), reference.clone()],
        )?)
        .trim()
        .to_string();
        operations::oid(&ObjectId {
            algorithm: "sha1".into(),
            hex: oid.clone(),
        })?;
        let timestamp = chrono::Utc::now().to_rfc3339();
        // Observation objects are disposable; input/output pins retain all durable work.
        self.repositories.git(
            &p.repository_id,
            vec!["update-ref".into(), "-d".into(), reference, oid.clone()],
        )?;
        Ok((oid, timestamp))
    }
    pub(crate) fn publication_handle(&self, r: &Request) -> Result<Value> {
        let b = self.publication_binding(r)?;
        if r.operation == "operation.reconcile" || r.operation == "operation.resolve" {
            return self.reconcile_or_resolve(r, &b);
        }
        let params: Publish = serde_json::from_value(r.params.clone())?;
        params.validate(&b)?;
        let attempt = PublicationAttempt {
            service: self,
            binding: &b,
            op: operations::opid(r)?,
        };
        attempt.publish(params, r.deadline_ms)
    }
    fn check_uncertain_publication(
        &self,
        publication: &mut PublicationRecord,
        target: &crate::state::OperationRecord,
        expected_generation: u64,
    ) -> Result<()> {
        if target.generation != expected_generation {
            return Err(
                Error::new("generation_conflict", "target operation generation changed")
                    .details(json!({"generation":target.generation})),
            );
        }
        if target.status != OperationStatus::Pending {
            return Err(Error::new(
                "operation_terminal",
                "target operation is already terminal",
            ));
        }
        if target.phase == "dispatched" && publication.state == "preparing" {
            publication.state = "indeterminate".into();
            self.save_publication(publication)?;
        }
        if !matches!(publication.state.as_str(), "dispatched" | "indeterminate") {
            return Err(Error::new(
                "invalid_request",
                "target has no dispatched uncertain publication",
            ));
        }
        Ok(())
    }

    fn replay_resolution(
        &self,
        publication: &mut PublicationRecord,
        target_id: &str,
        target: &crate::state::OperationRecord,
    ) -> Result<Value> {
        let result = publication
            .result
            .clone()
            .ok_or_else(|| Error::new("state_corrupt", "resolved publication lacks result"))?;
        let state = publication.state.clone();
        self.finish_publication(publication, &state, result.clone())?;
        if target.status == OperationStatus::Pending {
            self.root
                .complete_operation(target_id, target.generation, result.clone())?;
        }
        Ok(json!({"target_operation_id":target_id,"resolution":result}))
    }

    fn reconcile_or_resolve(&self, r: &Request, b: &Binding) -> Result<Value> {
        let (target_id, expected_generation, resolve) = reconciliation_target(r)?;
        operations::id(&target_id)?;
        let op = operations::opid(r)?;
        if op == target_id {
            return Err(Error::new(
                "invalid_request",
                "reconciliation/resolution needs its own operation ID",
            ));
        }
        let target = self
            .root
            .operation(&target_id)?
            .ok_or_else(|| Error::new("operation_not_found", "target operation does not exist"))?;
        let mut publication = self.publication_record(&b.repository_id, &target_id)?;
        if publication.resolved_by.as_deref() == Some(op) {
            return self.replay_resolution(&mut publication, &target_id, &target);
        }
        self.check_uncertain_publication(&mut publication, &target, expected_generation)?;
        let result = if let Some(p) = resolve {
            p.validate()?;
            publication.resolved_by = Some(op.into());
            self.finish_publication(&mut publication,"resolved",json!({"outcome":"resolved","disposition":p.disposition,"reason":p.reason,"evidence":p.evidence,"resolved_at":chrono::Utc::now().to_rfc3339(),"historical_remote_effect":"unknown"}))?
        } else {
            match self.publication_observe(b, &publication, op) {
                Ok((observed, at)) => {
                    if self.publication_present(&publication, &observed)? {
                        publication.resolved_by = Some(op.into());
                        self.finish_publication(&mut publication,"already_present",json!({"outcome":"already_present","observed_target":operations::wire_oid(&observed),"observed_at":at,"caused_by_this_operation":false}))?
                    } else {
                        return Ok(
                            json!({"target_operation_id":target_id,"outcome":"indeterminate","observed_target":operations::wire_oid(&observed),"observed_at":at,"historical_remote_effect":"unknown"}),
                        );
                    }
                }
                Err(error) => {
                    if error.code == "remote_auth_failed" {
                        return Err(error);
                    }
                    return Ok(
                        json!({"target_operation_id":target_id,"outcome":"indeterminate","observation_error":"remote_unavailable","historical_remote_effect":"unknown"}),
                    );
                }
            }
        };
        self.root
            .complete_operation(&target_id, target.generation, result.clone())?;
        Ok(json!({"target_operation_id":target_id,"resolution":result}))
    }
}

impl RepositoryService {
    /// A new publication of a task branch (`refs/heads/aq/*`) of the central
    /// repository at its current commit. The branch itself keeps the commit.
    fn ref_publication(
        &self,
        b: &Binding,
        op: &str,
        params: &Publish,
    ) -> Result<PublicationRecord> {
        let expected = params.expected_old.hex.as_str();
        let repo = &b.repository_id;
        let source = params.source_ref.clone().unwrap_or_default();
        if !params.candidate_id.is_empty() {
            return Err(Error::new(
                "invalid_request",
                "give either candidate_id or source_ref, not both",
            ));
        }
        aq_git_protocol::validate_ref(&source).map_err(|e| Error::new(e.code, e.message))?;
        if !source.starts_with(crate::git_http::TASK_PREFIX) {
            return Err(Error::new(
                "invalid_request",
                "source_ref must be a task branch under refs/heads/aq/",
            ));
        }
        let commit = self
            .pool_ref(repo, &source)?
            .ok_or_else(|| Error::new("object_unavailable", "source_ref does not exist"))?;
        params.check_expected_source(&commit)?;
        if params.update.is_fast_forward() && !absent(expected) {
            self.commit_exists(repo, expected)?;
            if !self.ancestor_at(&self.repositories.pool_path(repo), expected, &commit)? {
                return Err(Error::new(
                    "target_moved",
                    "source is not a fast-forward of the explicit expected old revision",
                ));
            }
        }
        Ok(PublicationRecord {
            version: 1,
            operation_id: op.into(),
            repository_id: repo.clone(),
            binding_generation: b.binding_generation,
            candidate_id: String::new(),
            candidate: commit,
            target: params.target.clone(),
            expected_old: expected.into(),
            update: params.update,
            state: "preparing".into(),
            result: None,
            resolved_by: None,
            source_ref: Some(source),
        })
    }
}

fn absent(oid: &str) -> bool {
    oid == "0000000000000000000000000000000000000000"
}

fn validate_target_advertisement(bytes: &[u8], expected: &str) -> Result<()> {
    // Never interpret a malformed advertisement as an absent target.
    let advertisement = std::str::from_utf8(bytes)
        .map_err(|_| Error::new("remote_unavailable", "Invalid target advertisement"))?;
    let mut lines = advertisement.lines();
    let (oid, target) = lines
        .next()
        .and_then(|line| line.split_once('\t'))
        .ok_or_else(|| Error::new("remote_unavailable", "Invalid target advertisement"))?;
    if target != expected || lines.next().is_some() {
        return Err(Error::new(
            "remote_unavailable",
            "Invalid target advertisement",
        ));
    }
    operations::oid(&ObjectId {
        algorithm: "sha1".into(),
        hex: oid.into(),
    })?;
    Ok(())
}

impl Resolve {
    fn validate(&self) -> Result<()> {
        let supported = ["abandon", "permit_new_operation"].contains(&self.disposition.as_str());
        let reported = match &self.evidence {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(fields) => !fields.is_empty(),
            _ => false,
        };
        let explained = !self.reason.trim().is_empty() && reported;
        let confirmed = supported && self.old_publishers_stopped;
        if !confirmed || !explained {
            return Err(Error::new(
                "invalid_request",
                "resolution requires disposition, reason, cleanup evidence and old_publishers_stopped=true",
            ));
        }
        Ok(())
    }
}

fn reconciliation_target(r: &Request) -> Result<(String, u64, Option<Resolve>)> {
    let target = if r.operation == "operation.resolve" {
        let p: Resolve = serde_json::from_value(r.params.clone())?;
        (
            p.target_operation_id.clone(),
            p.expected_generation,
            Some(p),
        )
    } else {
        let p: Reconcile = serde_json::from_value(r.params.clone())?;
        (p.target_operation_id, p.expected_generation, None)
    };
    Ok(target)
}
