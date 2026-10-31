use aq_git_core::{
    RepositoryService,
    state::{RetentionRecord, RetentionRef, RetentionState},
};
use aq_git_protocol::{Request, Response};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, process::Command};
fn git_output(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new("/usr/bin/git")
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "f@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "f@example.test")
        .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
        .output()
        .unwrap()
}
fn git(path: &Path, args: &[&str]) -> String {
    let o = git_output(path, args);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().into()
}

#[test]
fn candidate_merge_profile_matches_git_for_renames_binary_modes_and_directory_conflicts() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("original"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    std::fs::write(remote.join("binary"), b"base\0bytes\n").unwrap();
    std::fs::write(remote.join("script"), "#!/bin/sh\necho base\n").unwrap();
    symlink("base-target", remote.join("link")).unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "base"]);
    let base = git(&remote, &["rev-parse", "HEAD"]);
    let mut cases = Vec::new();
    let mut refs = vec!["refs/heads/main".to_owned()];
    for kind in [
        "rename",
        "binary",
        "symlink",
        "executable",
        "directory-file",
        "rename-edit",
    ] {
        let mut tips = Vec::new();
        for side in ["left", "right"] {
            let branch = format!("{kind}-{side}");
            git(&remote, &["checkout", "-b", &branch, &base]);
            match kind {
                "rename" => {
                    git(&remote, &["mv", "original", &format!("renamed-{side}")]);
                }
                "binary" => {
                    std::fs::write(remote.join("binary"), format!("{side}\0bytes\n")).unwrap()
                }
                "symlink" => {
                    std::fs::remove_file(remote.join("link")).unwrap();
                    symlink(format!("{side}-target"), remote.join("link")).unwrap();
                }
                "executable" if side == "left" => std::fs::set_permissions(
                    remote.join("script"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap(),
                "executable" => {
                    std::fs::write(remote.join("script"), "#!/bin/sh\necho changed\n").unwrap()
                }
                "directory-file" if side == "left" => {
                    std::fs::write(remote.join("collision"), "file\n").unwrap()
                }
                "directory-file" => {
                    std::fs::create_dir(remote.join("collision")).unwrap();
                    std::fs::write(remote.join("collision/child"), "directory child\n").unwrap();
                }
                "rename-edit" if side == "left" => {
                    git(&remote, &["mv", "original", "renamed"]);
                }
                "rename-edit" => {
                    std::fs::write(remote.join("original"), "one\ntwo\nthree\nfour\nchanged\n")
                        .unwrap()
                }
                _ => unreachable!(),
            }
            git(&remote, &["add", "."]);
            git(&remote, &["commit", "-m", &branch]);
            tips.push(git(&remote, &["rev-parse", "HEAD"]));
            refs.push(format!("refs/heads/{branch}"));
        }
        cases.push((kind, tips));
    }
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    success(service.execute(request(&repo, "repo.register", json!({"identity":"merge-profile", "mode":"central", "remote_url":remote, "allow_file_transport":true, "allowed_refs":refs}))));
    success(service.execute(request(&repo, "repo.sync", json!({}))));
    for (kind, tips) in cases {
        let baseline = git_output(
            &remote,
            &[
                "-c",
                "merge.renormalize=false",
                "-c",
                "merge.renames=true",
                "-c",
                "merge.directoryRenames=conflict",
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "-z",
                &tips[0],
                &tips[1],
            ],
        );
        let clean = matches!(kind, "executable" | "rename-edit");
        assert_eq!(
            baseline.status.code(),
            Some(if clean { 0 } else { 1 }),
            "{kind}: {:?}",
            baseline.stderr
        );
        let lh = retain(&service, &repo, &tips[0]);
        let rh = retain(&service, &repo, &tips[1]);
        let request = request(
            &repo,
            "candidate.create",
            json!({"base":oid(&base), "sources":[{"oid":oid(&tips[0]), "retention_handle":lh}, {"oid":oid(&tips[1]), "retention_handle":rh}], "commit":{"name":"Candidate", "email":"fixture@example.test", "message":kind}}),
        );
        let result = success(service.execute(request.clone()));
        let mut fields = baseline.stdout.split(|b| *b == 0);
        let tree = std::str::from_utf8(fields.next().unwrap()).unwrap();
        if clean {
            assert_eq!(result["state"], "ready", "{kind}");
            let candidate = result["candidate"]["hex"].as_str().unwrap();
            let pool = service.repositories.pool_path(&repo);
            assert_eq!(
                git(&pool, &["rev-parse", &format!("{candidate}^{{tree}}")]),
                tree,
                "{kind}"
            );
            if kind == "executable" {
                assert!(git(&pool, &["ls-tree", candidate, "script"]).starts_with("100755 "));
                assert_eq!(
                    git(&pool, &["show", &format!("{candidate}:script")]),
                    "#!/bin/sh\necho changed"
                );
            } else {
                assert_eq!(
                    git(&pool, &["show", &format!("{candidate}:renamed")]),
                    "one\ntwo\nthree\nfour\nchanged"
                );
            }
        } else {
            assert_eq!(result["state"], "conflicted", "{kind}");
            assert_eq!(result["clean_prefix"]["hex"], tips[0]);
            assert_eq!(result["conflict_step"], 1);
            assert_eq!(result["conflict_paths_truncated"], false);
            let expected: BTreeSet<_> = fields
                .filter(|path| !path.is_empty())
                .map(hex::encode)
                .collect();
            let actual: BTreeSet<_> = result["conflicting_paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|path| {
                    assert_eq!(path["encoding"], "hex");
                    path["value"].as_str().unwrap().to_owned()
                })
                .collect();
            assert!(!expected.is_empty(), "{kind}");
            assert_eq!(actual, expected, "{kind}");
        }
        assert_eq!(result, success(service.execute(request)), "{kind} replay");
    }
}
fn request(repo: &str, operation: &str, params: Value) -> Request {
    Request {
        protocol_version: 1,
        request_id: "test".into(),
        operation: operation.into(),
        repository_id: Some(repo.into()),
        operation_id: Some(uuid::Uuid::new_v4().to_string()),
        expected_binding_generation: Some(if operation == "repo.register" { 0 } else { 1 }),
        params,
        deadline_ms: None,
    }
}
fn success(response: Response) -> Value {
    assert!(response.ok, "{:?}", response.error);
    response.result.unwrap()
}
fn oid(id: &str) -> Value {
    json!({"algorithm":"sha1","hex":id})
}
fn retain(service: &RepositoryService, repo: &str, id: &str) -> String {
    let handle = uuid::Uuid::new_v4().to_string();
    let name = format!("refs/aq-git/fixture/{handle}");
    git(
        &service.repositories.pool_path(repo),
        &["update-ref", &name, id],
    );
    service
        .root
        .create_retention(&RetentionRecord {
            version: 1,
            id: handle.clone(),
            repository_id: repo.into(),
            binding_generation: 1,
            generation: 1,
            resource: "fixture".into(),
            refs: vec![RetentionRef {
                name,
                oid: id.into(),
            }],
            state: RetentionState::Active,
            dependencies: BTreeSet::new(),
            release_operation: None,
            release_reason: None,
        })
        .unwrap();
    handle
}
#[test]
fn frozen_merge_conflict_replay_and_independent_retention() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]);
    std::fs::write(remote.join("a"), "base\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "base"]);
    let base = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["checkout", "-b", "left"]);
    std::fs::write(remote.join("left"), "left\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "left"]);
    let left = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["checkout", "-b", "right", &base]);
    std::fs::write(remote.join("right"), "right\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-m", "right"]);
    let right = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["checkout", "-b", "conflict-left", &base]);
    std::fs::write(remote.join("a"), "left edit\n").unwrap();
    git(&remote, &["commit", "-am", "left edit"]);
    let cleft = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["checkout", "-b", "conflict-right", &base]);
    std::fs::write(remote.join("a"), "right edit\n").unwrap();
    git(&remote, &["commit", "-am", "right edit"]);
    let cright = git(&remote, &["rev-parse", "HEAD"]);
    let mut service = RepositoryService::open(temp.path().join("state")).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    success(service.execute(request(&repo,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/left","refs/heads/right","refs/heads/conflict-left","refs/heads/conflict-right"]}))));
    success(service.execute(request(&repo, "repo.sync", json!({}))));
    let lh = retain(&service, &repo, &left);
    let rh = retain(&service, &repo, &right);
    let make = |a: &str, ah: &str, b: &str, bh: &str| json!({"base":oid(&base),"sources":[{"oid":oid(a),"retention_handle":ah},{"oid":oid(b),"retention_handle":bh}],"commit":{"name":"Candidate","email":"candidate@example.test","message":"ordered merge"}});
    let req = request(&repo, "candidate.create", make(&left, &lh, &right, &rh));
    let result = success(service.execute(req.clone()));
    assert_eq!(result["state"], "ready");
    let candidate = result["candidate"]["hex"].as_str().unwrap();
    let parents = git(
        &service.repositories.pool_path(&repo),
        &["show", "-s", "--format=%P", candidate],
    );
    assert_eq!(parents, format!("{left} {right}"));
    assert!(
        service
            .root
            .retention(&lh)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
    assert!(
        service
            .root
            .retention(&rh)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
    git(&remote, &["update-ref", "refs/heads/left", &base]);
    assert_eq!(result, success(service.execute(req.clone())));
    let mut changed = req;
    changed.params["sources"].as_array_mut().unwrap().reverse();
    assert_eq!(
        service.execute(changed).error.unwrap().code,
        "idempotency_conflict"
    );
    let clh = retain(&service, &repo, &cleft);
    let crh = retain(&service, &repo, &cright);
    let conflict = success(service.execute(request(
        &repo,
        "candidate.create",
        make(&cleft, &clh, &cright, &crh),
    )));
    assert_eq!(conflict["state"], "conflicted");
    assert!(conflict["candidate"].is_null());
    assert_eq!(conflict["conflict_step"], 1);
    assert_eq!(
        conflict["conflicting_paths"],
        json!([{"encoding":"hex","value":"61"}])
    );
    assert_eq!(conflict["clean_prefix"]["hex"], cleft);
    // Repair uses an actual merge with both frozen parents, then a retained
    // checkpoint and a separately identified superseding candidate.
    let workspace = success(service.execute(request(
        &repo,
        "workspace.create",
        json!({"base":oid(&cleft)}),
    )));
    let path = Path::new(workspace["path"].as_str().unwrap());
    let merge = Command::new("/usr/bin/git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Repair",
            "-c",
            "user.email=repair@example.test",
            "merge",
            "--no-commit",
            "--no-ff",
            &cright,
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert_eq!(merge.status.code(), Some(1));
    std::fs::write(path.join("a"), "resolved both source edits\n").unwrap();
    git(path, &["add", "a"]);
    git(path, &["commit", "-m", "repair frozen conflicting source"]);
    let repaired = git(path, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(path, &["show", "-s", "--format=%P", "HEAD"]),
        format!("{cleft} {cright}")
    );
    let checkpoint=success(service.execute(request(&repo,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":oid(&repaired),"quiescent":true,"mode":"head"}))));
    let replacement=success(service.execute(request(&repo,"candidate.create",json!({"base":oid(&base),"sources":[{"oid":oid(&repaired),"retention_handle":checkpoint["retention_handle"]}],"commit":{"name":"Coordinator","email":"coordinator@example.test","message":"repaired candidate"},"supersedes":conflict["candidate_id"]}))));
    assert_eq!(replacement["state"], "ready");
    assert_eq!(replacement["candidate"]["hex"], repaired);
    assert_eq!(replacement["supersedes"], conflict["candidate_id"]);
    // A timed-out local construction still exposes its independent durable
    // input/prefix handle. Explicit release cancels this creation permanently.
    use std::os::unix::fs::PermissionsExt;
    let wrapper = temp.path().join("paused-merge-git");
    std::fs::write(&wrapper,b"#!/bin/sh\nfor arg in \"$@\"; do if [ \"$arg\" = merge-tree ]; then exec /bin/sleep 120; fi; done\nexec /usr/bin/git \"$@\"\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    service.repositories.runner = aq_git_core::git::GitRunner::new(wrapper);
    let mut timed = request(&repo, "candidate.create", make(&left, &lh, &right, &rh));
    timed.deadline_ms = Some(1000);
    let cid = timed.operation_id.clone().unwrap();
    let failure = service.execute(timed.clone());
    assert_eq!(failure.error.unwrap().code, "deadline_exceeded");
    let inspected = success(service.execute(request(
        &repo,
        "retention.inspect",
        json!({"handle_id":cid}),
    )));
    assert_eq!(inspected["state"], "active");
    assert!(!inspected["refs"].as_array().unwrap().is_empty());
    assert!(
        service
            .root
            .retention(&lh)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
    let released=success(service.execute(request(&repo,"retention.release",json!({"handle_id":cid,"expected_generation":inspected["generation"],"refs":inspected["refs"],"reason":"cancel timed-out local candidate"}))));
    assert_eq!(released["state"], "released");
    assert_eq!(
        service.root.operation(&cid).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Failed
    );
    let before = service.repositories.runner.metrics().spawns;
    let replay = service.execute(timed);
    assert_eq!(replay.error.unwrap().code, "retention_released");
    assert_eq!(before, service.repositories.runner.metrics().spawns);
    assert_eq!(
        service.root.operation(&cid).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Failed
    );
}

#[test]
fn advertised_handlers_refuse_unknown_parameters_before_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare"]);
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    success(service.execute(request(&repo,"repo.register",json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true}))));
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1/examples");
    for entry in std::fs::read_dir(examples).unwrap() {
        let path = entry.unwrap().path();
        if !path.to_string_lossy().ends_with(".request.json") {
            continue;
        }
        let mut r: Request = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        if r.operation == "recovery.clear-fence" {
            continue;
        }
        if r.repository_id.is_some() {
            r.repository_id = Some(repo.clone());
        }
        if r.operation_id.is_some() {
            r.operation_id = Some(uuid::Uuid::new_v4().to_string());
        }
        r.params["unexpected_parameter"] = json!(true);
        let operation = r.operation.clone();
        let before = service.repositories.runner.metrics().spawns;
        let response = service.execute(r);
        assert!(
            !response.ok,
            "{operation} silently accepted an unknown parameter"
        );
        assert_eq!(
            response.error.unwrap().code,
            "invalid_request",
            "{operation}"
        );
        assert_eq!(
            before,
            service.repositories.runner.metrics().spawns,
            "{operation} invoked Git before rejecting unknown parameters"
        );
    }
}
