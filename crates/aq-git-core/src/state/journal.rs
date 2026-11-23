use super::StateRoot;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Pending,
    Complete,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationRecord {
    pub version: u32,
    pub id: String,
    pub payload: Value,
    pub payload_hash: String,
    pub generation: u64,
    pub status: OperationStatus,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub created_at_ms: i64,
    pub phase: String,
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
}
pub(crate) fn safe_id(id: &str) -> Result<()> {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_');
    if !(1..=128).contains(&id.len()) || !id.bytes().all(allowed) {
        return Err(Error::new(
            "invalid_id",
            "ID must contain 1..128 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: std::collections::BTreeMap<_, _> =
                m.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            serde_json::to_value(sorted).unwrap()
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        v => v.clone(),
    }
}
impl StateRoot {
    pub fn operation(&self, id: &str) -> Result<Option<OperationRecord>> {
        safe_id(id)?;
        let record: Option<OperationRecord> = self.read_json(format!("operations/{id}.json"))?;
        if record.as_ref().is_some_and(|r| r.version != 1) {
            return Err(Error::new(
                "state_incompatible",
                "unsupported operation format",
            ));
        }
        Ok(record)
    }
    pub fn begin_operation(&self, id: &str, payload: &Value) -> Result<OperationRecord> {
        safe_id(id)?;
        let _guard = crate::git::lock_with_deadline(&self.inner.mutex)?;
        let payload = canonical(payload);
        let hash = hex::encode(Sha256::digest(serde_json::to_vec(&payload).unwrap()));
        if let Some(record) = self.operation(id)? {
            if record.payload_hash != hash || record.payload != payload {
                return Err(Error::new(
                    "idempotency_conflict",
                    "operation ID already has another payload",
                ));
            }
            return Ok(record);
        }
        let now = chrono::Utc::now().to_rfc3339();
        let record = OperationRecord {
            version: 1,
            id: id.into(),
            payload,
            payload_hash: hash,
            generation: 1,
            status: OperationStatus::Pending,
            result: None,
            error: None,
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            phase: "intent".into(),
            metadata: Value::Null,
            created_at: now.clone(),
            updated_at: now,
        };
        self.write_json(format!("operations/{id}.json"), &record)?;
        Ok(record)
    }
    pub fn advance_operation(
        &self,
        id: &str,
        generation: u64,
        phase: impl Into<String>,
        metadata: Value,
    ) -> Result<OperationRecord> {
        self.update_pending(id, generation, |record| {
            record.phase = phase.into();
            record.metadata = metadata;
        })
    }
    pub fn complete_operation(
        &self,
        id: &str,
        generation: u64,
        result: Value,
    ) -> Result<OperationRecord> {
        self.finish_operation(id, generation, Ok(result))
    }
    pub fn fail_operation(
        &self,
        id: &str,
        generation: u64,
        error: Value,
    ) -> Result<OperationRecord> {
        self.finish_operation(id, generation, Err(error))
    }

    fn finish_operation(
        &self,
        id: &str,
        generation: u64,
        outcome: std::result::Result<Value, Value>,
    ) -> Result<OperationRecord> {
        self.update_pending(id, generation, |record| {
            (record.status, record.result, record.error) = match outcome {
                Ok(value) => (OperationStatus::Complete, Some(value), None),
                Err(error) => (OperationStatus::Failed, None, Some(error)),
            };
        })
    }

    fn update_pending(
        &self,
        id: &str,
        generation: u64,
        update: impl FnOnce(&mut OperationRecord),
    ) -> Result<OperationRecord> {
        let _guard = crate::git::lock_with_deadline(&self.inner.mutex)?;
        let mut record = self
            .operation(id)?
            .ok_or_else(|| Error::new("operation_not_found", "unknown operation"))?;
        if record.generation != generation {
            return Err(Error::new(
                "generation_conflict",
                "operation generation changed",
            ));
        }
        if record.status != OperationStatus::Pending {
            return Err(Error::new(
                "operation_terminal",
                "operation already terminal",
            ));
        }
        record.generation += 1;
        update(&mut record);
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.write_json(format!("operations/{id}.json"), &record)?;
        Ok(record)
    }
}
