mod batch;
mod objects;
use super::*;
#[cfg(test)]
use batch::{BatchSize, batch_success};
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
        "diff.patch" => &["base", "target", "max_bytes"],
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
    let bad_limit = ["limit", "max_bytes"]
        .iter()
        .any(|key| r.params.get(key).is_some_and(|v| v.as_u64().is_none()));
    if bad_limit
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
            return self.batch_query(r, b);
        }
        if r.operation == "diff.patch" {
            // Patch bytes can far exceed the 64 KiB cached-answer bound; recompute each time.
            return self.diff_patch(r, b);
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
        let flight = self.query_flight(&key);
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
        self.remember(key, (value.clone(), serialized_len), None);
        *lease = Some(Ok(value.clone()));
        Ok(value)
    }
    fn query_flight(&self, key: &str) -> Arc<Mutex<Option<Result<Value>>>> {
        let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
        flights.retain(|_, v| v.strong_count() > 0);
        if let Some(f) = flights.get(key).and_then(|f| f.upgrade()) {
            self.cache.lock().unwrap_or_else(|e| e.into_inner()).joins += 1;
            f
        } else {
            let f = Arc::new(Mutex::new(None));
            flights.insert(key.to_owned(), Arc::downgrade(&f));
            f
        }
    }

    pub(super) fn remember(&self, key: String, entry: (Value, usize), completion: Option<Instant>) {
        let (value, serialized_len) = entry;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((old, stamp, _)) = cache.entries.remove(&key) {
            cache.bytes = cache.bytes.saturating_sub(cache_entry_bytes(&key, &old));
            cache.order.remove(&stamp);
            cache.completions.remove(&key);
        }
        let size = cache_entry_bytes(&key, &value);
        if size <= 16 * 1024 * 1024 {
            make_cache_room(&mut cache, size);
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
    /// Exact `git diff --binary --full-index --no-ext-diff --no-textconv <base> <target> --`
    /// bytes, base64-encoded with a SHA-256 over the raw bytes. Never cached and never truncated.
    fn diff_patch(&self, r: &Request, b: &Binding) -> Result<Value> {
        let p = &r.params;
        let id = &b.repository_id;
        if !self.pool_path(id).is_dir() {
            return Err(Error::new(
                "object_unavailable",
                "Repository pool is absent; import retained objects",
            ));
        }
        let base = oid(p, "base")?;
        let target = oid(p, "target")?;
        let max_bytes = p
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DIFF_PATCH_MAX_BYTES as u64);
        if max_bytes == 0 || max_bytes > DIFF_PATCH_MAX_BYTES as u64 {
            return Err(Error::new(
                "limit_exceeded",
                format!("Patch max_bytes must be 1..{DIFF_PATCH_MAX_BYTES}"),
            )
            .details(json!({"max_bytes":DIFF_PATCH_MAX_BYTES})));
        }
        self.require_commit(b, &base)?;
        self.require_commit(b, &target)?;
        let too_large = || {
            Error::new(
                "limit_exceeded",
                "Patch exceeds the byte limit; no partial patch is returned",
            )
            .details(json!({"max_bytes":max_bytes}))
        };
        let out = match self.runner.run(
            &self.pool_path(id),
            &[
                "diff".into(),
                "--binary".into(),
                "--full-index".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                base.clone(),
                target.clone(),
                "--".into(),
            ],
        ) {
            Ok(out) => out,
            Err(e) if e.code == "output_limit" => return Err(too_large()),
            Err(e) => return Err(e),
        };
        if !out.status.success() {
            return Err(Error::new("git_failed", "Git command failed")
                .details(json!({"exit_code":out.status.code()})));
        }
        let patch = out.stdout;
        if patch.len() as u64 > max_bytes {
            return Err(too_large());
        }
        use sha2::{Digest, Sha256};
        Ok(json!({
            "format":"git_diff_binary_full_index_v1",
            "base":typed(&base),
            "target":typed(&target),
            "size":patch.len(),
            "sha256":hex::encode(Sha256::digest(&patch)),
            "patch":{"encoding":"base64","value":base64(&patch)},
        }))
    }
}

/// Largest raw patch a `diff.patch` answer may carry. Its base64 form (about 5.6 MB) stays inside
/// the 8 MiB response frame.
pub const DIFF_PATCH_MAX_BYTES: usize = 4 * 1024 * 1024;

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod batch_size_tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (raw, encoded) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (&[0xff, 0xfe, 0xfd], "//79"),
        ] {
            assert_eq!(base64(raw), encoded);
        }
    }

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

fn make_cache_room(cache: &mut Cache, size: usize) {
    while cache.bytes + size > 16 * 1024 * 1024 || cache.entries.len() >= 4096 {
        let Some((_, k)) = cache.order.pop_first() else {
            break;
        };
        if let Some((v, _, _)) = cache.entries.remove(&k) {
            cache.completions.remove(&k);
            cache.evictions += 1;
            cache.bytes = cache.bytes.saturating_sub(cache_entry_bytes(&k, &v));
        }
    }
}
