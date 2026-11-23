use super::*;
type RefMap = BTreeMap<String, String>;

struct SyncFlight {
    key: String,
    minimum: i64,
}

impl RepositoryManager {
    fn scope(&self, r: &Request, b: &Binding) -> Result<Vec<String>> {
        let mut refs = match r.params.get("refs") {
            Some(v) => serde_json::from_value::<Vec<String>>(v.clone())?,
            None => b.allowed_refs.clone(),
        };
        if !(1..=1000).contains(&refs.len()) || refs.iter().any(|s| !b.allowed_refs.contains(s)) {
            return Err(Error::new(
                "invalid_request",
                "Requested refs must be a bounded configured scope",
            ));
        }
        refs.sort();
        refs.dedup();
        Ok(refs)
    }
    pub(crate) fn sync_shared(&self, r: &Request, b: &Binding) -> Result<Value> {
        let refs = self.scope(r, b)?;
        let key = format!(
            "{}:{}:{}",
            b.repository_id,
            b.binding_generation,
            serde_json::to_string(&refs)?
        );
        let minimum = match r.params.get("min_started_at_ms") {
            Some(v) => v.as_i64().ok_or_else(|| {
                Error::new("invalid_request", "min_started_at_ms must be integer")
            })?,
            None => 0,
        };
        if minimum > chrono::Utc::now().timestamp_millis() {
            return Err(Error::new(
                "invalid_request",
                "Minimum sync start cannot be in the future",
            ));
        }
        let first = self.sync_flight(
            r,
            b,
            SyncFlight {
                key: key.clone(),
                minimum,
            },
        )?;
        if first["started_at_ms"].as_i64().unwrap_or(0) >= minimum {
            return Ok(first);
        }
        self.sync_flight(
            r,
            b,
            SyncFlight {
                key: format!("{key}:after:{minimum}"),
                minimum,
            },
        )
    }
    fn sync_flight(&self, r: &Request, b: &Binding, flight: SyncFlight) -> Result<Value> {
        let SyncFlight { key, minimum } = flight;
        let admitted = admitted_at();
        let flight = {
            let mut flights = self.sync_flights.lock().unwrap_or_else(|e| e.into_inner());
            flights.retain(|_, v| v.strong_count() > 0);
            if let Some(f) = flights.get(&key).and_then(|f| f.upgrade()) {
                f
            } else {
                let f = Arc::new(Mutex::new(None));
                flights.insert(key.clone(), Arc::downgrade(&f));
                f
            }
        };
        let mut result = crate::git::lock_with_deadline(&flight)?;
        if let Some(value) = &*result {
            return value.clone();
        }
        let completed_key = format!("{key}:completed-sync");
        let completed = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache
                .completions
                .get(&completed_key)
                .is_some_and(|completed| *completed >= admitted)
            {
                cache
                    .entries
                    .get(&completed_key)
                    .filter(|(value, _, _)| {
                        value["started_at_ms"]
                            .as_i64()
                            .is_some_and(|started| started >= minimum)
                    })
                    .map(|(value, _, _)| value.clone())
            } else {
                None
            }
        };
        if let Some(value) = completed {
            return Ok(value.as_ref().clone());
        }
        let _permit = self.heavy_budget.acquire()?;
        let value = self.sync(r, b);
        self.root.write_json(
            format!("repositories/{}/refresh.json", b.repository_id),
            &json!({"at_ms":chrono::Utc::now().timestamp_millis(),"error":value.as_ref().err()}),
        )?;
        if let Ok(value) = &value {
            self.remember(
                completed_key,
                value.clone(),
                serde_json::to_vec(&value)?.len(),
                Some(Instant::now()),
            );
        }
        *result = Some(value.clone());
        value
    }
    fn sync(&self, r: &Request, b: &Binding) -> Result<Value> {
        if b.mode != "central" {
            return Err(Error::new(
                "unsupported_feature",
                "Worker synchronization uses artifact import",
            ));
        }
        let refs = self.scope(r, b)?;
        let url = b
            .remote_url
            .as_ref()
            .ok_or_else(|| Error::new("invalid_request", "No remote configured"))?;
        let sid = r
            .operation_id
            .clone()
            .ok_or_else(|| Error::new("invalid_request", "operation_id required"))?;
        let sync = Sync {
            manager: self,
            binding: b,
            refs,
            url,
            intent_path: format!("repositories/{}/sync/{sid}.json", b.repository_id),
            id: sid,
        };
        sync.run()
    }
    pub(super) fn observation(&self, r: &Request, b: &Binding) -> Result<Value> {
        let refs = self.scope(r, b)?;
        let found = self.latest_observation(b, &refs)?;
        let mut o = found.ok_or_else(|| {
            Error::new(
                "observation_unavailable",
                "Requested scope has no completed observation",
            )
        })?;
        if r.params
            .get("max_age_ms")
            .is_some_and(|v| v.as_u64().is_none())
        {
            return Err(Error::new(
                "invalid_request",
                "max_age_ms must be nonnegative integer",
            ));
        }
        if let Some(age) = r.params["max_age_ms"].as_i64()
            && chrono::Utc::now().timestamp_millis() - o["completed_at_ms"].as_i64().unwrap_or(0)
                > age
        {
            return Err(Error::new(
                "observation_stale",
                "Observation exceeds requested age",
            ));
        }
        if let Some(refresh) = self
            .root
            .read_json::<Value>(format!("repositories/{}/refresh.json", b.repository_id))?
        {
            o["last_refresh_error"] = refresh["error"].clone();
        }
        Ok(o)
    }
    fn latest_observation(&self, b: &Binding, refs: &[String]) -> Result<Option<Value>> {
        let covers = |o: &Value| {
            o["binding_generation"] == json!(b.binding_generation)
                && refs.iter().all(|x| {
                    o["covered_refs"]
                        .as_array()
                        .is_some_and(|a| a.contains(&json!(x)))
                })
        };
        let mut found = self
            .root
            .read_json::<Value>(format!("repositories/{}/observation.json", b.repository_id))?
            .filter(|o| covers(o));
        if found.is_some() {
            return Ok(found);
        }
        {
            for path in
                self.json_files(&format!("repositories/{}/observations", b.repository_id))?
            {
                let Some(candidate) = self.root.read_json::<Value>(path)?.filter(&covers) else {
                    continue;
                };
                if found.as_ref().is_none_or(|old| {
                    candidate["completed_at_ms"].as_i64() > old["completed_at_ms"].as_i64()
                }) {
                    found = Some(candidate);
                }
            }
        }
        Ok(found)
    }
}

struct Sync<'a> {
    manager: &'a RepositoryManager,
    binding: &'a Binding,
    refs: Vec<String>,
    url: &'a str,
    id: String,
    intent_path: String,
}

impl Sync<'_> {
    fn run(&self) -> Result<Value> {
        let prior: Option<Value> = self.manager.root.read_json(&self.intent_path)?;
        if let Some(result) = prior.as_ref().and_then(|i| i.get("result")) {
            return Ok(result.clone());
        }
        let started = prior
            .as_ref()
            .and_then(|i| i["started_at_ms"].as_i64())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let refs = &self.refs;
        let b = self.binding;
        let sid = &self.id;
        let map = if let Some(prior) = prior {
            serde_json::from_value(prior["refs"].clone())?
        } else {
            let map = self.advertise()?;
            self.manager.root.write_json(&self.intent_path, &json!({"format_version":1,"binding_generation":b.binding_generation,"observation_id":sid,"started_at_ms":started,"covered_refs":refs,"refs":map}))?;
            map
        };
        let mut fetch_duration_ms = 0;
        if !map.is_empty() {
            let known = self.known_refs(&map)?;
            fetch_duration_ms = self.fetch(&map)?;
            self.install(&map, &known, started)?;
        }
        self.write_observation(&map, started, fetch_duration_ms)
    }

    fn advertise(&self) -> Result<RefMap> {
        let mut map = RefMap::new();
        let refs = &self.refs;
        // A successful bounded advertisement establishes exact coverage, including absent configured refs.
        let mut args = vec![
            "ls-remote".into(),
            "--refs".into(),
            "--".into(),
            self.url.to_owned(),
        ];
        args.extend(refs.iter().cloned());
        let advertisement = self.manager.remote_git(self.binding, args)?;
        for line in advertisement
            .split(|c| *c == b'\n')
            .filter(|line| !line.is_empty())
        {
            let (name, hex) = parse_advertisement(line)?;
            if refs.contains(&name) && map.insert(name, hex.clone()).is_some_and(|old| old != hex) {
                return Err(Error::new("git_failed", "Inconsistent ref advertisement"));
            }
        }
        Ok(map)
    }

    fn known_refs(&self, map: &RefMap) -> Result<RefMap> {
        let sid = &self.id;
        let mut known = RefMap::new();
        let existing = self.manager.git(
            &self.binding.repository_id,
            vec![
                "for-each-ref".into(),
                "--format=%(refname) %(objectname)".into(),
                format!("refs/aq-git/observations/{sid}/"),
                format!("refs/aq-git/staging/{sid}/"),
            ],
        )?;
        for line in existing.split(|b| *b == b'\n') {
            if let Some(space) = line.iter().position(|b| *b == b' ') {
                known.insert(
                    String::from_utf8_lossy(&line[..space]).into_owned(),
                    String::from_utf8_lossy(&line[space + 1..]).into_owned(),
                );
            }
        }
        let mut expected = BTreeMap::new();
        for (i, (_, oid)) in map.iter().enumerate() {
            expected.insert(format!("refs/aq-git/staging/{sid}/{i}"), oid.clone());
            expected.insert(format!("refs/aq-git/observations/{sid}/{i}"), oid.clone());
        }
        if known
            .iter()
            .any(|(name, oid)| expected.get(name) != Some(oid))
        {
            return Err(Error::new(
                "recovery_blocked",
                "Sync refs changed unexpectedly",
            ));
        }
        Ok(known)
    }

    fn fetch(&self, map: &RefMap) -> Result<u64> {
        let sid = &self.id;
        let mut args = vec![
            "-c".into(),
            "fetch.writeCommitGraph=true".into(),
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            "--".into(),
            self.url.to_owned(),
        ];
        for (i, (_, oid)) in map.iter().enumerate() {
            args.push(format!("{oid}:refs/aq-git/staging/{sid}/{i}"));
        }
        let fetch_started = Instant::now();
        self.manager.remote_git(self.binding, args)?;
        Ok(fetch_started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }

    fn install(&self, map: &RefMap, known: &RefMap, started: i64) -> Result<()> {
        let b = self.binding;
        let mut transaction = self.pin_commands(map, known)?;
        let main = self.manager.plan_main_advance(b, map, started)?;
        if let Some(main) = &main {
            transaction.push_str(&main.instruction());
        }
        transaction.push_str("prepare\ncommit\n");
        let out = self.manager.runner.run_with(
            &self.manager.pool_path(&b.repository_id),
            &["update-ref".into(), "--stdin".into()],
            transaction.as_bytes(),
            &[],
            self.manager.runner.limits.timeout,
        )?;
        if !out.status.success() {
            return Err(Error::new(
                "recovery_blocked",
                "Cannot install observation ref transaction",
            ));
        }
        if let Some(main) = main {
            main.finish(&self.manager.root)?;
        }
        Ok(())
    }
    fn pin_commands(&self, map: &RefMap, known: &RefMap) -> Result<String> {
        let sid = &self.id;
        let mut transaction = String::from("start\n");
        for (i, (_, oid)) in map.iter().enumerate() {
            let target = format!("refs/aq-git/observations/{sid}/{i}");
            match known.get(&target) {
                Some(old) if old != oid => {
                    return Err(Error::new(
                        "recovery_blocked",
                        "Observation pin changed unexpectedly",
                    ));
                }
                Some(_) => {}
                None => transaction.push_str(&format!("create {target} {oid}\n")),
            }
            transaction.push_str(&format!("delete refs/aq-git/staging/{sid}/{i} {oid}\n"));
        }
        Ok(transaction)
    }
    fn write_observation(
        &self,
        map: &RefMap,
        started: i64,
        fetch_duration_ms: u64,
    ) -> Result<Value> {
        let b = self.binding;
        let sid = &self.id;
        let refs = &self.refs;
        let info = self
            .manager
            .pool_path(&b.repository_id)
            .join("objects/info");
        let commit_graph_available = info.join("commit-graph").is_file()
            || info.join("commit-graphs/commit-graph-chain").is_file();
        let observation = json!({"format_version":1,"observation_id":sid,"binding_generation":b.binding_generation,"covered_refs":refs,"refs":map,"started_at_ms":started,"completed_at_ms":chrono::Utc::now().timestamp_millis(),"availability":"available","last_refresh_error":null,"fetch_duration_ms":fetch_duration_ms,"commit_graph_available":commit_graph_available});
        self.manager.root.write_json(
            format!("repositories/{}/observations/{sid}.json", b.repository_id),
            &observation,
        )?;
        self.manager.root.write_json(
            format!("repositories/{}/observation.json", b.repository_id),
            &observation,
        )?;
        self.manager.root.write_json(&self.intent_path,&json!({"format_version":1,"binding_generation":b.binding_generation,"observation_id":sid,"started_at_ms":started,"covered_refs":refs,"refs":map,"result":observation}))?;
        Ok(observation)
    }
}

fn parse_advertisement(line: &[u8]) -> Result<(String, String)> {
    let tab = line
        .iter()
        .position(|c| *c == b'\t')
        .ok_or_else(|| Error::new("git_failed", "Malformed ref advertisement"))?;
    let name = String::from_utf8(line[tab + 1..].to_vec())
        .map_err(|_| Error::new("git_failed", "Invalid ref advertisement"))?;
    aq_git_protocol::validate_ref(&name)
        .map_err(|_| Error::new("git_failed", "Invalid advertised ref"))?;
    let hex = String::from_utf8(line[..tab].to_vec())
        .map_err(|_| Error::new("git_failed", "Malformed advertised OID"))?;
    aq_git_protocol::ObjectId {
        algorithm: "sha1".into(),
        hex: hex.clone(),
    }
    .validate()
    .map_err(|_| Error::new("git_failed", "Malformed advertised OID"))?;
    Ok((name, hex))
}
