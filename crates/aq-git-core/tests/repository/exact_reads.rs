use super::*;
#[test]
fn exact_reads_observations_and_warm_batches() {
    let t = tempfile::tempdir().unwrap();
    let remote = t.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("a"), "one").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "one"]);
    let first = git(&remote, &["rev-parse", "HEAD"]);
    std::fs::write(remote.join("a"), "two").unwrap();
    git(&remote, &["commit", "-am", "two"]);
    let second = git(&remote, &["rev-parse", "HEAD"]);
    let root = Arc::new(StateRoot::open(t.path().join("state")).unwrap());
    let runner = GitRunner::new("/usr/bin/git".into());
    let m = RepositoryManager::new(root, runner.clone());
    let id = uuid::Uuid::new_v4().to_string();
    m.handle(&request(&id,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/missing"]}))).unwrap();
    assert!(runner.metrics().spawns >= 1);
    assert_observations(&m, &id, [&first, &second]);
    assert_warm_query_batch(&m, &id, [&first, &second]);
    let second = oid(&second);
    assert_concurrent_inspections(&m, &id, &second);
    assert_strict_batch_items(&m, &id, &second);
    assert_duplicate_cache_hits(&m, &id, &second);
    assert_unavailable_reads(&m, &id, &second);
}

fn assert_observations(m: &RepositoryManager, id: &str, [first, second]: [&str; 2]) {
    let sync = request(id, "repo.sync", json!({}));
    let obs = m.handle(&sync).unwrap();
    assert_eq!(obs["commit_graph_available"], true);
    let before = m.runner.metrics().spawns;
    assert_eq!(m.handle(&sync).unwrap(), obs);
    assert_eq!(m.runner.metrics().spawns, before);
    m.git(id, vec!["commit-graph".into(), "verify".into()])
        .unwrap();
    let containment = request(
        id,
        "contains",
        json!({"ancestor":oid(first),"descendant":oid(second)}),
    );
    let indexed = m.handle(&containment).unwrap();
    let graph_dir = m.pool_path(id).join("objects/info/commit-graphs");
    std::fs::remove_dir_all(&graph_dir).unwrap();
    m.clear_cache();
    assert_eq!(m.handle(&containment).unwrap(), indexed);
    assert!(
        !graph_dir.exists(),
        "exact reads must not rebuild a derived index"
    );
    m.handle(&request(id, "repo.sync", json!({}))).unwrap();
    m.git(id, vec!["commit-graph".into(), "verify".into()])
        .unwrap();
    assert!(graph_dir.exists());
    m.clear_cache();
    assert_eq!(m.handle(&containment).unwrap(), indexed);
    assert_eq!(obs["refs"]["refs/heads/main"], second);
    assert!(obs["refs"].get("refs/heads/missing").is_none());
    let before = m.runner.metrics().spawns;
    m.handle(&request(
        id,
        "repo.observation",
        json!({"refs":["refs/heads/missing"]}),
    ))
    .unwrap();
    assert_eq!(before, m.runner.metrics().spawns);
}

fn assert_warm_query_batch(m: &RepositoryManager, id: &str, [first, second]: [&str; 2]) {
    let mut q = request(
        id,
        "query.batch",
        json!({"items":[{"operation":"inspect","params":{"oid":oid(first)}},{"operation":"contains","params":{"ancestor":oid(first),"descendant":oid(second)}},{"operation":"tree","params":{"oid":oid(second)}},{"operation":"diff","params":{"base":oid(first),"target":oid(second)}}]}),
    );
    q.expected_binding_generation = None;
    let result = m.handle(&q).unwrap();
    assert_eq!(result["items"][1]["result"]["contains"], true);
    assert_eq!(
        result["items"][2]["result"]["entries"][0]["path"]["value"],
        "a"
    );
    let before = m.runner.metrics().spawns;
    let exchanges = m.query_metrics()["object_exchanges"].clone();
    assert_eq!(result, m.handle(&q).unwrap());
    assert_eq!(before, m.runner.metrics().spawns);
    assert_eq!(exchanges, m.query_metrics()["object_exchanges"]);
}

fn assert_concurrent_inspections(m: &RepositoryManager, id: &str, second: &Value) {
    m.clear_cache();
    let shared = Arc::new(m.clone());
    let barrier = Arc::new(std::sync::Barrier::new(32));
    let exchanges = m.query_metrics()["object_exchanges"].as_u64().unwrap();
    let handles = (0..32)
        .map(|_| {
            let manager = shared.clone();
            let barrier = barrier.clone();
            let query = request(id, "inspect", json!({"oid":second}));
            std::thread::spawn(move || {
                barrier.wait();
                manager.handle(&query).unwrap()
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        assert_eq!(&handle.join().unwrap()["oid"], second);
    }
    assert_eq!(
        m.query_metrics()["object_exchanges"].as_u64().unwrap() - exchanges,
        1
    );
}

fn assert_strict_batch_items(m: &RepositoryManager, id: &str, second: &Value) {
    let before = m.runner.metrics().spawns;
    let before_exchanges = m.query_metrics()["object_exchanges"].clone();
    let strict_batch=m.handle(&request(id,"query.batch",json!({"items":[{"operation":"inspect","params":{"oid":second},"unexpected":true},{"operation":"inspect","params":{"oid":second}}]}))).unwrap();
    assert_eq!(strict_batch["items"][0]["ok"], false);
    assert_eq!(strict_batch["items"][0]["error"]["code"], "invalid_request");
    assert_eq!(&strict_batch["items"][1]["result"]["oid"], second);
    assert_eq!(m.runner.metrics().spawns, before);
    assert_eq!(m.query_metrics()["object_exchanges"], before_exchanges);
}

fn assert_duplicate_cache_hits(m: &RepositoryManager, id: &str, second: &Value) {
    m.clear_cache();
    let duplicates = request(
        id,
        "query.batch",
        json!({"items":vec![json!({"operation":"inspect","params":{"oid":second}});100]}),
    );
    let initial_exchanges = m.query_metrics()["object_exchanges"].as_u64().unwrap();
    let duplicated = m.handle(&duplicates).unwrap();
    assert_eq!(duplicated["items"].as_array().unwrap().len(), 100);
    assert_eq!(
        m.query_metrics()["object_exchanges"].as_u64().unwrap() - initial_exchanges,
        1
    );
    assert_eq!(m.query_metrics()["cache_misses"], 1);
    assert_eq!(m.query_metrics()["cache_hits"], 99);
    let before = m.runner.metrics().spawns;
    let exchanges = m.query_metrics()["object_exchanges"].clone();
    assert_eq!(m.handle(&duplicates).unwrap(), duplicated);
    assert_eq!(m.runner.metrics().spawns, before);
    assert_eq!(m.query_metrics()["object_exchanges"], exchanges);
    assert_eq!(m.query_metrics()["cache_hits"], 199);
}

fn assert_unavailable_reads(m: &RepositoryManager, id: &str, second: &Value) {
    let missing = request(
        id,
        "contains",
        json!({"ancestor":oid(&"a".repeat(40)),"descendant":second}),
    );
    assert_eq!(m.handle(&missing).unwrap_err().code, "object_unavailable");
    assert_eq!(m.handle(&missing).unwrap_err().code, "object_unavailable");
    let stale = request(id, "repo.observation", json!({"max_age_ms":0}));
    assert_eq!(m.handle(&stale).unwrap_err().code, "observation_stale");
}
