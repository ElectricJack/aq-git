use super::*;

pub(super) struct PublicationAttempt<'a> {
    pub(super) service: &'a RepositoryService,
    pub(super) binding: &'a Binding,
    pub(super) op: &'a str,
}
impl Publish {
    pub(super) fn validate(&self, b: &Binding) -> Result<()> {
        if !self.has_source_ref() {
            if self.expected_source.is_some() {
                return Err(Error::new(
                    "invalid_request",
                    "expected_source requires source_ref",
                ));
            }
            operations::id(&self.candidate_id)?;
        }
        operations::oid(&self.expected_old)?;
        aq_git_protocol::validate_ref(&self.target).map_err(|e| Error::new(e.code, e.message))?;
        if self.target.starts_with(crate::git_http::TRY_PREFIX) {
            return Err(Error::new(
                "unsupported_feature",
                "try refs are never published",
            ));
        }
        match (b.target_scope(&self.target), self.update) {
            (None, _) => {
                return Err(Error::new(
                    "unsupported_feature",
                    "destination is not an allowed publication branch",
                ));
            }
            (Some(TargetScope::Exact), UpdateMode::Replace) => {
                return Err(Error::new(
                    "unsupported_feature",
                    "replace updates are limited to allowed target prefixes",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}
impl PublicationAttempt<'_> {
    pub(super) fn publish(&self, params: Publish, deadline_ms: Option<u64>) -> Result<Value> {
        let b = self.binding;
        let op = self.op;
        let mut publication = self.load(params)?;
        if publication.version != 1 || publication.binding_generation != b.binding_generation {
            return Err(Error::new(
                "state_incompatible",
                "publication record or binding is incompatible",
            ));
        }
        if let Some(result) = publication.result.clone() {
            return self.service.finish_publication(
                &mut publication.clone(),
                &publication.state,
                result,
            );
        }
        self.service.acquire_target(&publication)?;
        if let Some(candidate) = publication.retained_candidate() {
            self.service.root.acquire_dependency(candidate, op)?;
        }
        let journal = self
            .service
            .root
            .operation(op)?
            .ok_or_else(|| Error::new("operation_not_found", "missing publication intent"))?;
        if journal.phase == "dispatched"
            || matches!(publication.state.as_str(), "dispatched" | "indeterminate")
        {
            publication.state = "indeterminate".into();
            self.service.save_publication(&publication)?;
            return Err(Error::new(
                "operation_indeterminate",
                "publication was dispatched; reconcile before any new attempt",
            )
            .details(json!({"operation_id":op,"target":publication.target})));
        }
        if let Some(result) = self.preflight(&mut publication)? {
            return Ok(result);
        }
        self.push(&mut publication, journal.generation, deadline_ms)
    }
    fn load(&self, params: Publish) -> Result<PublicationRecord> {
        let b = self.binding;
        let repo = &b.repository_id;
        let op = self.op;
        let existing: Option<PublicationRecord> = self
            .service
            .root
            .read_json(format!("repositories/{repo}/publications/{op}.json"))?;
        if let Some(record) = existing {
            return Ok(record);
        }
        let publication = if params.has_source_ref() {
            self.service.ref_publication(b, op, &params)?
        } else {
            self.prepare_candidate(params)?
        };
        self.service.save_publication(&publication)?;
        Ok(publication)
    }
    fn prepare_candidate(&self, params: Publish) -> Result<PublicationRecord> {
        let b = self.binding;
        let repo = &b.repository_id;
        let expected = params.expected_old.hex.as_str();
        let candidate = self.service.candidate_record(repo, &params.candidate_id)?;
        if candidate.state != "ready" || candidate.binding_generation != b.binding_generation {
            return Err(Error::new(
                "conflicted",
                "publication requires a ready candidate in the current binding",
            ));
        }
        self.service.active_handle(repo, &params.candidate_id)?;
        // A replacement lease compares the remote value only; the old head
        // need not be local or related to the candidate.
        let fast_forward = params.update.is_fast_forward();
        if fast_forward && !absent(expected) {
            self.service.commit_exists(repo, expected)?;
            if !self.service.ancestor_at(
                &self.service.repositories.pool_path(repo),
                expected,
                &candidate.prefix,
            )? {
                return Err(Error::new(
                    "target_moved",
                    "candidate is not a fast-forward of the explicit expected old revision",
                ));
            }
        }
        Ok(self.candidate_intent(params, candidate.prefix))
    }

    fn candidate_intent(&self, params: Publish, commit: String) -> PublicationRecord {
        let b = self.binding;
        let repo = &b.repository_id;
        let op = self.op;
        let expected = params.expected_old.hex.as_str();
        PublicationRecord {
            version: 1,
            operation_id: op.into(),
            repository_id: repo.into(),
            binding_generation: b.binding_generation,
            candidate_id: params.candidate_id,
            candidate: commit,
            target: params.target,
            expected_old: expected.into(),
            update: params.update,
            state: "preparing".into(),
            result: None,
            resolved_by: None,
            source_ref: None,
        }
    }
    fn preflight(&self, publication: &mut PublicationRecord) -> Result<Option<Value>> {
        let b = self.binding;
        let op = self.op;
        let (observed, observed_at) = match self.service.publication_observe(b, publication, op) {
            Ok(observation) => observation,
            Err(error) if error.code == "remote_auth_failed" => {
                self.reject_preflight(publication, "remote_auth_failed")?;
                return Err(error);
            }
            Err(_) => {
                return self
                    .reject_preflight(publication, "preflight_observation_unavailable")
                    .map(Some);
            }
        };
        // An absent-lease creation never treats an existing branch as present.
        if !absent(&publication.expected_old)
            && self.service.publication_present(publication, &observed)?
        {
            return self.service.finish_publication(publication,"already_present",json!({"outcome":"already_present","observed_target":operations::wire_oid(&observed),"observed_at":observed_at,"caused_by_this_operation":false})).map(Some);
        }
        if observed != publication.expected_old {
            return self.service.finish_publication(publication,"rejected",json!({"outcome":"rejected","reason":"target_moved","observed_target":operations::wire_oid(&observed),"observed_at":observed_at})).map(Some);
        }
        Ok(None)
    }
    fn reject_preflight(&self, publication: &mut PublicationRecord, reason: &str) -> Result<Value> {
        self.service.finish_publication(
            publication,
            "rejected",
            json!({"outcome": "rejected", "reason": reason, "dispatched": false}),
        )
    }

    fn push(
        &self,
        publication: &mut PublicationRecord,
        generation: u64,
        deadline_ms: Option<u64>,
    ) -> Result<Value> {
        let b = self.binding;
        let op = self.op;
        // This durable phase is the non-redispatch boundary, even if spawning fails.
        self.service.root.advance_operation(op,generation,"dispatched",json!({"target":publication.target,"candidate":publication.candidate,"expected_old":publication.expected_old}))?;
        publication.state = "dispatched".into();
        self.service.save_publication(publication)?;
        let args = push_args(publication, b);
        let pushed = self.service.repositories.remote_run(
            b,
            &args,
            Duration::from_millis(deadline_ms.unwrap_or(300_000).min(300_000)),
        );
        let output = match pushed {
            Ok(output) => output,
            Err(error) => {
                publication.state = "indeterminate".into();
                self.service.save_publication(publication)?;
                if error.code == "remote_auth_failed" {
                    return Err(
                        error.details(json!({"remote_effect_unknown":true,"dispatched":true}))
                    );
                }
                return Err(Error::new(
                    "operation_indeterminate",
                    "remote push outcome is unknown; no automatic redispatch",
                ));
            }
        };
        self.acknowledge(publication, output)
    }
    fn acknowledge(
        &self,
        publication: &mut PublicationRecord,
        output: crate::git::GitOutput,
    ) -> Result<Value> {
        if let Some((flag, class)) = acknowledgement_flag(publication, &output.stdout) {
            let applied = applied_update(flag, publication);
            if applied && output.status.success() {
                let candidate = publication.candidate.clone();
                let result = json!({"outcome":"applied","target":publication.target,"old":operations::wire_oid(&publication.expected_old),"new":operations::wire_oid(&candidate),"acknowledged_at":chrono::Utc::now().to_rfc3339()});
                return self
                    .service
                    .finish_publication(publication, "applied", result);
            }
            if flag == b'=' && output.status.success() {
                let candidate = publication.candidate.clone();
                return self.service.finish_publication(publication,"already_present",json!({"outcome":"already_present","observed_target":operations::wire_oid(&candidate),"observed_at":chrono::Utc::now().to_rfc3339(),"caused_by_this_operation":false}));
            }
            if flag == b'!' {
                return self.service.finish_publication(publication,"rejected",json!({"outcome":"rejected","reason":"remote_rejected","rejection_class":class,"observed_at":chrono::Utc::now().to_rfc3339()}));
            }
        }
        publication.state = "indeterminate".into();
        self.service.save_publication(publication)?;
        Err(Error::new(
            "operation_indeterminate",
            "push did not provide an unambiguous target acknowledgement",
        ))
    }
}
fn acknowledgement_flag(
    publication: &PublicationRecord,
    bytes: &[u8],
) -> Option<(u8, RejectionClass)> {
    let expected_refspec = format!("{}:{}", publication.candidate, publication.target);
    let outcomes = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| {
            flag_for_target(
                line,
                expected_refspec.as_bytes(),
                publication.target.as_bytes(),
            )
        })
        .collect::<Vec<_>>();
    match outcomes.as_slice() {
        [ack] => Some(*ack),
        _ => None,
    }
}

fn applied_update(flag: u8, publication: &PublicationRecord) -> bool {
    match flag {
        b' ' => true,
        b'+' => publication.update == UpdateMode::Replace,
        b'*' => absent(&publication.expected_old),
        _ => false,
    }
}

fn push_args(publication: &PublicationRecord, b: &Binding) -> Vec<String> {
    vec![
        "push".into(),
        "--porcelain".into(),
        "--no-verify".into(),
        format!(
            "--force-with-lease={}:{}",
            publication.target,
            if absent(&publication.expected_old) {
                ""
            } else {
                &publication.expected_old
            }
        ),
        b.remote_url.clone().unwrap(),
        format!("{}:{}", publication.candidate, publication.target),
    ]
}

fn flag_for_target(
    line: &[u8],
    expected_refspec: &[u8],
    expected_target: &[u8],
) -> Option<(u8, RejectionClass)> {
    let mut fields = line.split(|b| *b == b'\t');
    let flag = fields.next()?;
    let refspec = fields.next()?;
    let summary = fields.next()?;
    let target = refspec.rsplit(|b| *b == b':').next()?;
    let target_matches = refspec == expected_refspec && target == expected_target;
    let valid = flag.len() == 1 && !summary.is_empty();
    if !target_matches || !valid {
        return None;
    }
    // A rejection with extra fields is malformed evidence and stays unknown.
    let class = if flag[0] == b'!' && fields.next().is_none() {
        RejectionClass::from_summary(summary)
    } else {
        RejectionClass::Unknown
    };
    Some((flag[0], class))
}
