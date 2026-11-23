use super::*;

impl RepositoryManager {
    pub(super) fn compute(&self, r: &Request, b: &Binding) -> Result<Value> {
        let p = &r.params;
        let id = &b.repository_id;
        if !self.pool_path(id).is_dir() {
            return Err(Error::new(
                "object_unavailable",
                "Repository pool is absent; import retained objects",
            ));
        }
        match r.operation.as_str() {
            "contains" => self.query_contains(b, p),
            "inspect" => self.query_inspect(b, p),
            "tree" => self.query_tree(b, p),
            "diff" => self.query_diff(b, p),
            _ => Err(Error::new("unsupported_feature", "Unknown query")),
        }
    }
    fn query_contains(&self, b: &Binding, p: &Value) -> Result<Value> {
        let id = &b.repository_id;
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
            Some(1) => Ok(json!({"contains":false,"semantics":"strict_commit_ancestry_v1"})),
            _ => Err(Error::new("git_failed", "Ancestry computation failed")),
        }
    }
    fn query_inspect(&self, b: &Binding, p: &Value) -> Result<Value> {
        let id = &b.repository_id;
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
    fn query_tree(&self, b: &Binding, p: &Value) -> Result<Value> {
        let id = &b.repository_id;
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
            entries.push(parse_tree_entry(entry)?);
        }
        Ok(json!({"entries":entries,"truncated":truncated}))
    }
    fn query_diff(&self, b: &Binding, p: &Value) -> Result<Value> {
        let id = &b.repository_id;
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
            entries.push(
                json!({"status":std::str::from_utf8(status).unwrap_or("?"),"path":path(name)}),
            );
        }
        Ok(json!({"entries":entries,"truncated":truncated}))
    }
}

fn parse_tree_entry(entry: &[u8]) -> Result<Value> {
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
    Ok(
        json!({"mode":header[0],"type":header[1],"oid":typed(header[2]),"path":path(&entry[tab+1..])}),
    )
}
