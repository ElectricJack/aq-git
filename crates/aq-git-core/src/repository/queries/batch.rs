use super::*;

impl RepositoryManager {
    pub(super) fn batch_query(&self, r: &Request, b: &Binding) -> Result<Value> {
        let items = r.params["items"]
            .as_array()
            .ok_or_else(|| Error::new("invalid_request", "items array required"))?;
        if items.len() > 100 {
            return Err(Error::new("limit_exceeded", "Batch exceeds 100 items"));
        }
        let prepared = items
            .iter()
            .map(|item| prepare_item(r, b, item))
            .collect::<Vec<_>>();
        let hits = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            prepared
                .iter()
                .map(|item| {
                    item.as_ref()
                        .ok()
                        .and_then(|(_, key)| cache.lookup(key, false))
                })
                .collect::<Vec<_>>()
        };
        let mut results = Vec::with_capacity(items.len());
        let mut encoded_size = BatchSize::new();
        for (item, hit) in prepared.into_iter().zip(hits) {
            let answer = match (item, hit) {
                (Err(error), _) => Err(error),
                (Ok(_), Some((value, serialized_len))) => {
                    // Enforce the bound before deep-cloning this or subsequent hits.
                    encoded_size.push_success(serialized_len)?;
                    results.push(batch_success(value.as_ref().clone()));
                    continue;
                }
                (Ok((query, key)), None) => self.query(&query, b).and_then(|value| {
                    let length = self
                        .cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entries
                        .get(&key)
                        .map(|(_, _, length)| *length);
                    let length = match length {
                        Some(length) => length,
                        None => serde_json::to_vec(&value)?.len(),
                    };
                    Ok((value, length))
                }),
            };
            match answer {
                Ok((value, length)) => {
                    encoded_size.push_success(length)?;
                    results.push(batch_success(value));
                }
                Err(error) => {
                    let wrapper = batch_error(error);
                    encoded_size.push(serde_json::to_vec(&wrapper)?.len())?;
                    results.push(wrapper);
                }
            }
        }
        let mut wrapper = serde_json::Map::new();
        wrapper.insert("items".into(), Value::Array(results));
        let value = Value::Object(wrapper);
        Ok(value)
    }
}

fn prepare_item(r: &Request, b: &Binding, item: &Value) -> Result<(Request, String)> {
    let object = item
        .as_object()
        .ok_or_else(|| Error::new("invalid_request", "Batch item must be an object"))?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "operation" | "params"))
    {
        return Err(Error::new("invalid_request", "Unknown batch item field"));
    }
    let operation = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::new("invalid_request", "Batch item operation required"))?;
    if !["inspect", "contains", "tree", "diff"].contains(&operation) {
        return Err(Error::new(
            "invalid_request",
            "Batch items must be independent exact reads",
        ));
    }
    let query = Request {
        protocol_version: r.protocol_version,
        request_id: r.request_id.clone(),
        operation: operation.into(),
        repository_id: r.repository_id.clone(),
        operation_id: r.operation_id.clone(),
        expected_binding_generation: r.expected_binding_generation,
        params: object.get("params").cloned().unwrap_or_else(|| json!({})),
        deadline_ms: r.deadline_ms,
    };
    validate_query_options(&query)?;
    let key = format!(
        "{}:{}:{}:{}",
        b.repository_id, b.binding_generation, query.operation, query.params
    );
    Ok((query, key))
}

fn batch_error(error: Error) -> Value {
    let mut details = serde_json::Map::new();
    details.insert("code".into(), Value::String(error.code));
    details.insert("message".into(), Value::String(error.message));
    details.insert("details".into(), error.details);
    let mut wrapper = serde_json::Map::new();
    wrapper.insert("ok".into(), Value::Bool(false));
    wrapper.insert("error".into(), Value::Object(details));
    Value::Object(wrapper)
}

pub(super) fn batch_success(value: Value) -> Value {
    let mut wrapper = serde_json::Map::new();
    wrapper.insert("ok".into(), Value::Bool(true));
    wrapper.insert("result".into(), value);
    Value::Object(wrapper)
}

pub(super) struct BatchSize {
    pub(super) bytes: usize,
    pub(super) items: usize,
}
impl BatchSize {
    pub(super) fn new() -> Self {
        Self {
            bytes: b"{\"items\":[]}".len(),
            items: 0,
        }
    }
    pub(super) fn push_success(&mut self, result_bytes: usize) -> Result<()> {
        self.push(
            b"{\"ok\":true,\"result\":}"
                .len()
                .saturating_add(result_bytes),
        )
    }
    pub(super) fn push(&mut self, item_bytes: usize) -> Result<()> {
        let bytes = self
            .bytes
            .saturating_add(usize::from(self.items > 0))
            .saturating_add(item_bytes);
        if bytes > 1024 * 1024 {
            return Err(Error::new("limit_exceeded", "Batch response exceeds 1 MiB"));
        }
        self.bytes = bytes;
        self.items += 1;
        Ok(())
    }
}
