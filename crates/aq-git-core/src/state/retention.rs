use super::{StateRoot, journal::safe_id};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionState {
    Active,
    Releasing,
    Released,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionRef {
    pub name: String,
    pub oid: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionRecord {
    pub version: u32,
    pub id: String,
    pub repository_id: String,
    pub binding_generation: u64,
    pub generation: u64,
    pub resource: String,
    pub refs: Vec<RetentionRef>,
    pub state: RetentionState,
    pub dependencies: BTreeSet<String>,
    pub release_operation: Option<String>,
    pub release_reason: Option<String>,
}
impl StateRoot {
    pub fn retention(&self, id: &str) -> Result<Option<RetentionRecord>> {
        safe_id(id)?;
        let record: Option<RetentionRecord> = self.read_json(format!("retention/{id}.json"))?;
        if record.as_ref().is_some_and(|r| r.version != 1) {
            return Err(Error::new(
                "state_incompatible",
                "unsupported retention format",
            ));
        }
        Ok(record)
    }
    pub fn create_retention(&self, record: &RetentionRecord) -> Result<()> {
        safe_id(&record.id)?;
        safe_id(&record.repository_id)?;
        if record.refs.iter().any(|r| {
            !(r.name.starts_with("refs/aq/") || r.name.starts_with("refs/aq-git/"))
                || r.name.contains("..")
                || r.name.contains('@')
                || r.name.ends_with(".lock")
                || r.name
                    .bytes()
                    .any(|b| b <= 32 || b == 127 || b"~^:?*[\\".contains(&b))
                || r.oid.len() != 40
                || !r
                    .oid
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }) {
            return Err(Error::new(
                "invalid_retention",
                "retention refs and SHA-1 OIDs must be exact managed values",
            ));
        }

        let _g = crate::git::lock_with_deadline(&self.inner.mutex)?;
        if record.version != 1 || record.generation != 1 || record.state != RetentionState::Active {
            return Err(Error::new(
                "invalid_retention",
                "new handle must be active version/generation one",
            ));
        }
        if let Some(existing) = self.retention(&record.id)? {
            if existing == *record {
                return Ok(());
            }
            return Err(Error::new("retention_conflict", "retention handle exists"));
        }
        self.write_json(format!("retention/{}.json", record.id), record)
    }
    pub fn acquire_dependency(&self, handle: &str, dependent: &str) -> Result<RetentionRecord> {
        safe_id(dependent)?;
        let _g = crate::git::lock_with_deadline(&self.inner.mutex)?;
        let mut r = self
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "unknown handle"))?;
        if r.state != RetentionState::Active {
            return Err(Error::new("retention_released", "handle is not active"));
        }
        if handle == dependent {
            return Err(Error::new("retention_cycle", "self dependency"));
        }
        let mut seen = BTreeSet::new();
        if self.reaches(dependent, handle, &mut seen)? {
            return Err(Error::new("retention_cycle", "dependency introduces cycle"));
        }
        if r.dependencies.insert(dependent.into()) {
            r.generation += 1;
            self.write_json(format!("retention/{handle}.json"), &r)?;
        }
        Ok(r)
    }
    fn reaches(&self, from: &str, target: &str, seen: &mut BTreeSet<String>) -> Result<bool> {
        if from == target {
            return Ok(true);
        }
        if !seen.insert(from.into()) {
            return Ok(false);
        }
        if let Some(r) = self.retention(from)? {
            for d in r.dependencies {
                if self.reaches(&d, target, seen)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    pub fn release_dependency(&self, handle: &str, dependent: &str) -> Result<RetentionRecord> {
        let _g = crate::git::lock_with_deadline(&self.inner.mutex)?;
        let mut r = self
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "unknown handle"))?;
        if r.dependencies.remove(dependent) {
            r.generation += 1;
            self.write_json(format!("retention/{handle}.json"), &r)?;
        }
        Ok(r)
    }
    pub fn begin_release(&self, handle: &str, generation: u64) -> Result<RetentionRecord> {
        self.begin_release_with_reason(handle, generation, None, None)
    }
    pub fn begin_release_with_reason(
        &self,
        handle: &str,
        generation: u64,
        operation: Option<String>,
        reason: Option<String>,
    ) -> Result<RetentionRecord> {
        let _g = crate::git::lock_with_deadline(&self.inner.mutex)?;
        let mut r = self
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "unknown handle"))?;
        if r.generation != generation {
            return Err(Error::new(
                "generation_conflict",
                "retention generation changed",
            ));
        }
        if !r.dependencies.is_empty() {
            let mut e = Error::new("retention_in_use", "handle has live dependencies");
            e.details = serde_json::json!({"blockers":r.dependencies});
            return Err(e);
        }
        if r.state == RetentionState::Active {
            r.state = RetentionState::Releasing;
            r.release_operation = operation;
            r.release_reason = reason;
            r.generation += 1;
            self.write_json(format!("retention/{handle}.json"), &r)?;
        }
        Ok(r)
    }
    /// Caller must remove and harden every exact ref using expected-old checks before this transition.
    pub fn finish_release(&self, handle: &str, generation: u64) -> Result<RetentionRecord> {
        let _g = crate::git::lock_with_deadline(&self.inner.mutex)?;
        let mut r = self
            .retention(handle)?
            .ok_or_else(|| Error::new("retention_not_found", "unknown handle"))?;
        if r.generation != generation {
            return Err(Error::new(
                "generation_conflict",
                "retention generation changed",
            ));
        }
        if r.state != RetentionState::Releasing {
            return Err(Error::new("retention_conflict", "release intent required"));
        }
        r.state = RetentionState::Released;
        r.generation += 1;
        self.write_json(format!("retention/{handle}.json"), &r)?;
        Ok(r)
    }
}
