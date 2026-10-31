use super::*;
fn oid(p: &Value, key: &str) -> Result<String> {
    let v = p
        .get(key)
        .ok_or_else(|| Error::new("invalid_request", format!("{key} required")))?;
    let o: aq_git_protocol::ObjectId = serde_json::from_value(v.clone())
        .map_err(|_| Error::new("invalid_request", "Expected typed full object ID"))?;
    o.validate().map_err(|e| Error::new(e.code, e.message))?;
    Ok(o.hex)
}
fn path(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => json!({"encoding":"utf8","value":s}),
        Err(_) => json!({"encoding":"hex","value":hex::encode(bytes)}),
    }
}
fn typed(hex: &str) -> Value {
    json!({"algorithm":"sha1","hex":hex})
}
fn validate_query_options(r: &Request) -> Result<()> {
    let allowed: &[&str] = match r.operation.as_str() {
        "inspect" => &["oid"],
        "tree" => &["oid", "limit", "recursive"],
        "diff" => &["base", "target", "limit"],
        "contains" => &["ancestor", "descendant"],
        "batch" | "query.batch" => &["items"],
        _ => return Err(Error::new("invalid_request", "Unknown exact query")),
    };
    if r.params
        .as_object()
        .is_none_or(|p| p.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(Error::new("invalid_request", "Unknown query parameter"));
    }
    if r.params.get("limit").is_some_and(|v| v.as_u64().is_none())
        || r.params
            .get("recursive")
            .is_some_and(|v| v.as_bool().is_none())
    {
        return Err(Error::new("invalid_request", "Invalid query option type"));
    }
    Ok(())
}
impl RepositoryManager {
    pub fn invalidate(&self, repo: &str) {
        let prefix = format!("{repo}:");
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let keys = c
            .entries
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some((value, _, _)) = c.entries.remove(&key) {
                c.bytes = c.bytes.saturating_sub(cache_entry_bytes(&key, &value));
            }
        }
        c.order.retain(|_, k| !k.starts_with(&prefix));
        c.completions.retain(|k, _| !k.starts_with(&prefix));
        drop(c);
        self.readers.invalidate(&self.pool_path(repo));
    }
    pub fn clear_cache(&self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Cache::default();
        self.readers.clear();
    }
    fn require_commit(&self, b: &Binding, oid: &str) -> Result<()> {
        let (kind, _) =
            self.readers
                .object(&self.runner, &self.pool_path(&b.repository_id), oid, 65536)?;
        if kind != "commit" {
            return Err(Error::new("invalid_request", "Expected commit object"));
        }
        Ok(())
    }
    pub(super) fn query(&self, r: &Request, b: &Binding) -> Result<Value> {
        validate_query_options(r)?;
        if matches!(r.operation.as_str(), "batch" | "query.batch") {
            let items = r.params["items"]
                .as_array()
                .ok_or_else(|| Error::new("invalid_request", "items array required"))?;
            if items.len() > 100 {
                return Err(Error::new("limit_exceeded", "Batch exceeds 100 items"));
            }
            let prepared = items
                .iter()
                .map(|item| {
                    (|| -> Result<(Request, String)> {
                        let object = item.as_object().ok_or_else(|| {
                            Error::new("invalid_request", "Batch item must be an object")
                        })?;
                        if object
                            .keys()
                            .any(|key| !matches!(key.as_str(), "operation" | "params"))
                        {
                            return Err(Error::new("invalid_request", "Unknown batch item field"));
                        }
                        let operation = object
                            .get("operation")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                Error::new("invalid_request", "Batch item operation required")
                            })?;
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
                    })()
                })
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
                        let mut details = serde_json::Map::new();
                        details.insert("code".into(), Value::String(error.code));
                        details.insert("message".into(), Value::String(error.message));
                        details.insert("details".into(), error.details);
                        let mut wrapper = serde_json::Map::new();
                        wrapper.insert("ok".into(), Value::Bool(false));
                        wrapper.insert("error".into(), Value::Object(details));
                        let wrapper = Value::Object(wrapper);
                        encoded_size.push(serde_json::to_vec(&wrapper)?.len())?;
                        results.push(wrapper);
                    }
                }
            }
            let mut wrapper = serde_json::Map::new();
            wrapper.insert("items".into(), Value::Array(results));
            let value = Value::Object(wrapper);
            return Ok(value);
        }
        let key = format!(
            "{}:{}:{}:{}",
            b.repository_id, b.binding_generation, r.operation, r.params
        );
        let cached = |count_miss: bool| {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.lookup(&key, count_miss)
        };
        if let Some((v, _)) = cached(true) {
            return Ok(v.as_ref().clone());
        }
        let flight = {
            let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            flights.retain(|_, v| v.strong_count() > 0);
            if let Some(f) = flights.get(&key).and_then(|f| f.upgrade()) {
                self.cache.lock().unwrap_or_else(|e| e.into_inner()).joins += 1;
                f
            } else {
                let f = Arc::new(Mutex::new(None));
                flights.insert(key.clone(), Arc::downgrade(&f));
                f
            }
        };
        let mut lease = crate::git::lock_with_deadline(&flight)?;
        if let Some(result) = &*lease {
            return result.clone();
        }
        if let Some((v, _)) = cached(false) {
            return Ok(v.as_ref().clone());
        }
        let value = match self.compute(r, b) {
            Ok(v) => v,
            Err(e) => {
                *lease = Some(Err(e.clone()));
                return Err(e);
            }
        };
        let serialized_len = serde_json::to_vec(&value)?.len();
        if serialized_len > 65536 {
            return Err(Error::new(
                "limit_exceeded",
                "Exact query response exceeds 64 KiB",
            ));
        }
        self.remember(key, value.clone(), serialized_len, None);
        *lease = Some(Ok(value.clone()));
        Ok(value)
    }
    pub(super) fn remember(
        &self,
        key: String,
        value: Value,
        serialized_len: usize,
        completion: Option<Instant>,
    ) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((old, stamp, _)) = cache.entries.remove(&key) {
            cache.bytes = cache.bytes.saturating_sub(cache_entry_bytes(&key, &old));
            cache.order.remove(&stamp);
            cache.completions.remove(&key);
        }
        let size = cache_entry_bytes(&key, &value);
        if size <= 16 * 1024 * 1024 {
            while cache.bytes + size > 16 * 1024 * 1024 || cache.entries.len() >= 4096 {
                if let Some((_, k)) = cache.order.pop_first() {
                    if let Some((v, _, _)) = cache.entries.remove(&k) {
                        cache.completions.remove(&k);
                        cache.evictions += 1;
                        cache.bytes = cache.bytes.saturating_sub(cache_entry_bytes(&k, &v));
                    }
                } else {
                    break;
                }
            }
            cache.bytes += size;
            cache.next += 1;
            let stamp = cache.next;
            cache.order.insert(stamp, key.clone());
            if let Some(completed) = completion {
                cache.completions.insert(key.clone(), completed);
            }
            cache
                .entries
                .insert(key, (Arc::new(value), stamp, serialized_len));
        }
    }
    fn compute(&self, r: &Request, b: &Binding) -> Result<Value> {
        let p = &r.params;
        let id = &b.repository_id;
        if !self.pool_path(id).is_dir() {
            return Err(Error::new(
                "object_unavailable",
                "Repository pool is absent; import retained objects",
            ));
        }
        match r.operation.as_str() {
            "contains" => {
                let ancestor = oid(p, "ancestor")?;
                let descendant = oid(p, "descendant")?;
                self.require_commit(b, &ancestor)?;
                self.require_commit(b, &descendant)?;
                let o = self.runner.run(
                    &self.pool_path(id),
                    &[
                        "merge-base".into(),
                        "--is-ancestor".into(),
                        ancestor,
                        descendant,
                    ],
                )?;
                match o.status.code() {
                    Some(0) => Ok(json!({"contains":true,"semantics":"strict_commit_ancestry_v1"})),
                    Some(1) => {
                        Ok(json!({"contains":false,"semantics":"strict_commit_ancestry_v1"}))
                    }
                    _ => Err(Error::new("git_failed", "Ancestry computation failed")),
                }
            }
            "inspect" => {
                let object = oid(p, "oid")?;
                let (kind, bytes) =
                    self.readers
                        .object(&self.runner, &self.pool_path(id), &object, 65536)?;
                if kind != "commit" {
                    return Err(Error::new("invalid_request", "Expected commit object"));
                }
                if bytes.len() > 65536 {
                    return Err(Error::new(
                        "limit_exceeded",
                        "Commit metadata exceeds 64 KiB",
                    ));
                }
                let split = bytes
                    .windows(2)
                    .position(|w| w == b"\n\n")
                    .unwrap_or(bytes.len());
                let headers = &bytes[..split];
                let mut tree = None;
                let mut parents = vec![];
                let mut author = None;
                let mut committer = None;
                for line in headers.split(|x| *x == b'\n') {
                    if line.starts_with(b"tree ") {
                        tree = Some(typed(std::str::from_utf8(&line[5..]).unwrap_or("")))
                    } else if line.starts_with(b"parent ") {
                        parents.push(typed(std::str::from_utf8(&line[7..]).unwrap_or("")))
                    } else if line.starts_with(b"author ") {
                        author = Some(path(&line[7..]))
                    } else if line.starts_with(b"committer ") {
                        committer = Some(path(&line[10..]))
                    }
                }
                Ok(
                    json!({"oid":typed(&object),"tree":tree,"parents":parents,"author":author,"committer":committer,"message":path(bytes.get(split+2..).unwrap_or_default())}),
                )
            }
            "tree" => {
                let object = oid(p, "oid")?;
                let limit = p["limit"].as_u64().unwrap_or(100);
                if limit == 0 || limit > 1000 {
                    return Err(Error::new("limit_exceeded", "Tree limit must be 1..1000"));
                }
                let mut args = vec!["ls-tree".into(), "-z".into()];
                if p["recursive"].as_bool().unwrap_or(false) {
                    args.push("-r".into())
                }
                args.push(object);
                let out = self.runner.run(&self.pool_path(id), &args)?;
                if !out.status.success() {
                    return Err(Error::new(
                        "object_unavailable",
                        "Tree object is unavailable",
                    ));
                }
                let mut entries = vec![];
                let mut truncated = false;
                for entry in out.stdout.split(|x| *x == 0).filter(|e| !e.is_empty()) {
                    if entries.len() >= limit as usize {
                        truncated = true;
                        break;
                    }
                    let tab = entry
                        .iter()
                        .position(|x| *x == b'\t')
                        .ok_or_else(|| Error::new("git_failed", "Malformed tree output"))?;
                    let header = std::str::from_utf8(&entry[..tab])
                        .map_err(|_| Error::new("git_failed", "Malformed tree header"))?
                        .split(' ')
                        .collect::<Vec<_>>();
                    if header.len() != 3 {
                        return Err(Error::new("git_failed", "Malformed tree header"));
                    }
                    entries.push(json!({"mode":header[0],"type":header[1],"oid":typed(header[2]),"path":path(&entry[tab+1..])}));
                }
                Ok(json!({"entries":entries,"truncated":truncated}))
            }
            "diff" => {
                let base = oid(p, "base")?;
                let target = oid(p, "target")?;
                self.require_commit(b, &base)?;
                self.require_commit(b, &target)?;
                let limit = p["limit"].as_u64().unwrap_or(100);
                if limit == 0 || limit > 1000 {
                    return Err(Error::new("limit_exceeded", "Diff limit must be 1..1000"));
                }
                let bytes = self.git(
                    id,
                    vec![
                        "diff".into(),
                        "--no-ext-diff".into(),
                        "--no-textconv".into(),
                        "--no-renames".into(),
                        "--name-status".into(),
                        "-z".into(),
                        base,
                        target,
                        "--".into(),
                    ],
                )?;
                let mut parts = bytes.split(|x| *x == 0).filter(|x| !x.is_empty());
                let mut entries = vec![];
                let mut truncated = false;
                while let Some(status) = parts.next() {
                    let name = parts
                        .next()
                        .ok_or_else(|| Error::new("git_failed", "Malformed diff output"))?;
                    if entries.len() >= limit as usize {
                        truncated = true;
                        break;
                    }
                    entries.push(json!({"status":std::str::from_utf8(status).unwrap_or("?"),"path":path(name)}));
                }
                Ok(json!({"entries":entries,"truncated":truncated}))
            }
            _ => Err(Error::new("unsupported_feature", "Unknown query")),
        }
    }
}

fn batch_success(value: Value) -> Value {
    let mut wrapper = serde_json::Map::new();
    wrapper.insert("ok".into(), Value::Bool(true));
    wrapper.insert("result".into(), value);
    Value::Object(wrapper)
}

struct BatchSize {
    bytes: usize,
    items: usize,
}
impl BatchSize {
    fn new() -> Self {
        Self {
            bytes: b"{\"items\":[]}".len(),
            items: 0,
        }
    }
    fn push_success(&mut self, result_bytes: usize) -> Result<()> {
        self.push(
            b"{\"ok\":true,\"result\":}"
                .len()
                .saturating_add(result_bytes),
        )
    }
    fn push(&mut self, item_bytes: usize) -> Result<()> {
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

#[cfg(test)]
mod batch_size_tests {
    use super::*;

    #[test]
    fn exact_lengths_include_unicode_escaping_nested_values_and_errors() {
        let values = [
            json!({"message":"雪🌲\n\t\"\\\u{0000}","nested":[null,true,-12,1.25]}),
            json!({"path":{"encoding":"hex","value":"ff090a"}}),
        ];
        let error = json!({"ok":false,"error":{"code":"invalid_request","message":"bad \"雪\"\n","details":{"path":"a\\b"}}});
        let mut size = BatchSize::new();
        let mut wrappers = Vec::new();
        for value in values {
            size.push_success(serde_json::to_vec(&value).unwrap().len())
                .unwrap();
            wrappers.push(batch_success(value));
        }
        size.push(serde_json::to_vec(&error).unwrap().len())
            .unwrap();
        wrappers.push(error);
        assert_eq!(
            size.bytes,
            serde_json::to_vec(&json!({"items":wrappers}))
                .unwrap()
                .len()
        );
    }

    #[test]
    fn exact_one_mib_is_allowed_and_next_item_is_rejected_without_advancing() {
        let mut size = BatchSize::new();
        let payload = Value::String(
            "x".repeat(1024 * 1024 - size.bytes - b"{\"ok\":true,\"result\":}".len() - 2),
        );
        size.push_success(serde_json::to_vec(&payload).unwrap().len())
            .unwrap();
        let actual = serde_json::to_vec(&json!({"items":[batch_success(payload)]})).unwrap();
        assert_eq!(size.bytes, actual.len());
        assert_eq!(actual.len(), 1024 * 1024);
        assert_eq!(size.push_success(4).unwrap_err().code, "limit_exceeded");
        assert_eq!(size.bytes, 1024 * 1024);
        assert_eq!(size.items, 1);
    }
}
