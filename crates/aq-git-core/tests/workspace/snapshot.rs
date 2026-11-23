use super::*;

#[test]
fn workspace_snapshot_includes_untracked_and_digest_is_stable() {
    let f = Fixture::new();
    let w = f.create_workspace();
    let path = Path::new(w["path"].as_str().unwrap());
    std::fs::write(path.join("file.txt"), "dirty tracked edit\n").unwrap();
    std::fs::create_dir(path.join("new")).unwrap();
    std::fs::write(path.join("new/untracked.txt"), "untracked work\n").unwrap();
    std::fs::write(path.join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(path.join("ignored"), "build output").unwrap();
    let status_before = git(path, &["status", "--porcelain", "--untracked-files=all"]);
    let params = json!({"workspace_id":w["workspace_id"],"expected_generation":1,"expected_head":oid(&f.base),"quiescent":true});
    let first_request = request(&f.repository, "workspace.snapshot", params.clone());
    let first = success(&f.service, first_request.clone());
    assert_eq!(
        first,
        success(&f.service, first_request),
        "replay is idempotent"
    );
    let second = success(
        &f.service,
        request(&f.repository, "workspace.snapshot", params),
    );
    assert_ne!(first["artifact_handle"], second["artifact_handle"]);
    let digest = first["bundle_sha256"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert_eq!(digest, second["bundle_sha256"].as_str().unwrap());
    for field in ["snapshot", "tree", "bundle_size"] {
        assert_eq!(first[field], second[field], "{field}");
    }
    let state = f.temp.path().join("state");
    let bundle = |result: &serde_json::Value| state.join(result["bundle"].as_str().unwrap());
    let bytes = std::fs::read(bundle(&first)).unwrap();
    assert_eq!(bytes, std::fs::read(bundle(&second)).unwrap());
    use sha2::Digest;
    assert_eq!(hex::encode(sha2::Sha256::digest(&bytes)), digest);
    assert_restored_snapshot(&f, &first, &bundle(&first));
    // The workspace HEAD, index and files are unchanged.
    assert_eq!(git(path, &["rev-parse", "HEAD"]), f.base);
    assert_eq!(
        git(path, &["status", "--porcelain", "--untracked-files=all"]),
        status_before
    );
    assert!(
        git(path, &["for-each-ref"])
            .lines()
            .all(|line| !line.contains("aq-git/snapshot"))
    );
}

fn assert_restored_snapshot(f: &Fixture, first: &serde_json::Value, bundle_path: &Path) {
    // The bundle restores the dirty and untracked content on top of the base.
    let restore = f.temp.path().join("restore");
    git(
        f.temp.path(),
        &[
            "clone",
            "-q",
            f.remote.to_str().unwrap(),
            restore.to_str().unwrap(),
        ],
    );
    git(
        &restore,
        &["bundle", "verify", bundle_path.to_str().unwrap()],
    );
    git(
        &restore,
        &[
            "fetch",
            "-q",
            bundle_path.to_str().unwrap(),
            "refs/aq-git/snapshot:refs/snapshot",
        ],
    );
    let snapshot = first["snapshot"]["hex"].as_str().unwrap();
    assert_eq!(git(&restore, &["rev-parse", "refs/snapshot"]), snapshot);
    assert_eq!(
        git(
            &restore,
            &["show", &format!("{snapshot}:new/untracked.txt")]
        ),
        "untracked work"
    );
    assert_eq!(
        git(&restore, &["show", &format!("{snapshot}:file.txt")]),
        "dirty tracked edit"
    );
    assert!(git(&restore, &["ls-tree", snapshot, "ignored"]).is_empty());
}
