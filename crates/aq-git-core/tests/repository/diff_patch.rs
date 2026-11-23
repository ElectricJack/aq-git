use super::*;
use sha2::{Digest, Sha256};
fn unbase64(text: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for byte in text.bytes().take_while(|b| *b != b'=') {
        acc = (acc << 6) | A.iter().position(|a| *a == byte).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}
fn tree_state(path: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut out = vec![];
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((
                entry.path().display().to_string(),
                meta.len(),
                meta.modified().unwrap(),
            ));
        }
    }
    out.sort();
    out
}
#[test]
fn diff_patch_returns_exact_verified_bytes_with_explicit_refusals() {
    let t = tempfile::tempdir().unwrap();
    let remote = t.path().join("remote");
    let [first, second, tree] = create_patch_history(&remote);
    let root = Arc::new(StateRoot::open(t.path().join("state")).unwrap());
    let m = RepositoryManager::new(root, GitRunner::new("/usr/bin/git".into()));
    let id = uuid::Uuid::new_v4().to_string();
    m.handle(&request(&id,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}))).unwrap();
    m.handle(&request(
        &id,
        "repo.sync",
        json!({"min_started_at_ms":chrono::Utc::now().timestamp_millis()}),
    ))
    .unwrap();

    let commits = [first.as_str(), second.as_str()];
    let expected = reference_patch(&remote, commits);
    let answer = assert_exact_patch(&m, &id, commits, &expected);
    assert_empty_patch(&m, &id, &first);
    assert_patch_limits(&m, &id, commits, (&expected, &answer));
    assert_invalid_patch_inputs(&m, &id, commits, &tree);
    assert_patch_operation_scope(&m, &id, commits);
}
fn create_patch_history(remote: &Path) -> [String; 3] {
    use std::os::unix::ffi::OsStringExt;
    std::fs::create_dir(remote).unwrap();
    git(remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("text"), "one\ntwo\n").unwrap();
    std::fs::write(remote.join("bin"), [0u8, 1, 2, 3, 255]).unwrap();
    std::fs::write(remote.join("gone"), "bye\n").unwrap();
    git(remote, &["add", "."]);
    git(remote, &["commit", "-m", "one"]);
    let first = git(remote, &["rev-parse", "HEAD"]);
    std::fs::write(remote.join("text"), "one\n2\n").unwrap();
    std::fs::write(remote.join("bin"), [0u8, 9, 2, 3, 255, 254]).unwrap();
    std::fs::remove_file(remote.join("gone")).unwrap();
    std::fs::write(
        remote.join(std::ffi::OsString::from_vec(vec![b'a', 255, b' ', b'"'])),
        "bytes\n",
    )
    .unwrap();
    std::fs::write(remote.join("sp\u{e9}ce name"), "unicode\n").unwrap();
    std::fs::write(remote.join(".gitattributes"), "* diff=external\n").unwrap();
    git(remote, &["add", "-A"]);
    git(remote, &["commit", "-m", "two"]);
    let second = git(remote, &["rev-parse", "HEAD"]);
    let tree = git(remote, &["rev-parse", "HEAD^{tree}"]);
    [first, second, tree]
}
fn reference_patch(remote: &Path, [first, second]: [&str; 2]) -> Vec<u8> {
    let expected = Command::new("/usr/bin/git")
        .current_dir(remote)
        .args([
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            first,
            second,
            "--",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap()
        .stdout;
    assert!(expected.windows(14).any(|w| w == b"GIT binary pat"));
    expected
}
fn assert_exact_patch(
    m: &RepositoryManager,
    id: &str,
    [first, second]: [&str; 2],
    expected: &[u8],
) -> Value {
    let before = tree_state(&m.pool_path(id));
    let q = request(
        id,
        "diff.patch",
        json!({"base":oid(first),"target":oid(second)}),
    );
    let answer = m.handle(&q).unwrap();
    assert_eq!(
        tree_state(&m.pool_path(id)),
        before,
        "diff.patch must not write"
    );
    assert_eq!(answer["format"], "git_diff_binary_full_index_v1");
    assert_eq!(answer["base"], oid(first));
    assert_eq!(answer["target"], oid(second));
    assert_eq!(answer["patch"]["encoding"], "base64");
    let patch = unbase64(answer["patch"]["value"].as_str().unwrap());
    assert_eq!(patch, expected);
    assert_eq!(answer["size"], expected.len());
    assert_eq!(
        answer["sha256"],
        hex::encode(Sha256::digest(expected)).as_str()
    );
    // Not cached: a repeat is computed again and identical.
    assert_eq!(m.handle(&q).unwrap(), answer);
    answer
}
fn assert_empty_patch(m: &RepositoryManager, id: &str, first: &str) {
    // Identical commits give a verifiable empty patch.
    let same = m
        .handle(&request(
            id,
            "diff.patch",
            json!({"base":oid(first),"target":oid(first)}),
        ))
        .unwrap();
    assert_eq!(same["size"], 0);
    assert_eq!(same["patch"]["value"], "");
    assert_eq!(same["sha256"], hex::encode(Sha256::digest(b"")).as_str());
}

fn assert_patch_limits(
    m: &RepositoryManager,
    id: &str,
    [first, second]: [&str; 2],
    (expected, answer): (&[u8], &Value),
) {
    // Explicit size limit: no partial patch, details name the limit.
    let small = expected.len() as u64 - 1;
    let err = m
        .handle(&request(
            id,
            "diff.patch",
            json!({"base":oid(first),"target":oid(second),"max_bytes":small}),
        ))
        .unwrap_err();
    assert_eq!(err.code, "limit_exceeded");
    assert_eq!(err.details["max_bytes"], small);
    let exact = m
        .handle(&request(
            id,
            "diff.patch",
            json!({"base":oid(first),"target":oid(second),"max_bytes":expected.len()}),
        ))
        .unwrap();
    assert_eq!(exact["sha256"], answer["sha256"]);
    for bad in [json!(0), json!(4194305), json!("x")] {
        let err = m
            .handle(&request(
                id,
                "diff.patch",
                json!({"base":oid(first),"target":oid(second),"max_bytes":bad}),
            ))
            .unwrap_err();
        assert!(["limit_exceeded", "invalid_request"].contains(&err.code.as_str()));
    }
}

fn assert_invalid_patch_inputs(
    m: &RepositoryManager,
    id: &str,
    [first, second]: [&str; 2],
    tree: &str,
) {
    // Missing, wrong-type and malformed inputs.
    let missing = m
        .handle(&request(
            id,
            "diff.patch",
            json!({"base":oid(first),"target":oid(&"c".repeat(40))}),
        ))
        .unwrap_err();
    assert_eq!(missing.code, "object_unavailable");
    let wrong = m
        .handle(&request(
            id,
            "diff.patch",
            json!({"base":oid(first),"target":oid(tree)}),
        ))
        .unwrap_err();
    assert_eq!(wrong.code, "invalid_request");
    for params in [
        json!({"base":oid(first)}),
        json!({"base":first,"target":second}),
        json!({"base":oid(first),"target":oid(second),"limit":1}),
    ] {
        assert_eq!(
            m.handle(&request(id, "diff.patch", params))
                .unwrap_err()
                .code,
            "invalid_request"
        );
    }
}

fn assert_patch_operation_scope(m: &RepositoryManager, id: &str, [first, second]: [&str; 2]) {
    // Patches are not batchable, and the name-status diff keeps its shape.
    let batch = m
        .handle(&request(
            id,
            "query.batch",
            json!({"items":[{"operation":"diff.patch","params":{"base":oid(first),"target":oid(second)}}]}),
        ))
        .unwrap();
    assert_eq!(batch["items"][0]["ok"], false);
    let names = m
        .handle(&request(
            id,
            "diff",
            json!({"base":oid(first),"target":oid(second)}),
        ))
        .unwrap();
    assert!(names["entries"].is_array() && names["truncated"] == false);
}
