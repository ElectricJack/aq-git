//! Expected-old, fast-forward publication and explicit uncertainty reconciliation.
use crate::{
    Error, Result, operations, repository::Binding, service::RepositoryService,
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
    candidate_id: String,
    target: String,
    expected_old: ObjectId,
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
    pub state: String,
    pub result: Option<Value>,
    pub resolved_by: Option<String>,
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
        if b.credential_profile.is_some() {
            return Err(Error::new(
                "unsupported_feature",
                "remote credential profiles are not enabled in this build",
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
            let previous = self.publication_record(&p.repository_id, &hold.operation_id)?;
            if previous.version != 1
                || previous.operation_id != hold.operation_id
                || previous.repository_id != p.repository_id
                || previous.target != p.target
            {
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
        self.root
            .release_dependency(&p.candidate_id, &p.operation_id)?;
        self.release_target_hold(p)?;
        Ok(result)
    }
    fn publication_observe(
        &self,
        b: &Binding,
        p: &PublicationRecord,
        operation: &str,
    ) -> Result<(String, String)> {
        let reference = format!("refs/aq-git/publication-observations/{operation}");
        let args = vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            b.remote_url.clone().unwrap(),
            format!("+{}:{reference}", p.target),
        ];
        let output = self.repositories.runner.run_with(
            &self.repositories.pool_path(&p.repository_id),
            &args,
            &[],
            &[],
            Duration::from_secs(300),
        )?;
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
        let repo = &b.repository_id;
        if r.operation == "operation.reconcile" || r.operation == "operation.resolve" {
            return self.reconcile_or_resolve(r, &b);
        }
        let params: Publish = serde_json::from_value(r.params.clone())?;
        operations::id(&params.candidate_id)?;
        let expected = operations::oid(&params.expected_old)?;
        aq_git_protocol::validate_ref(&params.target).map_err(|e| Error::new(e.code, e.message))?;
        if !params.target.starts_with("refs/heads/") || !b.allowed_targets.contains(&params.target)
        {
            return Err(Error::new(
                "unsupported_feature",
                "destination is not an allowed existing publication branch",
            ));
        }
        let op = operations::opid(r)?;
        let existing: Option<PublicationRecord> = self
            .root
            .read_json(format!("repositories/{repo}/publications/{op}.json"))?;
        let mut publication = if let Some(p) = existing {
            p
        } else {
            let candidate = self.candidate_record(repo, &params.candidate_id)?;
            if candidate.state != "ready" || candidate.binding_generation != b.binding_generation {
                return Err(Error::new(
                    "conflicted",
                    "publication requires a ready candidate in the current binding",
                ));
            }
            self.active_handle(repo, &params.candidate_id)?;
            self.commit_exists(repo, expected)?;
            if !self.ancestor_at(
                &self.repositories.pool_path(repo),
                expected,
                &candidate.prefix,
            )? {
                return Err(Error::new(
                    "target_moved",
                    "candidate is not a fast-forward of the explicit expected old revision",
                ));
            }
            let p = PublicationRecord {
                version: 1,
                operation_id: op.into(),
                repository_id: repo.into(),
                binding_generation: b.binding_generation,
                candidate_id: params.candidate_id,
                candidate: candidate.prefix,
                target: params.target,
                expected_old: expected.into(),
                state: "preparing".into(),
                result: None,
                resolved_by: None,
            };
            self.save_publication(&p)?;
            p
        };
        if publication.version != 1 || publication.binding_generation != b.binding_generation {
            return Err(Error::new(
                "state_incompatible",
                "publication record or binding is incompatible",
            ));
        }
        if let Some(result) = publication.result.clone() {
            return self.finish_publication(&mut publication.clone(), &publication.state, result);
        }
        self.acquire_target(&publication)?;
        self.root
            .acquire_dependency(&publication.candidate_id, op)?;
        let journal = self
            .root
            .operation(op)?
            .ok_or_else(|| Error::new("operation_not_found", "missing publication intent"))?;
        if journal.phase == "dispatched"
            || publication.state == "dispatched"
            || publication.state == "indeterminate"
        {
            publication.state = "indeterminate".into();
            self.save_publication(&publication)?;
            return Err(Error::new(
                "operation_indeterminate",
                "publication was dispatched; reconcile before any new attempt",
            )
            .details(json!({"operation_id":op,"target":publication.target})));
        }
        let (observed, observed_at) = match self.publication_observe(&b,&publication,op){
            Ok(observation)=>observation,
            Err(_)=>return self.finish_publication(&mut publication,"rejected",json!({"outcome":"rejected","reason":"preflight_observation_unavailable","dispatched":false}))
        };
        if self.ancestor_at(
            &self.repositories.pool_path(repo),
            &publication.candidate,
            &observed,
        )? {
            return self.finish_publication(&mut publication,"already_present",json!({"outcome":"already_present","observed_target":operations::wire_oid(&observed),"observed_at":observed_at,"caused_by_this_operation":false}));
        }
        if observed != publication.expected_old {
            return self.finish_publication(&mut publication,"rejected",json!({"outcome":"rejected","reason":"target_moved","observed_target":operations::wire_oid(&observed),"observed_at":observed_at}));
        }
        // This durable phase is the non-redispatch boundary, even if spawning fails.
        self.root.advance_operation(op,journal.generation,"dispatched",json!({"target":publication.target,"candidate":publication.candidate,"expected_old":publication.expected_old}))?;
        publication.state = "dispatched".into();
        self.save_publication(&publication)?;
        let args = vec![
            "push".into(),
            "--porcelain".into(),
            "--no-verify".into(),
            format!(
                "--force-with-lease={}:{}",
                publication.target, publication.expected_old
            ),
            b.remote_url.unwrap(),
            format!("{}:{}", publication.candidate, publication.target),
        ];
        let pushed = self.repositories.runner.run_with(
            &self.repositories.pool_path(repo),
            &args,
            &[],
            &[],
            Duration::from_millis(r.deadline_ms.unwrap_or(300_000).min(300_000)),
        );
        let output = match pushed {
            Ok(output) => output,
            Err(_) => {
                publication.state = "indeterminate".into();
                self.save_publication(&publication)?;
                return Err(Error::new(
                    "operation_indeterminate",
                    "remote push outcome is unknown; no automatic redispatch",
                ));
            }
        };
        let expected_refspec = format!("{}:{}", publication.candidate, publication.target);
        let mut outcomes = output
            .stdout
            .split(|b| *b == b'\n')
            .filter_map(|line| {
                let mut fields = line.split(|b| *b == b'\t');
                let flag = fields.next()?;
                let refspec = fields.next()?;
                let summary = fields.next()?;
                let target = refspec.rsplit(|b| *b == b':').next()?;
                if refspec != expected_refspec.as_bytes()
                    || target != publication.target.as_bytes()
                    || flag.len() != 1
                    || summary.is_empty()
                {
                    None
                } else {
                    Some(flag[0])
                }
            })
            .collect::<Vec<_>>();
        if outcomes.len() == 1 {
            let flag = outcomes.remove(0);
            if flag == b' ' && output.status.success() {
                let candidate = publication.candidate.clone();
                let result = json!({"outcome":"applied","target":publication.target,"old":operations::wire_oid(&publication.expected_old),"new":operations::wire_oid(&candidate),"acknowledged_at":chrono::Utc::now().to_rfc3339()});
                return self.finish_publication(&mut publication, "applied", result);
            }
            if flag == b'=' && output.status.success() {
                let candidate = publication.candidate.clone();
                return self.finish_publication(&mut publication,"already_present",json!({"outcome":"already_present","observed_target":operations::wire_oid(&candidate),"observed_at":chrono::Utc::now().to_rfc3339(),"caused_by_this_operation":false}));
            }
            if flag == b'!' {
                return self.finish_publication(&mut publication,"rejected",json!({"outcome":"rejected","reason":"remote_rejected","observed_at":chrono::Utc::now().to_rfc3339()}));
            }
        }
        publication.state = "indeterminate".into();
        self.save_publication(&publication)?;
        Err(Error::new(
            "operation_indeterminate",
            "push did not provide an unambiguous target acknowledgement",
        ))
    }
    fn reconcile_or_resolve(&self, r: &Request, b: &Binding) -> Result<Value> {
        let (target_id, expected_generation, resolve) = if r.operation == "operation.resolve" {
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
            let result = publication
                .result
                .clone()
                .ok_or_else(|| Error::new("state_corrupt", "resolved publication lacks result"))?;
            let state = publication.state.clone();
            self.finish_publication(&mut publication, &state, result.clone())?;
            if target.status == OperationStatus::Pending {
                self.root
                    .complete_operation(&target_id, target.generation, result.clone())?;
            }
            return Ok(json!({"target_operation_id":target_id,"resolution":result}));
        }
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
            self.save_publication(&publication)?;
        }
        if !matches!(publication.state.as_str(), "dispatched" | "indeterminate") {
            return Err(Error::new(
                "invalid_request",
                "target has no dispatched uncertain publication",
            ));
        }
        let result = if let Some(p) = resolve {
            if !["abandon", "permit_new_operation"].contains(&p.disposition.as_str())
                || p.reason.trim().is_empty()
                || !p.old_publishers_stopped
                || !(p.evidence.as_str().is_some_and(|s| !s.trim().is_empty())
                    || p.evidence.as_object().is_some_and(|o| !o.is_empty()))
            {
                return Err(Error::new(
                    "invalid_request",
                    "resolution requires disposition, reason, cleanup evidence and old_publishers_stopped=true",
                ));
            }
            publication.resolved_by = Some(op.into());
            self.finish_publication(&mut publication,"resolved",json!({"outcome":"resolved","disposition":p.disposition,"reason":p.reason,"evidence":p.evidence,"resolved_at":chrono::Utc::now().to_rfc3339(),"historical_remote_effect":"unknown"}))?
        } else {
            match self.publication_observe(b, &publication, op) {
                Ok((observed, at)) => {
                    if self.ancestor_at(
                        &self.repositories.pool_path(&b.repository_id),
                        &publication.candidate,
                        &observed,
                    )? {
                        publication.resolved_by = Some(op.into());
                        self.finish_publication(&mut publication,"already_present",json!({"outcome":"already_present","observed_target":operations::wire_oid(&observed),"observed_at":at,"caused_by_this_operation":false}))?
                    } else {
                        return Ok(
                            json!({"target_operation_id":target_id,"outcome":"indeterminate","observed_target":operations::wire_oid(&observed),"observed_at":at,"historical_remote_effect":"unknown"}),
                        );
                    }
                }
                Err(_) => {
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
