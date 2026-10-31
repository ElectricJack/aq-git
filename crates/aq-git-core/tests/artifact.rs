use aq_git_core::{
    RepositoryService,
    state::{RetentionRecord, RetentionRef, RetentionState},
};
use aq_git_protocol::Request;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .current_dir(path)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "f@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "f@example.test")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}
fn oid(s: &str) -> Value {
    json!({"algorithm":"sha1","hex":s})
}
fn request(repo: &str, operation: &str, params: Value) -> Request {
    Request {
        protocol_version: 1,
        request_id: "fixture".into(),
        operation: operation.into(),
        repository_id: Some(repo.into()),
        operation_id: Some(uuid::Uuid::new_v4().to_string()),
        expected_binding_generation: Some(if operation == "repo.register" { 0 } else { 1 }),
        params,
        deadline_ms: None,
    }
}
fn run(service: &RepositoryService, request: Request) -> Value {
    let response = service.execute(request);
    assert!(response.ok, "{:?}", response.error);
    response.result.unwrap()
}
struct Fixture {
    _temp: tempfile::TempDir,
    source: RepositoryService,
    receiver: RepositoryService,
    repo: String,
    first: String,
    second: String,
    source_handle: String,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "-b", "main"]);
        fs::write(remote.join("file"), "one\n").unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "one"]);
        let first = git(&remote, &["rev-parse", "HEAD"]);
        fs::write(remote.join("file"), "two\n").unwrap();
        git(&remote, &["commit", "-am", "two"]);
        let second = git(&remote, &["rev-parse", "HEAD"]);
        let source = RepositoryService::open(temp.path().join("source")).unwrap();
        let receiver = RepositoryService::open(temp.path().join("receiver")).unwrap();
        let repo = uuid::Uuid::new_v4().to_string();
        run(
            &source,
            request(
                &repo,
                "repo.register",
                json!({"identity":"same-repository","mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}),
            ),
        );
        run(&source, request(&repo, "repo.sync", json!({})));
        run(
            &receiver,
            request(
                &repo,
                "repo.register",
                json!({"identity":"same-repository","mode":"worker","binding_generation":1}),
            ),
        );
        let source_handle = uuid::Uuid::new_v4().to_string();
        let reference = format!("refs/aq-git/checkpoints/{source_handle}");
        source
            .repositories
            .git(
                &repo,
                vec!["update-ref".into(), reference.clone(), second.clone()],
            )
            .unwrap();
        source
            .root
            .create_retention(&RetentionRecord {
                version: 1,
                id: source_handle.clone(),
                repository_id: repo.clone(),
                binding_generation: 1,
                generation: 1,
                resource: "checkpoint".into(),
                refs: vec![RetentionRef {
                    name: reference,
                    oid: second.clone(),
                }],
                state: RetentionState::Active,
                dependencies: BTreeSet::new(),
                release_operation: None,
                release_reason: None,
            })
            .unwrap();
        Self {
            _temp: temp,
            source,
            receiver,
            repo,
            first,
            second,
            source_handle,
        }
    }
    fn export(
        &self,
        source: &str,
        base: &str,
        prerequisites: Value,
        reference_only: bool,
    ) -> Value {
        run(
            &self.source,
            request(
                &self.repo,
                "artifact.export",
                json!({"source":oid(source),"base":oid(base),"retention_handle":self.source_handle,"confirmed_prerequisites":prerequisites,"reference_only":reference_only}),
            ),
        )
    }
    fn stage(&self, artifact: &Value) -> (String, PathBuf) {
        let handle = uuid::Uuid::new_v4().to_string();
        let target = self
            .receiver
            .root
            .create_dir(format!("artifacts/incoming/{handle}"))
            .unwrap();
        let source = self
            .source
            .root
            .path()
            .join("artifacts")
            .join(artifact["artifact_handle"].as_str().unwrap());
        fs::copy(source.join("manifest.json"), target.join("manifest.json")).unwrap();
        if source.join("objects.bundle").exists() {
            fs::copy(source.join("objects.bundle"), target.join("objects.bundle")).unwrap();
        }
        (handle, target)
    }
    fn import(&self, handle: &str) -> Request {
        request(
            &self.repo,
            "artifact.import",
            json!({"artifact_handle":handle}),
        )
    }
}
#[test]
fn complete_transfer_is_independent_and_replays() {
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, _) = f.stage(&export);
    let req = f.import(&handle);
    let receipt = run(&f.receiver, req.clone());
    assert_eq!(receipt["source"], oid(&f.second));
    assert_eq!(run(&f.receiver, req), receipt);
    fs::remove_dir_all(f.source.repositories.pool_path(&f.repo)).unwrap();
    assert_eq!(
        f.receiver
            .repositories
            .git(
                &f.repo,
                vec!["cat-file".into(), "-t".into(), f.second.clone()]
            )
            .unwrap(),
        b"commit\n"
    );
    let replay = run(&f.receiver, f.import(&handle));
    assert_eq!(replay["historical"], true);
}
#[test]
fn corrupted_identity_and_digest_leave_target_unpinned() {
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, path) = f.stage(&export);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
    manifest["repository_identity"] = json!("other");
    fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert_eq!(
        f.receiver.execute(f.import(&handle)).error.unwrap().code,
        "binding_conflict"
    );
    manifest["repository_identity"] = json!("same-repository");
    fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(path.join("objects.bundle"), "corrupted").unwrap();
    assert_eq!(
        f.receiver.execute(f.import(&handle)).error.unwrap().code,
        "artifact_digest_mismatch"
    );
    assert!(
        f.receiver
            .root
            .retention(export["artifact_handle"].as_str().unwrap())
            .unwrap()
            .is_none()
    );
}
#[test]
fn incremental_requires_prerequisites_and_reference_only_checks_source() {
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([oid(&f.first)]), false);
    let (handle, _) = f.stage(&export);
    assert_eq!(
        f.receiver.execute(f.import(&handle)).error.unwrap().code,
        "prerequisite_missing"
    );
    let full = f.export(&f.second, &f.first, json!([]), false);
    let (full_handle, _) = f.stage(&full);
    run(&f.receiver, f.import(&full_handle));
    run(&f.receiver, f.import(&handle));
    let reference = f.export(&f.second, &f.first, json!([oid(&f.second)]), true);
    assert_eq!(reference["manifest"]["bundle_size"], 0);
    let (reference_handle, _) = f.stage(&reference);
    run(&f.receiver, f.import(&reference_handle));
}
#[test]
fn unadvertised_refs_and_transfer_reuse_are_rejected() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, path) = f.stage(&export);
    let bytes = fs::read(path.join("objects.bundle")).unwrap();
    let needle = format!(
        "refs/aq-git/transfers/{}",
        export["artifact_handle"].as_str().unwrap()
    );
    let replacement = needle.replacen("transfers", "intruders", 1);
    assert_eq!(needle.len(), replacement.len());
    let mut bytes = bytes;
    let offset = bytes
        .windows(needle.len())
        .position(|x| x == needle.as_bytes())
        .unwrap();
    bytes[offset..offset + needle.len()].copy_from_slice(replacement.as_bytes());
    fs::write(path.join("objects.bundle"), &bytes).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
    manifest["bundle_sha256"] = json!(hex::encode(Sha256::digest(&bytes)));
    fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert_eq!(
        f.receiver.execute(f.import(&handle)).error.unwrap().code,
        "invalid_artifact"
    );
    let (good_handle, good_path) = f.stage(&export);
    run(&f.receiver, f.import(&good_handle));
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(good_path.join("manifest.json")).unwrap()).unwrap();
    manifest["caller_correlation"] = json!({"different":"payload"});
    fs::write(
        good_path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert_eq!(
        f.receiver
            .execute(f.import(&good_handle))
            .error
            .unwrap()
            .code,
        "idempotency_conflict"
    );
}
#[test]
fn import_replay_after_release_is_historical_without_repinning() {
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, _) = f.stage(&export);
    let receipt = run(&f.receiver, f.import(&handle));
    let transfer = receipt["transfer_id"].as_str().unwrap();
    let record = f.receiver.root.retention(transfer).unwrap().unwrap();
    let releasing = f
        .receiver
        .root
        .begin_release(transfer, record.generation)
        .unwrap();
    for reference in &releasing.refs {
        f.receiver
            .repositories
            .git(
                &f.repo,
                vec![
                    "update-ref".into(),
                    "-d".into(),
                    reference.name.clone(),
                    reference.oid.clone(),
                ],
            )
            .unwrap();
    }
    f.receiver
        .root
        .finish_release(transfer, releasing.generation)
        .unwrap();
    let replay = run(&f.receiver, f.import(&handle));
    assert_eq!(replay["retention_state"], "released");
    assert_eq!(replay["historical"], true);
    assert_eq!(
        f.receiver.root.retention(transfer).unwrap().unwrap().state,
        RetentionState::Released
    );
}
#[test]
fn interrupted_receipt_and_export_manifest_resume_exact_intent() {
    use aq_git_core::state::WriteFault;
    let f = Fixture::new();
    let export_request = request(
        &f.repo,
        "artifact.export",
        json!({"source":oid(&f.second),"base":oid(&f.first),"retention_handle":f.source_handle}),
    );
    let export_id = export_request.operation_id.clone().unwrap();
    f.source
        .root
        .inject_write_failure_after(4, WriteFault::BeforeRename);
    assert_eq!(
        f.source.execute(export_request.clone()).error.unwrap().code,
        "io_error"
    );
    assert_eq!(
        f.source.root.operation(&export_id).unwrap().unwrap().status,
        aq_git_core::state::OperationStatus::Pending
    );
    assert!(
        !f.source
            .root
            .retention(&f.source_handle)
            .unwrap()
            .unwrap()
            .dependencies
            .contains(&export_id)
    );
    assert!(f.source.root.retention(&export_id).unwrap().is_some());
    let export = run(&f.source, export_request);
    let (handle, _) = f.stage(&export);
    let import_request = f.import(&handle);
    let import_id = import_request.operation_id.clone().unwrap();
    f.receiver
        .root
        .inject_write_failure_after(4, WriteFault::BeforeRename);
    assert_eq!(
        f.receiver
            .execute(import_request.clone())
            .error
            .unwrap()
            .code,
        "io_error"
    );
    assert_eq!(
        f.receiver
            .root
            .operation(&import_id)
            .unwrap()
            .unwrap()
            .phase,
        "artifact_validated"
    );
    assert!(
        f.receiver.root.retention(&export_id).unwrap().is_some(),
        "import state: {:?}; files {:?}",
        f.receiver.root.operation(&import_id).unwrap(),
        fs::read_dir(f.receiver.root.path().join("retention"))
            .map(|r| r.map(|e| e.unwrap().path()).collect::<Vec<_>>())
    );
    assert!(
        !f.receiver
            .root
            .path()
            .join(format!(
                "repositories/{}/transfers/{export_id}.json",
                f.repo
            ))
            .exists()
    );
    let receipt = run(&f.receiver, import_request.clone());
    assert_eq!(receipt["transfer_id"], export_id);
    assert_eq!(run(&f.receiver, import_request), receipt);
}
#[test]
fn configured_compressed_and_expanded_quotas_refuse_before_receipt() {
    let f = Fixture::new();
    f.source
        .root
        .write_json(
            "artifact_limits.json",
            &json!({"max_bundle_bytes":32,"max_expanded_bytes":1048576,"disk_reserve_bytes":0}),
        )
        .unwrap();
    let rejected = f.source.execute(request(
        &f.repo,
        "artifact.export",
        json!({"source":oid(&f.second),"base":oid(&f.first),"retention_handle":f.source_handle}),
    ));
    assert_eq!(rejected.error.unwrap().code, "output_limit");
    f.source.root.write_json("artifact_limits.json",&json!({"max_bundle_bytes":1048576,"max_expanded_bytes":1048576,"disk_reserve_bytes":0})).unwrap();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, _) = f.stage(&export);
    f.receiver
        .root
        .write_json(
            "artifact_limits.json",
            &json!({"max_bundle_bytes":32,"max_expanded_bytes":1048576,"disk_reserve_bytes":0}),
        )
        .unwrap();
    assert_eq!(
        f.receiver.execute(f.import(&handle)).error.unwrap().code,
        "limit_exceeded"
    );
    f.receiver
        .root
        .write_json(
            "artifact_limits.json",
            &json!({"max_bundle_bytes":1048576,"max_expanded_bytes":1024,"disk_reserve_bytes":0}),
        )
        .unwrap();
    assert_eq!(
        f.receiver.execute(f.import(&handle)).error.unwrap().code,
        "storage_limit"
    );
    assert!(
        f.receiver
            .root
            .retention(export["artifact_handle"].as_str().unwrap())
            .unwrap()
            .is_none()
    );
}
#[test]
fn released_pool_evicts_then_new_transfer_rehydrates() {
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, _) = f.stage(&export);
    let receipt = run(&f.receiver, f.import(&handle));
    let transfer = receipt["retention_handle"].as_str().unwrap();
    let pin = f.receiver.root.retention(transfer).unwrap().unwrap();
    run(
        &f.receiver,
        request(
            &f.repo,
            "retention.release",
            json!({"handle_id":transfer,"expected_generation":pin.generation,"refs":pin.refs,"reason":"handoff done"}),
        ),
    );
    run(
        &f.receiver,
        request(
            &f.repo,
            "repo.cache-evict",
            json!({"expected_storage_generation":1,"reconstructible":true,"reason":"fixture rehydration"}),
        ),
    );
    assert!(!f.receiver.repositories.pool_path(&f.repo).exists());
    let historical = run(&f.receiver, f.import(&handle));
    assert_eq!(historical["historical"], true);
    assert_eq!(historical["retention_state"], "released");
    assert!(!f.receiver.repositories.pool_path(&f.repo).exists());
    let new_export = f.export(&f.second, &f.first, json!([]), false);
    let (new_handle, _) = f.stage(&new_export);
    let rehydrate = f.import(&new_handle);
    f.receiver
        .root
        .inject_write_failure_after(3, aq_git_core::state::WriteFault::BeforeRename);
    assert_eq!(
        f.receiver.execute(rehydrate.clone()).error.unwrap().code,
        "io_error"
    );
    assert!(f.receiver.repositories.pool_path(&f.repo).exists());
    let interrupted: Value = f
        .receiver
        .root
        .read_json(format!("repositories/{}/pool.json", f.repo))
        .unwrap()
        .unwrap();
    assert_eq!(interrupted["lifecycle"], "absent");
    let new_receipt = run(&f.receiver, rehydrate);
    assert_eq!(new_receipt["source"], oid(&f.second));
    let pool: Value = f
        .receiver
        .root
        .read_json(format!("repositories/{}/pool.json", f.repo))
        .unwrap()
        .unwrap();
    assert_eq!(pool["storage_generation"], 3);
    assert_eq!(pool["lifecycle"], "present");
}
#[test]
fn validated_snapshot_enters_pool_without_server_repack_and_checks_advertisement() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let script = f._temp.path().join("git-no-local-repack");
    fs::write(&script,"#!/bin/sh\nis_fetch=0\nis_unbundle=0\nfor arg do [ \"$arg\" != fetch ] || is_fetch=1; [ \"$arg\" != unbundle ] || is_unbundle=1; done\nif [ \"$is_fetch\" = 1 ]; then for arg do case \"$arg\" in */artifacts/quarantine/*) case \"$arg\" in *.bundle) ;; *) printf 'quarantine upload-pack is unavailable\\n' >&2; exit 77;; esac;; esac; done; fi\nif [ \"$is_unbundle\" = 1 ]; then printf 'unbundle\\n' >>\"${0}.calls\"; if [ -f \"${0}.bad-advertisement\" ]; then printf 'unexpected source or ref\\n'; exit 0; fi; fi\nexec /usr/bin/git \"$@\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    f.receiver = RepositoryService::from_state_root_with_git(
        f.receiver.root.as_ref().clone(),
        script.clone(),
    )
    .unwrap();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, _) = f.stage(&export);
    let receipt = run(&f.receiver, f.import(&handle));
    assert_eq!(receipt["source"], oid(&f.second));
    assert_eq!(
        fs::read_to_string(script.with_file_name("git-no-local-repack.calls")).unwrap(),
        "unbundle\n"
    );
    let next = f.export(&f.second, &f.first, json!([]), false);
    let (next_handle, _) = f.stage(&next);
    fs::write(
        script.with_file_name("git-no-local-repack.bad-advertisement"),
        "fixture switch",
    )
    .unwrap();
    let error = f.receiver.execute(f.import(&next_handle)).error.unwrap();
    assert_eq!(
        error.code, "git_failed",
        "unexpected import error: {error:?}"
    );
    assert!(error.message.contains("unexpected source or ref"));
    assert!(
        f.receiver
            .root
            .retention(next["artifact_handle"].as_str().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn interrupted_eviction_fences_import_before_intent_and_preserves_generations() {
    use aq_git_core::state::{OperationStatus, WriteFault};
    for (writes, point, expected_phase) in [
        (2, WriteFault::AfterRename, "evicting"),
        (3, WriteFault::BeforeRename, "evicting"),
        (4, WriteFault::BeforeRename, "absent"),
    ] {
        let f = Fixture::new();
        let export = f.export(&f.second, &f.first, json!([]), false);
        let (handle, _) = f.stage(&export);
        let evict = request(
            &f.repo,
            "repo.cache-evict",
            json!({"expected_storage_generation":1,"reconstructible":true,"reason":"interrupted eviction before new import"}),
        );
        let eviction = evict.operation_id.clone().unwrap();
        f.receiver.root.inject_write_failure_after(writes, point);
        assert_eq!(
            f.receiver.execute(evict.clone()).error.unwrap().code,
            "io_error"
        );
        let before: Value = f
            .receiver
            .root
            .read_json(format!("repositories/{}/pool.json", f.repo))
            .unwrap()
            .unwrap();
        assert_eq!(before["lifecycle"], expected_phase);
        assert_eq!(before["operation_id"], eviction);
        let pool_existed = f.receiver.repositories.pool_path(&f.repo).exists();
        let import = f.import(&handle);
        let import_id = import.operation_id.clone().unwrap();
        let spawns = f.receiver.repositories.runner.metrics().spawns;
        let response = f.receiver.execute(import.clone());
        let error = response.error.unwrap();
        assert_eq!(error.code, "recovery_blocked");
        assert_eq!(error.details["blocking_operation_id"], eviction);
        assert_eq!(f.receiver.repositories.runner.metrics().spawns, spawns);
        assert!(f.receiver.root.operation(&import_id).unwrap().is_none());
        assert_eq!(
            f.receiver.repositories.pool_path(&f.repo).exists(),
            pool_existed
        );
        assert_eq!(
            f.receiver
                .root
                .read_json::<Value>(format!("repositories/{}/pool.json", f.repo))
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            f.receiver
                .root
                .operation(&eviction)
                .unwrap()
                .unwrap()
                .status,
            OperationStatus::Pending
        );
        let done = run(&f.receiver, evict);
        assert_eq!(done["storage_generation"], 2);
        assert_eq!(done["lifecycle"], "absent");
        let imported = run(&f.receiver, import);
        assert_eq!(imported["source"], oid(&f.second));
        let pool: Value = f
            .receiver
            .root
            .read_json(format!("repositories/{}/pool.json", f.repo))
            .unwrap()
            .unwrap();
        assert_eq!(pool["storage_generation"], 3);
        assert_eq!(pool["lifecycle"], "present");
    }
}
#[test]
fn malformed_or_missing_pool_state_is_not_silently_rehydrated() {
    let f = Fixture::new();
    let export = f.export(&f.second, &f.first, json!([]), false);
    let (handle, _) = f.stage(&export);
    let path = format!("repositories/{}/pool.json", f.repo);
    for malformed in [
        json!({"format_version":99,"storage_generation":1,"lifecycle":"present"}),
        json!({"format_version":1,"storage_generation":0,"lifecycle":"present"}),
        json!({"format_version":1,"storage_generation":"one","lifecycle":"present"}),
        json!({"format_version":1,"storage_generation":1,"lifecycle":"future"}),
    ] {
        f.receiver.root.write_json(&path, &malformed).unwrap();
        let import = f.import(&handle);
        let operation = import.operation_id.clone().unwrap();
        let before = f.receiver.repositories.runner.metrics().spawns;
        assert_eq!(
            f.receiver.execute(import).error.unwrap().code,
            "state_incompatible"
        );
        assert!(f.receiver.root.operation(&operation).unwrap().is_none());
        assert_eq!(f.receiver.repositories.runner.metrics().spawns, before);
        assert_eq!(
            f.receiver.root.read_json::<Value>(&path).unwrap().unwrap(),
            malformed
        );
    }
    std::fs::remove_file(f.receiver.root.path().join(&path)).unwrap();
    let import = f.import(&handle);
    let operation = import.operation_id.clone().unwrap();
    assert_eq!(
        f.receiver.execute(import).error.unwrap().code,
        "recovery_blocked"
    );
    assert!(f.receiver.root.operation(&operation).unwrap().is_none());
    assert!(f.receiver.root.read_json::<Value>(&path).unwrap().is_none());
}
