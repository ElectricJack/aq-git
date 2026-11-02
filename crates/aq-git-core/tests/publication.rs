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
        let mut binding = json!({"identity":"fixture","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/source","refs/heads/other"],"allowed_targets":["refs/heads/main", "refs/heads/new"]});
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
        let handle = uuid::Uuid::new_v4().to_string();
        let name = format!("refs/aq-git/fixture/{handle}");
        git(
            &service.repositories.pool_path(&repo),
            &["update-ref", &name, &source],
        );
        service
            .root
            .create_retention(&RetentionRecord {
                version: 1,
                id: handle.clone(),
                repository_id: repo.clone(),
                binding_generation: 1,
                generation: 1,
                resource: "fixture".into(),
                refs: vec![RetentionRef {
                    name,
                    oid: source.clone(),
                }],
                state: RetentionState::Active,
                dependencies: BTreeSet::new(),
                release_operation: None,
                release_reason: None,
            })
            .unwrap();
        let created=success(service.execute(request(&repo,"candidate.create",json!({"base":oid(&base),"sources":[{"oid":oid(&source),"retention_handle":handle}],"commit":{"name":"Candidate","email":"candidate@example.test","message":"publication"}}))));
        let candidate = created["candidate_id"].as_str().unwrap().into();
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
fn lost_output_after_real_push_never_redispatches_and_reconciles_presence() {
    let mut f = Fixture::new();
    f.suppress_push_output();
    let publish = f.publish();
    let id = publish.operation_id.clone().unwrap();
    let unknown = f.service.execute(publish.clone());
    assert_eq!(unknown.error.unwrap().code, "operation_indeterminate");
    assert_eq!(git(&f.remote, &["rev-parse", "refs/heads/main"]), f.source);
    let before = f.service.repositories.runner.metrics().spawns;
    assert_eq!(
        f.service.execute(publish.clone()).error.unwrap().code,
        "operation_indeterminate"
    );
    assert_eq!(before, f.service.repositories.runner.metrics().spawns);
    assert_eq!(
        f.service.execute(f.publish()).error.unwrap().code,
        "operation_indeterminate"
    );
    let target = f.service.root.operation(&id).unwrap().unwrap();
    let result = success(f.service.execute(request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":id,"expected_generation":target.generation}),
    )));
    assert_eq!(result["resolution"]["outcome"], "already_present");
    assert_eq!(result["resolution"]["caused_by_this_operation"], false);
    assert_eq!(
        success(f.service.execute(publish))["outcome"],
        "already_present"
    );
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
#[test]
fn unchanged_old_after_uncertainty_stays_unknown_until_audited_resolution() {
    let mut f = Fixture::new();
    f.suppress_push_output();
    let publish = f.publish();
    let id = publish.operation_id.clone().unwrap();
    assert_eq!(
        f.service.execute(publish).error.unwrap().code,
        "operation_indeterminate"
    );
    git(&f.remote, &["update-ref", "refs/heads/main", &f.base]);
    let target = f.service.root.operation(&id).unwrap().unwrap();
    let reconciled = success(f.service.execute(request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":id,"expected_generation":target.generation}),
    )));
    assert_eq!(reconciled["outcome"], "indeterminate");
    assert_eq!(
        f.service.root.operation(&id).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Pending
    );
    let mut resolve = request(
        &f.repo,
        "operation.resolve",
        json!({"target_operation_id":id,"expected_generation":target.generation,"disposition":"permit_new_operation","reason":"fixture stopped old publisher and inspected remote","old_publishers_stopped":false,"evidence":{"fixture":"old engine stopped"}}),
    );
    assert_eq!(
        f.service.execute(resolve.clone()).error.unwrap().code,
        "invalid_request"
    );
    resolve.operation_id = Some(uuid::Uuid::new_v4().to_string());
    resolve.params["old_publishers_stopped"] = json!(true);
    let resolved = success(f.service.execute(resolve.clone()));
    assert_eq!(
        resolved["resolution"]["historical_remote_effect"],
        "unknown"
    );
    assert_eq!(resolved, success(f.service.execute(resolve)));
    f.service.repositories.runner = GitRunner::new("/usr/bin/git".into());
    assert_eq!(
        success(f.service.execute(f.publish()))["outcome"],
        "applied"
    );
}

#[test]
fn reconciliation_and_resolution_resume_between_domain_and_journal_acknowledgements() {
    for resolve in [false, true] {
        let mut f = Fixture::new();
        f.suppress_push_output();
        let publish = f.publish();
        let id = publish.operation_id.clone().unwrap();
        assert_eq!(
            f.service.execute(publish).error.unwrap().code,
            "operation_indeterminate"
        );
        let target_before = f.service.root.operation(&id).unwrap().unwrap();
        let command = if resolve {
            request(
                &f.repo,
                "operation.resolve",
                json!({"target_operation_id":id,"expected_generation":target_before.generation,"disposition":"abandon","reason":"fixture stopped publishers","old_publishers_stopped":true,"evidence":"fixture cleanup complete"}),
            )
        } else {
            request(
                &f.repo,
                "operation.reconcile",
                json!({"target_operation_id":id,"expected_generation":target_before.generation}),
            )
        };
        let own_id = command.operation_id.as_ref().unwrap();
        let own_before=f.service.root.begin_operation(own_id,&json!({"operation":command.operation,"repository_id":command.repository_id,"expected_binding_generation":command.expected_binding_generation,"params":command.params})).unwrap();
        let result = success(f.service.execute(command.clone()));
        // Reconstruct the durable crash boundary: domain result installed, target
        // and command acknowledgements not yet persisted. No Git write is retried.
        f.service
            .root
            .write_json(format!("operations/{id}.json"), &target_before)
            .unwrap();
        f.service
            .root
            .write_json(format!("operations/{own_id}.json"), &own_before)
            .unwrap();
        assert_eq!(result, success(f.service.execute(command.clone())));
        assert_eq!(
            f.service.root.operation(&id).unwrap().unwrap().status,
            aq_git_core::state::OperationStatus::Complete
        );
        f.service
            .root
            .write_json(format!("operations/{own_id}.json"), &own_before)
            .unwrap();
        assert_eq!(result, success(f.service.execute(command)));
    }
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
fn replaying_old_acknowledgements_cannot_clear_a_successor_indeterminate_hold() {
    use sha2::{Digest, Sha256};
    for mode in ["publish", "operation.resolve", "operation.reconcile"] {
        let mut f = Fixture::new();
        let original = f.publish();
        let original_id = original.operation_id.clone().unwrap();
        let replay = if mode == "publish" {
            success(f.service.execute(original.clone()));
            original
        } else {
            f.suppress_push_output();
            assert_eq!(
                f.service.execute(original).error.unwrap().code,
                "operation_indeterminate"
            );
            let target = f.service.root.operation(&original_id).unwrap().unwrap();
            let command = if mode == "operation.resolve" {
                request(
                    &f.repo,
                    mode,
                    json!({"target_operation_id":original_id,"expected_generation":target.generation,"disposition":"abandon","reason":"fixture stopped old publishers","old_publishers_stopped":true,"evidence":"fixture cleanup complete"}),
                )
            } else {
                request(
                    &f.repo,
                    mode,
                    json!({"target_operation_id":original_id,"expected_generation":target.generation}),
                )
            };
            success(f.service.execute(command.clone()));
            command
        };
        let command_id = replay.operation_id.as_ref().unwrap();
        let mut pending = f.service.root.operation(command_id).unwrap().unwrap();
        let historical = pending.result.clone().unwrap();
        // Reconstruct the crash after domain completion/hold cleanup but before
        // this command's terminal journal acknowledgement reaches durability.
        pending.status = aq_git_core::state::OperationStatus::Pending;
        pending.result = None;
        pending.error = None;
        pending.generation -= 1;
        pending.phase = if mode == "publish" {
            "dispatched"
        } else {
            "intent"
        }
        .into();
        f.service
            .root
            .write_json(format!("operations/{command_id}.json"), &pending)
            .unwrap();
        git(&f.remote, &["update-ref", "refs/heads/main", &f.base]);
        f.suppress_push_output();
        let successor = f.publish();
        let successor_id = successor.operation_id.clone().unwrap();
        assert_eq!(
            f.service.execute(successor).error.unwrap().code,
            "operation_indeterminate"
        );
        let path = format!(
            "repositories/{}/publication-targets/{}.json",
            f.repo,
            hex::encode(Sha256::digest(b"refs/heads/main"))
        );
        let before: Value = f.service.root.read_json(&path).unwrap().unwrap();
        assert_eq!(before["active"], true);
        assert_eq!(before["operation_id"], successor_id);
        assert!(before["generation"].as_u64().unwrap() > 0);
        assert_eq!(success(f.service.execute(replay)), historical, "{mode}");
        let after: Value = f.service.root.read_json(&path).unwrap().unwrap();
        assert_eq!(
            after, before,
            "{mode} replay must not modify successor owner/generation"
        );
        let blocked = f.service.execute(f.publish()).error.unwrap();
        assert_eq!(blocked.code, "operation_indeterminate");
        assert_eq!(blocked.details["blocking_operation_id"], successor_id);
        assert_eq!(
            f.service
                .root
                .operation(&successor_id)
                .unwrap()
                .unwrap()
                .status,
            aq_git_core::state::OperationStatus::Pending
        );
        assert!(
            f.service
                .root
                .retention(&f.candidate)
                .unwrap()
                .unwrap()
                .dependencies
                .contains(&successor_id)
        );
        let unchanged: Value = f.service.root.read_json(&path).unwrap().unwrap();
        assert_eq!(unchanged, before);
    }
}

#[test]
fn unavailable_remote_reconciliation_preserves_hold_then_fresh_command_recovers() {
    let mut f = Fixture::new();
    f.suppress_push_output();
    let publish = f.publish();
    let target = publish.operation_id.clone().unwrap();
    assert_eq!(
        f.service.execute(publish).error.unwrap().code,
        "operation_indeterminate"
    );
    let before = f.service.root.operation(&target).unwrap().unwrap();
    let reconcile = request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":target,"expected_generation":before.generation}),
    );
    let saved = f.temp.path().join("remote-offline.git");
    std::fs::rename(&f.remote, &saved).unwrap();
    let unavailable = success(f.service.execute(reconcile.clone()));
    assert_eq!(unavailable["outcome"], "indeterminate");
    assert_eq!(unavailable["observation_error"], "remote_unavailable");
    let after = f.service.root.operation(&target).unwrap().unwrap();
    assert_eq!(after.status, aq_git_core::state::OperationStatus::Pending);
    assert_eq!(after.generation, before.generation);
    assert!(
        f.service
            .root
            .retention(&f.candidate)
            .unwrap()
            .unwrap()
            .dependencies
            .contains(&target)
    );
    assert_eq!(
        f.service.execute(f.publish()).error.unwrap().code,
        "operation_indeterminate"
    );
    std::fs::rename(&saved, &f.remote).unwrap();
    let spawns = f.service.repositories.runner.metrics().spawns;
    assert_eq!(success(f.service.execute(reconcile)), unavailable);
    assert_eq!(spawns, f.service.repositories.runner.metrics().spawns);
    let recovered = success(f.service.execute(request(
        &f.repo,
        "operation.reconcile",
        json!({"target_operation_id":target,"expected_generation":after.generation}),
    )));
    assert_eq!(recovered["resolution"]["outcome"], "already_present");
    assert_eq!(recovered["resolution"]["caused_by_this_operation"], false);
    assert_eq!(
        f.service.root.operation(&target).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Complete
    );
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
    assert_eq!(
        f.service.execute(publish).error.unwrap().code,
        "operation_indeterminate"
    );
    assert_eq!(f.service.repositories.runner.metrics().spawns, spawns);
    assert_eq!(
        f.service.execute(f.publish()).error.unwrap().code,
        "operation_indeterminate"
    );
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
fn https_token_profile_fetches_and_publishes() {
    let https = https::HttpsRemote::new();
    let f = Fixture::with_transport(None, Some(&https));
    assert_eq!(
        success(f.service.execute(f.publish()))["outcome"],
        "applied"
    );
    let mut create = f.publish();
    create.params["target"] = json!("refs/heads/new");
    create.params["expected_old"] = oid(&"0".repeat(40));
    assert_eq!(success(f.service.execute(create))["outcome"], "applied");
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.source);
    assert_eq!(git(&f.remote, &["rev-parse", "new"]), f.source);
    // A changed file is read on the next invocation; no prior helper caches the token.
    let mut profiles = json!({"token":https.profile()});
    let denied = https.temp.path().join("denied-token");
    std::fs::write(&denied, "synthetic_wrong_token").unwrap();
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o600)).unwrap();
    profiles["token"]["token_path"] = json!(denied);
    f.service
        .root
        .write_json("credential_profiles.json", &profiles)
        .unwrap();
    assert_eq!(
        f.service
            .execute(request(&f.repo, "repo.sync", json!({})))
            .error
            .unwrap()
            .code,
        "remote_auth_failed"
    );
}
