//! Frozen, retained ordered merge candidates. No worktree or moving ref is input.
use crate::{
    Error, Result,
    operations::{self, CommitMetadata},
    service::RepositoryService,
    state::RetentionRef,
};
use aq_git_protocol::{ObjectId, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub const OPERATIONS: &[&str] = &["candidate.create", "candidate.show"];
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSource {
    pub oid: ObjectId,
    pub retention_handle: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    base: ObjectId,
    sources: Vec<CandidateSource>,
    commit: CommitMetadata,
    #[serde(default = "profile")]
    merge_profile: String,
    #[serde(default)]
    supersedes: Option<String>,
}
fn profile() -> String {
    "git-ort-v1".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Show {
    candidate_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub version: u32,
    pub candidate_id: String,
    pub repository_id: String,
    pub binding_generation: u64,
    pub base: String,
    pub sources: Vec<CandidateSource>,
    pub commit: CommitMetadata,
    pub merge_profile: String,
    pub supersedes: Option<String>,
    pub state: String,
    pub next_source: usize,
    pub prefix: String,
    pub refs: Vec<RetentionRef>,
    pub conflicting_paths: Vec<Value>,
    #[serde(default)]
    pub conflict_paths_truncated: bool,
    #[serde(default)]
    pub conflict_path_count: usize,
    pub conflict_step: Option<usize>,
    pub git_version: String,
}
impl RepositoryService {
    pub(crate) fn candidate_record(&self, repo: &str, candidate: &str) -> Result<CandidateRecord> {
        operations::id(candidate)?;
        self.root
            .read_json(format!("repositories/{repo}/candidates/{candidate}.json"))?
            .ok_or_else(|| Error::new("candidate_not_found", "candidate not found"))
    }
    fn save_candidate(&self, c: &CandidateRecord) -> Result<()> {
        self.root.write_json(
            format!(
                "repositories/{}/candidates/{}.json",
                c.repository_id, c.candidate_id
            ),
            c,
        )
    }
    fn candidate_ref(&self, repo: &str, reference: &RetentionRef) -> Result<()> {
        let output = self.repositories.runner.run(
            &self.repositories.pool_path(repo),
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                reference.name.clone(),
            ],
        )?;
        if output.status.success() {
            if String::from_utf8_lossy(&output.stdout).trim() != reference.oid {
                return Err(Error::new(
                    "retention_conflict",
                    "candidate ref differs from recorded intent",
                ));
            }
        } else if output.status.code() == Some(1) {
            self.repositories.git(
                repo,
                vec![
                    "update-ref".into(),
                    reference.name.clone(),
                    reference.oid.clone(),
                    "0".repeat(40),
                ],
            )?;
        } else {
            return Err(Error::new(
                "git_failed",
                "candidate retention ref inspection failed",
            ));
        }
        Ok(())
    }
    // All public mutations hold repository coordination. Extend the handle
    // only after the corresponding exact refs and candidate step are durable.
    fn retain_candidate_prefix(&self, c: &CandidateRecord) -> Result<()> {
        let intent = self
            .root
            .operation(&c.candidate_id)?
            .ok_or_else(|| Error::new("operation_not_found", "candidate intent is missing"))?;
        if intent.status != crate::state::OperationStatus::Pending {
            return Err(Error::new(
                "operation_terminal",
                "candidate creation is no longer pending",
            ));
        }
        let Some(handle) = self.root.retention(&c.candidate_id)? else {
            return self.pin(operations::Pin {
                repo: &c.repository_id,
                generation: c.binding_generation,
                handle: &c.candidate_id,
                resource: "candidate",
                refs: c.refs.clone(),
            });
        };
        if handle.state != crate::state::RetentionState::Active {
            return Err(Error::new(
                "retention_released",
                "candidate retention was explicitly released",
            ));
        }
        validate_candidate_handle(c, &handle)?;
        if handle.refs != c.refs {
            self.extend_candidate_prefix(c, handle)?;
        }
        Ok(())
    }
    fn extend_candidate_prefix(
        &self,
        c: &CandidateRecord,
        mut handle: crate::state::RetentionRecord,
    ) -> Result<()> {
        let _guard = self
            .root
            .inner
            .mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = self
            .root
            .retention(&c.candidate_id)?
            .ok_or_else(|| Error::new("retention_not_found", "candidate handle disappeared"))?;
        if current != handle {
            return Err(Error::new(
                "generation_conflict",
                "candidate retention changed during prefix extension",
            ));
        }
        handle.refs = c.refs.clone();
        handle.generation = handle
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::new("state_incompatible", "retention generation exhausted"))?;
        self.root
            .write_json(format!("retention/{}.json", c.candidate_id), &handle)?;
        Ok(())
    }
    fn candidate_result(&self, c: &CandidateRecord) -> Result<Value> {
        Ok(
            json!({"candidate_id":c.candidate_id,"state":c.state,"base":operations::wire_oid(&c.base),"sources":c.sources,"merge_profile":c.merge_profile,"git_version":c.git_version,"commit":c.commit,"supersedes":c.supersedes,"candidate":if c.state=="ready"{Some(operations::wire_oid(&c.prefix))}else{None},"clean_prefix":operations::wire_oid(&c.prefix),"conflict_step":c.conflict_step,"conflicting_paths":c.conflicting_paths,"conflict_paths_truncated":c.conflict_paths_truncated,"conflict_path_count":c.conflict_path_count,"retention_handle":c.candidate_id}),
        )
    }
    pub(crate) fn candidate_handle(&self, r: &Request) -> Result<Value> {
        let binding = self.repositories.checked_binding(r)?;
        let repo = &binding.repository_id;
        if r.operation == "candidate.show" {
            let p: Show = serde_json::from_value(r.params.clone())?;
            return self.candidate_result(&self.candidate_record(repo, &p.candidate_id)?);
        }
        if r.operation != "candidate.create" {
            return Err(Error::new(
                "unsupported_feature",
                "candidate operation unavailable",
            ));
        }
        let p: Create = serde_json::from_value(r.params.clone())?;
        p.validate()?;
        let cid = operations::opid(r)?;
        let build = CandidateBuild {
            service: self,
            repo,
            cid,
        };
        let mut candidate = build.load(&binding, p)?;
        if candidate.version != 1 || candidate.binding_generation != binding.binding_generation {
            return Err(Error::new(
                "state_incompatible",
                "candidate format or binding is incompatible",
            ));
        }
        if candidate.state == "building" {
            build.retain_inputs(&candidate)?;
            build.merge_sources(&mut candidate)?;
        }
        self.retain_candidate_prefix(&candidate)?;
        for source in &candidate.sources {
            self.root
                .release_dependency(&source.retention_handle, cid)?;
        }
        self.candidate_result(&candidate)
    }
}

fn validate_candidate_handle(
    c: &CandidateRecord,
    handle: &crate::state::RetentionRecord,
) -> Result<()> {
    let compatible = (
        &handle.repository_id,
        handle.binding_generation,
        handle.resource.as_str(),
    ) == (&c.repository_id, c.binding_generation, "candidate");
    let contains_refs = handle
        .refs
        .iter()
        .all(|reference| c.refs.contains(reference));
    if !compatible || !contains_refs {
        return Err(Error::new(
            "retention_conflict",
            "candidate handle does not match frozen prefix intent",
        ));
    }
    Ok(())
}

impl Create {
    fn validate(&self) -> Result<()> {
        operations::oid(&self.base)?;
        self.commit.validate()?;
        if self.sources.len() > 64 {
            return Err(Error::new(
                "limit_exceeded",
                "candidate supports at most 64 sources",
            ));
        }
        if self.merge_profile != "git-ort-v1" {
            return Err(Error::new(
                "unsupported_feature",
                "merge profile is unsupported",
            ));
        }
        if let Some(id) = &self.supersedes {
            operations::id(id)?;
        }
        Ok(())
    }
}

struct CandidateBuild<'a> {
    service: &'a RepositoryService,
    repo: &'a str,
    cid: &'a str,
}

impl CandidateBuild<'_> {
    fn input_refs(
        &self,
        binding: &crate::repository::Binding,
        p: &Create,
    ) -> Result<Vec<RetentionRef>> {
        let repo = self.repo;
        let cid = self.cid;
        let base = p.base.hex.as_str();
        self.service.commit_exists(repo, base)?;
        self.service.validate_tree_features(repo, base)?;
        let mut refs = vec![RetentionRef {
            name: format!("refs/aq-git/candidates/{cid}/base"),
            oid: base.into(),
        }];
        for (index, source) in p.sources.iter().enumerate() {
            let oid = operations::oid(&source.oid)?;
            operations::id(&source.retention_handle)?;
            let handle = self.service.active_handle(repo, &source.retention_handle)?;
            if handle.binding_generation != binding.binding_generation
                || !handle.refs.iter().any(|r| r.oid == oid)
            {
                return Err(Error::new(
                    "retention_conflict",
                    "source handle does not retain the requested source at this binding generation",
                ));
            }
            self.service.commit_exists(repo, oid)?;
            self.service.validate_tree_features(repo, oid)?;
            refs.push(RetentionRef {
                name: format!("refs/aq-git/candidates/{cid}/inputs/{index}"),
                oid: oid.into(),
            });
        }
        Ok(refs)
    }
    fn load(&self, binding: &crate::repository::Binding, p: Create) -> Result<CandidateRecord> {
        let repo = self.repo;
        let cid = self.cid;
        let existing: Option<CandidateRecord> = self
            .service
            .root
            .read_json(format!("repositories/{repo}/candidates/{cid}.json"))?;
        if let Some(c) = existing {
            return Ok(c);
        }
        let base = p.base.hex.as_str();
        let refs = self.input_refs(binding, &p)?;
        let version = String::from_utf8_lossy(&self.service.git_at(
            &self.service.repositories.pool_path(repo),
            vec!["--version".into()],
        )?)
        .trim()
        .to_string();
        let c = CandidateRecord {
            version: 1,
            candidate_id: cid.into(),
            repository_id: repo.into(),
            binding_generation: binding.binding_generation,
            base: base.into(),
            sources: p.sources,
            commit: p.commit,
            merge_profile: p.merge_profile,
            supersedes: p.supersedes,
            state: "building".into(),
            next_source: 0,
            prefix: base.into(),
            refs,
            conflicting_paths: vec![],
            conflict_paths_truncated: false,
            conflict_path_count: 0,
            conflict_step: None,
            git_version: version,
        };
        self.service.save_candidate(&c)?;
        Ok(c)
    }
    fn acquire_inputs(&self, candidate: &CandidateRecord) -> Result<()> {
        let cid = self.cid;
        let mut attempted = Vec::new();
        for source in &candidate.sources {
            attempted.push(source.retention_handle.as_str());
            if let Err(error) = self
                .service
                .root
                .acquire_dependency(&source.retention_handle, cid)
            {
                // A crash may leave only some edges installed. If a
                // later input has since been explicitly released, do
                // not strand earlier handles behind a failed creation.
                for handle in attempted {
                    self.service.root.release_dependency(handle, cid)?;
                }
                return Err(error);
            }
        }
        Ok(())
    }
    fn retain_inputs(&self, candidate: &CandidateRecord) -> Result<()> {
        let repo = self.repo;
        let cid = self.cid;
        let existing_handle = self.service.root.retention(cid)?;
        if existing_handle
            .as_ref()
            .is_some_and(|h| h.state != crate::state::RetentionState::Active)
        {
            return Err(Error::new(
                "retention_released",
                "candidate retention was explicitly released",
            ));
        }
        // Temporary source dependencies end once an independently releasable
        // candidate input handle owns every required object.
        if existing_handle.is_none() {
            self.acquire_inputs(candidate)?;
        }
        let retained = (|| -> Result<()> {
            for reference in &candidate.refs {
                self.service.candidate_ref(repo, reference)?;
            }
            self.service.retain_candidate_prefix(candidate)
        })();
        if let Err(error) = retained {
            if self.service.root.retention(cid)?.is_none() {
                for source in &candidate.sources {
                    self.service
                        .root
                        .release_dependency(&source.retention_handle, cid)?;
                }
            }
            return Err(error);
        }
        for source in &candidate.sources {
            self.service
                .root
                .release_dependency(&source.retention_handle, cid)?;
        }
        Ok(())
    }
    fn merge_sources(&self, candidate: &mut CandidateRecord) -> Result<()> {
        let repo = self.repo;
        let path = self.service.repositories.pool_path(repo);
        let current_version =
            String::from_utf8_lossy(&self.service.git_at(&path, vec!["--version".into()])?)
                .trim()
                .to_string();
        if current_version != candidate.git_version {
            return Err(Error::new(
                "state_incompatible",
                "cannot resume candidate with another Git version",
            ));
        }
        while candidate.next_source < candidate.sources.len() {
            self.advance(&path, candidate)?;
            self.service.save_candidate(candidate)?;
            self.service.retain_candidate_prefix(candidate)?;
            if candidate.state == "conflicted" {
                break;
            }
        }
        if candidate.state == "building" {
            candidate.state = "ready".into();
            self.service.save_candidate(candidate)?;
        }
        Ok(())
    }
    fn advance(&self, path: &std::path::Path, candidate: &mut CandidateRecord) -> Result<()> {
        let cid = self.cid;
        let repo = self.repo;
        let index = candidate.next_source;
        let source = candidate.sources[index].oid.hex.clone();
        let next = if self.service.ancestor_at(path, &source, &candidate.prefix)? {
            Some(candidate.prefix.clone())
        } else if self.service.ancestor_at(path, &candidate.prefix, &source)? {
            Some(source.clone())
        } else {
            self.merge_diverged(path, candidate, source)?
        };
        if let Some(next) = next {
            let reference = RetentionRef {
                name: format!("refs/aq-git/candidates/{cid}/prefix/{index}"),
                oid: next.clone(),
            };
            // Commit bytes and exact ref precede acknowledgement of a prefix step.
            self.service.candidate_ref(repo, &reference)?;
            candidate.refs.push(reference);
            candidate.prefix = next;
            candidate.next_source += 1;
        }
        Ok(())
    }
    fn merge_diverged(
        &self,
        path: &std::path::Path,
        candidate: &mut CandidateRecord,
        source: String,
    ) -> Result<Option<String>> {
        let index = candidate.next_source;
        let cid = self.cid;
        let output = self.service.repositories.runner.run(
            path,
            &[
                "-c".into(),
                "merge.renormalize=false".into(),
                "-c".into(),
                "merge.renames=true".into(),
                "-c".into(),
                "merge.directoryRenames=conflict".into(),
                "merge-tree".into(),
                "--write-tree".into(),
                "--name-only".into(),
                "--no-messages".into(),
                "-z".into(),
                candidate.prefix.clone(),
                source.clone(),
            ],
        )?;
        let mut fields = output.stdout.split(|b| *b == 0);
        let tree = fields.next().unwrap_or_default();
        let valid_tree = tree.len() == 40 && tree.iter().all(u8::is_ascii_hexdigit);
        if ![Some(0), Some(1)].contains(&output.status.code()) || !valid_tree {
            return Err(Error::new(
                "git_failed",
                "merge-tree did not return a valid structured result",
            ));
        }
        if output.status.success() {
            let tree = std::str::from_utf8(tree)
                .map_err(|_| Error::new("git_failed", "invalid merge tree OID"))?;
            Ok(Some(self.service.create_commit(
                path,
                crate::workspace::CommitTree {
                    tree,
                    parents: &[candidate.prefix.clone(), source],
                    metadata: &candidate.commit,
                    author: None,
                    operation: cid,
                },
            )?))
        } else {
            let (paths, truncated, count) = bounded_conflict_paths(fields);
            candidate.conflicting_paths = paths;
            candidate.conflict_paths_truncated = truncated;
            candidate.conflict_path_count = count;
            candidate.state = "conflicted".into();
            candidate.conflict_step = Some(index);
            Ok(None)
        }
    }
}

const MAX_CONFLICT_PATH_BYTES: usize = 1024 * 1024;
fn bounded_conflict_paths<'a>(fields: impl Iterator<Item = &'a [u8]>) -> (Vec<Value>, bool, usize) {
    let mut paths = Vec::new();
    let mut bytes = 2usize;
    let mut observed = 0usize;
    let mut truncated = false;
    for path in fields.filter(|path| !path.is_empty()) {
        observed += 1;
        // Hex doubles input size. Reserve a conservative JSON-object overhead
        // before allocating either the hex string or encoded path object.
        let size = path.len().saturating_mul(2).saturating_add(64);
        if paths.len() >= 10_000 || size > MAX_CONFLICT_PATH_BYTES.saturating_sub(bytes) {
            truncated = true;
            continue;
        }
        bytes += size;
        paths.push(json!({"encoding":"hex","value":hex::encode(path)}));
    }
    (paths, truncated, observed)
}
#[cfg(test)]
mod bounds_tests {
    use super::*;
    #[test]
    fn conflict_paths_bound_encoded_bytes_and_report_omissions() {
        let large = vec![b'x'; 600_000];
        let (paths, truncated, count) =
            bounded_conflict_paths([large.as_slice(), b"a"].into_iter());
        assert!(truncated);
        assert_eq!(count, 2);
        assert_eq!(
            paths,
            json!([{"encoding":"hex","value":"61"}])
                .as_array()
                .unwrap()
                .clone()
        );
        assert!(serde_json::to_vec(&paths).unwrap().len() <= MAX_CONFLICT_PATH_BYTES);
        let (paths, truncated, count) =
            bounded_conflict_paths(std::iter::repeat_n(b"a".as_slice(), 10_001));
        assert_eq!(paths.len(), 10_000);
        assert!(truncated);
        assert_eq!(count, 10_001);
        assert!(serde_json::to_vec(&paths).unwrap().len() <= MAX_CONFLICT_PATH_BYTES);
    }
}
