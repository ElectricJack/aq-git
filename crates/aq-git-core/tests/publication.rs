#[path = "support/https.rs"]
mod https;
#[path = "support/ssh.rs"]
mod ssh;
use aq_git_core::{
    RepositoryService,
    git::GitRunner,
    state::{RetentionRecord, RetentionRef, RetentionState},
};
use aq_git_protocol::{Request, Response};
use serde_json::{Value, json};
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, path::Path, process::Command};
fn git(path: &Path, args: &[&str]) -> String {
    let o = Command::new("/usr/bin/git")
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
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().into()
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
fn candidate_from(service: &RepositoryService, repo: &str, (base, source): (&str, &str)) -> String {
    let handle = uuid::Uuid::new_v4().to_string();
    let name = format!("refs/aq-git/fixture/{handle}");
    git(
        &service.repositories.pool_path(repo),
        &["update-ref", &name, source],
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
                oid: source.into(),
            }],
            state: RetentionState::Active,
            dependencies: BTreeSet::new(),
            release_operation: None,
            release_reason: None,
        })
        .unwrap();
    let created=success(service.execute(request(repo,"candidate.create",json!({"base":oid(base),"sources":[{"oid":oid(source),"retention_handle":handle}],"commit":{"name":"Candidate","email":"candidate@example.test","message":"publication"}}))));
    created["candidate_id"].as_str().unwrap().into()
}
struct Fixture {
    temp: tempfile::TempDir,
    remote: std::path::PathBuf,
    service: RepositoryService,
    repo: String,
    base: String,
    source: String,
    other: String,
    candidate: String,
}
impl Fixture {
    fn new() -> Self {
        Self::with_ssh(None)
    }
    fn with_ssh(ssh: Option<&ssh::SshRemote>) -> Self {
        Self::with_transport(ssh, None)
    }
    fn with_transport(ssh: Option<&ssh::SshRemote>, https: Option<&https::HttpsRemote>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare"]);
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-b", "main"]);
        std::fs::write(work.join("a"), "base\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "base"]);
        let base = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-b", "source"]);
        std::fs::write(work.join("a"), "source\n").unwrap();
        git(&work, &["commit", "-am", "source"]);
        let source = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-b", "other", &base]);
        std::fs::write(work.join("a"), "other\n").unwrap();
        git(&work, &["commit", "-am", "other"]);
        let other = git(&work, &["rev-parse", "HEAD"]);
        git(
            &work,
            &["push", remote.to_str().unwrap(), "main", "source", "other"],
        );
        let mut service = RepositoryService::open(temp.path().join("state")).unwrap();
        let repo = uuid::Uuid::new_v4().to_string();
        let mut binding = json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/source","refs/heads/other"],"allowed_targets":["refs/heads/main", "refs/heads/new"],"allowed_target_prefixes":["refs/heads/aq/"]});
        if let Some(ssh) = ssh {
            service
                .root
                .write_json(
                    "credential_profiles.json",
                    &json!({"deploy":ssh.profile(&remote)}),
                )
                .unwrap();
            binding["remote_url"] = json!(ssh.url(&remote));
            binding["credential_profile"] = json!("deploy");
        }
        if let Some(https) = https {
            https.attach(&remote);
            service
                .root
                .write_json(
                    "credential_profiles.json",
                    &json!({"token":https.profile()}),
                )
                .unwrap();
            binding["remote_url"] = json!(https.url());
            binding["credential_profile"] = json!("token");
            service.repositories.runner = https.runner();
        }
        success(service.execute(request(&repo, "repo.register", binding)));
        success(service.execute(request(&repo, "repo.sync", json!({}))));
        let candidate = candidate_from(&service, &repo, (&base, &source));
        Self {
            temp,
            remote,
            service,
            repo,
            base,
            source,
            other,
            candidate,
        }
    }
    fn candidate_from(&self, source: &str) -> String {
        candidate_from(&self.service, &self.repo, (&self.base, source))
    }
    fn publish(&self) -> Request {
        request(
            &self.repo,
            "publish",
            json!({"candidate_id":self.candidate,"target":"refs/heads/main","expected_old":oid(&self.base)}),
        )
    }
    fn suppress_push_output(&mut self) {
        let wrapper = self.temp.path().join("git-wrapper");
        std::fs::write(&wrapper,b"#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = push ]; then /usr/bin/git \"$@\" >/dev/null 2>/dev/null; exit $?; fi\ndone\nexec /usr/bin/git \"$@\"\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        self.service.repositories.runner = GitRunner::new(wrapper);
    }
}
#[test]
fn expected_old_applied_up_to_date_and_moved_target() {
    let f = Fixture::new();
    let publish = f.publish();
    let first = success(f.service.execute(publish.clone()));
    assert_eq!(first["outcome"], "applied");
    assert_eq!(git(&f.remote, &["rev-parse", "refs/heads/main"]), f.source);
    assert_eq!(first, success(f.service.execute(publish)));
    let second = success(f.service.execute(f.publish()));
    assert_eq!(second["outcome"], "already_present");
    assert_eq!(second["caused_by_this_operation"], false);
    git(&f.remote, &["update-ref", "refs/heads/main", &f.other]);
    let moved = success(f.service.execute(f.publish()));
    assert_eq!(moved["outcome"], "rejected");
    assert_eq!(moved["reason"], "target_moved");
    let mut rewrite = f.publish();
    rewrite.params["expected_old"] = oid(&f.other);
    assert_eq!(
        f.service.execute(rewrite).error.unwrap().code,
        "target_moved"
    );
    assert_eq!(git(&f.remote, &["rev-parse", "refs/heads/main"]), f.other);
}
#[test]
fn remote_races_after_preflight_preserve_lease_and_up_to_date_causality() {
    for present in [false, true] {
        let mut f = Fixture::new();
        let wrapper = f.temp.path().join("racing-git");
        let remote = f.remote.to_str().unwrap().replace('\'', "'\\''");
        let moved = if present {
            f.source.clone()
        } else {
            f.other.clone()
        };
        let script = format!(
            r#"#!/bin/sh
for arg in "$@"; do
    if [ "$arg" = push ]; then /usr/bin/git --git-dir '{remote}' update-ref refs/heads/main {moved}; fi
done
exec /usr/bin/git "$@"
"#
        );
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        f.service.repositories.runner = GitRunner::new(wrapper);
        let result = success(f.service.execute(f.publish()));
        if present {
            assert_eq!(result["outcome"], "already_present");
            assert_eq!(result["caused_by_this_operation"], false);
        } else {
            assert_eq!(result["outcome"], "rejected");
            assert_eq!(result["reason"], "remote_rejected");
        }
        assert_eq!(git(&f.remote, &["rev-parse", "refs/heads/main"]), moved);
        assert!(
            f.service
                .root
                .retention(&f.candidate)
                .unwrap()
                .unwrap()
                .dependencies
                .is_empty()
        );
    }
}
#[test]
fn worker_unauthorized_target_and_released_candidate_refuse_before_git() {
    let f = Fixture::new();
    let mut unauthorized = f.publish();
    unauthorized.params["target"] = json!("refs/heads/unconfigured");
    let before = f.service.repositories.runner.metrics().spawns;
    assert_eq!(
        f.service.execute(unauthorized).error.unwrap().code,
        "unsupported_feature"
    );
    assert_eq!(before, f.service.repositories.runner.metrics().spawns);
    let worker = uuid::Uuid::new_v4().to_string();
    success(f.service.execute(request(
        &worker,
        "repo.register",
        json!({"identity":"worker","mode":"worker"}),
    )));
    let mut publish = f.publish();
    publish.repository_id = Some(worker);
    let before = f.service.repositories.runner.metrics().spawns;
    assert_eq!(
        f.service.execute(publish).error.unwrap().code,
        "unsupported_feature"
    );
    assert_eq!(before, f.service.repositories.runner.metrics().spawns);
    let handle = f.service.root.retention(&f.candidate).unwrap().unwrap();
    success(f.service.execute(request(&f.repo,"retention.release",json!({"handle_id":f.candidate,"expected_generation":handle.generation,"refs":handle.refs,"reason":"candidate explicitly retired"}))));
    let before = f.service.repositories.runner.metrics().spawns;
    assert_eq!(
        f.service.execute(f.publish()).error.unwrap().code,
        "retention_released"
    );
    assert_eq!(before, f.service.repositories.runner.metrics().spawns);
}

#[test]
fn ssh_deploy_key_profile_fetches_and_publishes() {
    let ssh = ssh::SshRemote::new();
    let f = Fixture::with_ssh(Some(&ssh));
    assert_eq!(
        success(f.service.execute(f.publish()))["outcome"],
        "applied"
    );
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.source);
    let mut create = f.publish();
    create.params["target"] = json!("refs/heads/new");
    create.params["expected_old"] = oid(&"0".repeat(40));
    let result = success(f.service.execute(create.clone()));
    assert_eq!(result["outcome"], "applied");
    assert_eq!(success(f.service.execute(create)), result);
    assert_eq!(git(&f.remote, &["rev-parse", "new"]), f.source);
}

#[test]
fn remote_auth_failure_is_typed() {
    for wrong_host in [false, true] {
        let ssh = ssh::SshRemote::new();
        let f = Fixture::with_ssh(Some(&ssh));
        if wrong_host {
            ssh.wrong_host();
        } else {
            ssh.deny_key();
        }
        let sync = f.service.execute(request(&f.repo, "repo.sync", json!({})));
        assert_eq!(sync.error.unwrap().code, "remote_auth_failed");
        let publish = f.publish();
        let response = f.service.execute(publish.clone());
        assert_eq!(response.error.unwrap().code, "remote_auth_failed");
        assert_eq!(
            f.service.execute(publish).error.unwrap().code,
            "remote_auth_failed"
        );
        assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.base);
    }
}

#[test]
fn new_branch_requires_absent_lease_and_rejects_existing_target() {
    let f = Fixture::new();
    let mut create = f.publish();
    create.params["expected_old"] = oid(&"0".repeat(40));
    assert_eq!(
        success(f.service.execute(create.clone()))["reason"],
        "target_moved"
    );
    create.operation_id = Some(uuid::Uuid::new_v4().to_string());
    create.params["target"] = json!("refs/heads/new");
    assert_eq!(success(f.service.execute(create))["outcome"], "applied");
    assert_eq!(git(&f.remote, &["rev-parse", "new"]), f.source);
}

#[test]
fn ssh_push_auth_failure_keeps_dispatched_operation_fenced() {
    let ssh = ssh::SshRemote::new();
    let mut f = Fixture::with_ssh(Some(&ssh));
    let wrapper = f.temp.path().join("git-revoke-key");
    std::fs::write(&wrapper, format!("#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = push ]; then cp '{}' '{}'; fi\ndone\nexec /usr/bin/git \"$@\"\n", ssh.temp.path().join("wrong.pub").display(), ssh.temp.path().join("authorized_keys").display())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    f.service.repositories.runner = GitRunner::new(wrapper);
    let publish = f.publish();
    let id = publish.operation_id.clone().unwrap();
    let response = f.service.execute(publish.clone());
    assert_eq!(response.operation_state.as_deref(), Some("pending"));
    let error = response.error.unwrap();
    assert_eq!(error.code, "remote_auth_failed");
    assert_eq!(error.details["remote_effect_unknown"], true);
    assert_eq!(error.retry, aq_git_protocol::RetryAdvice::ReconcileFirst);
    let spawns = f.service.repositories.runner.metrics().spawns;
    assert_indeterminate(&f, publish);
    assert_eq!(f.service.repositories.runner.metrics().spawns, spawns);
    assert_indeterminate(&f, f.publish());
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.base);
    std::fs::copy(
        ssh.temp.path().join("client.pub"),
        ssh.temp.path().join("authorized_keys"),
    )
    .unwrap();
    let record = f.service.root.operation(&id).unwrap().unwrap();
    let reconcile = request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":id,"expected_generation":record.generation}),
    );
    assert_eq!(
        success(f.service.execute(reconcile))["outcome"],
        "indeterminate"
    );
}

#[test]
fn new_branch_racing_creator_is_never_overwritten() {
    let mut f = Fixture::new();
    let wrapper = f.temp.path().join("git-racing-creator");
    std::fs::write(&wrapper,format!("#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = push ]; then /usr/bin/git --git-dir='{}' update-ref refs/heads/new '{}'; fi\ndone\nexec /usr/bin/git \"$@\"\n",f.remote.display(), f.other)).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    f.service.repositories.runner = GitRunner::new(wrapper);
    let mut create = f.publish();
    create.params["target"] = json!("refs/heads/new");
    create.params["expected_old"] = oid(&"0".repeat(40));
    assert_eq!(success(f.service.execute(create))["outcome"], "rejected");
    assert_eq!(git(&f.remote, &["rev-parse", "new"]), f.other);
}

#[test]
fn https_token_profile_renews_for_publish_and_remote_delete() {
    let https = https::HttpsRemote::new();
    let f = Fixture::with_transport(None, Some(&https));
    assert_eq!(
        success(f.service.execute(f.publish()))["outcome"],
        "applied"
    );
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.source);
    let old_head = "refs/heads/aq/old-token";
    publish_head(&f, old_head);
    let renewed = "synthetic_renewed_token\n";
    std::fs::write(https.temp.path().join("accepted-token"), renewed).unwrap();
    assert_eq!(
        f.service
            .execute(request(&f.repo, "repo.sync", json!({})))
            .error
            .unwrap()
            .code,
        "remote_auth_failed"
    );
    // The next operation sees the renewed token at the same path on the same service.
    let replacement = https.temp.path().join("replacement");
    std::fs::write(&replacement, renewed).unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::rename(replacement, https.temp.path().join("token")).unwrap();
    let new_head = "refs/heads/aq/new-token";
    publish_head(&f, new_head);
    assert!(branches::remote_has(&f, new_head));
    let done = success(
        f.service
            .execute(delete_ref(&f, old_head, Some(&f.source), true)),
    );
    assert_eq!(done["remote"]["outcome"], "deleted");
    assert!(!branches::remote_has(&f, old_head));
}

fn publish_to(f: &Fixture, candidate: &str, (target, old): (&str, &str), update: &str) -> Request {
    request(
        &f.repo,
        "publish",
        json!({"candidate_id":candidate,"target":target,"expected_old":oid(old),"update":update}),
    )
}
fn delete_ref(f: &Fixture, name: &str, old: Option<&str>, remote: bool) -> Request {
    request(
        &f.repo,
        "ref.delete",
        json!({"ref":name,"expected_old":old.map(oid),"remote":remote}),
    )
}
fn publish_head(f: &Fixture, head: &str) {
    let zero = "0".repeat(40);
    let create = publish_to(f, &f.candidate, (head, &zero), "fast_forward");
    assert_eq!(success(f.service.execute(create))["outcome"], "applied");
}

fn assert_indeterminate(f: &Fixture, command: Request) {
    assert_eq!(
        f.service.execute(command).error.unwrap().code,
        "operation_indeterminate"
    );
}
#[path = "publication/branches.rs"]
mod branches;
#[path = "publication/recovery.rs"]
mod recovery;
#[path = "publication/rejection.rs"]
mod rejection;
