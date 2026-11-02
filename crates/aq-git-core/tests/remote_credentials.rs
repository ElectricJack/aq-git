mod support;
use aq_git_core::{RepositoryService, git::GitRunner};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use support::{request, success};

fn assert_tree_has_no_secret(path: &Path, secret: &[u8]) {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            assert_tree_has_no_secret(&entry.path(), secret);
        } else {
            let bytes = fs::read(entry.path()).unwrap();
            assert!(
                !bytes.windows(secret.len()).any(|part| part == secret),
                "secret in persisted state"
            );
        }
    }
}

#[test]
fn credential_material_never_logged() {
    let temp = tempfile::tempdir().unwrap();
    let token = temp.path().join("token");
    let secret = "ghp_synthetic_D0_token_do_not_log";
    fs::write(&token, secret).unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    service.root.write_json("credential_profiles.json", &json!({"token":{"kind":"https_token", "remote_url":"https://example.test/repo.git", "token_path":token}})).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    let mut register = request(
        &repo,
        "repo.register",
        json!({"identity":"fixture", "mode":"central", "remote_url":"https://example.test/repo.git", "credential_profile":"token", "allowed_refs":["refs/heads/main"]}),
    );
    register.expected_binding_generation = Some(0);
    success(&service, register);
    let wrapper = temp.path().join("git-diagnostic");
    fs::write(&wrapper,format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nenv >> '{}'\ncat '{}' >&2\nprintf '\\nfatal: Authentication failed\\n' >&2\nexit 128\n",temp.path().join("invocation").display(),temp.path().join("invocation").display(),token.display())).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mut service = service;
    service.repositories.runner = GitRunner::new(wrapper);
    let sync = request(&repo, "repo.sync", json!({}));
    let response = service.execute(sync.clone());
    assert_eq!(response.error.as_ref().unwrap().code, "remote_auth_failed");
    assert!(!serde_json::to_string(&response).unwrap().contains(secret));
    assert!(!format!("{response:?}").contains(secret));
    assert!(
        !fs::read_to_string(temp.path().join("invocation"))
            .unwrap()
            .contains(secret)
    );
    assert_eq!(
        service.execute(sync).error.unwrap().code,
        "remote_auth_failed"
    );
    assert_tree_has_no_secret(service.root.path(), secret.as_bytes());
}

#[test]
fn invalid_profiles_refuse_without_disclosing_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let token = temp.path().join("token");
    fs::write(&token, "synthetic_token").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    for (profile, remote, mode) in [
        (
            json!({"kind":"https_token","remote_url":"https://example.test/a","token_path":token}),
            "https://example.test/b",
            "central",
        ),
        (
            json!({"kind":"https_token","remote_url":"https://example.test/a","token_path":token}),
            "https://example.test/a",
            "worker",
        ),
        (
            json!({"kind":"synthetic_secret_in_invalid_kind","token_path":token}),
            "https://example.test/a",
            "central",
        ),
        (
            json!({"kind":"https_token","remote_url":"https://example.test/a","token_path":"relative"}),
            "https://example.test/a",
            "central",
        ),
    ] {
        service
            .root
            .write_json("credential_profiles.json", &json!({"token":profile}))
            .unwrap();
        let repo = uuid::Uuid::new_v4().to_string();
        let mut register = request(
            &repo,
            "repo.register",
            json!({"identity":"fixture","mode":mode,"remote_url":remote,"credential_profile":"token"}),
        );
        register.expected_binding_generation = Some(0);
        let response = service.execute(register);
        assert!(!response.ok);
        assert!(
            !serde_json::to_string(&response)
                .unwrap()
                .contains("synthetic_secret")
        );
        assert!(
            service
                .root
                .read_json::<serde_json::Value>(format!("repositories/{repo}/binding.json"))
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(service.repositories.runner.metrics().spawns, 0);
}

#[test]
fn credential_file_permissions_and_symlinks_refuse() {
    let temp = tempfile::tempdir().unwrap();
    let token = temp.path().join("token");
    fs::write(&token, "synthetic_token").unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&token, &link).unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    for path in [&token, &link] {
        fs::set_permissions(
            &token,
            fs::Permissions::from_mode(if path == &token { 0o644 } else { 0o600 }),
        )
        .unwrap();
        service.root.write_json("credential_profiles.json", &json!({"token":{"kind":"https_token","remote_url":"https://example.test/a","token_path":path}})).unwrap();
        let mut register = request(
            &uuid::Uuid::new_v4().to_string(),
            "repo.register",
            json!({"identity":"fixture","mode":"central","remote_url":"https://example.test/a","credential_profile":"token"}),
        );
        register.expected_binding_generation = Some(0);
        assert_eq!(
            service.execute(register).error.unwrap().code,
            "invalid_configuration"
        );
    }
}

#[test]
fn binding_updates_revalidate_profile_scope_and_replay() {
    let temp = tempfile::tempdir().unwrap();
    let token = temp.path().join("token");
    fs::write(&token, "synthetic_token").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    service.root.write_json("credential_profiles.json", &json!({
        "first":{"kind":"https_token","remote_url":"https://example.test/first","token_path":token},
        "second":{"kind":"https_token","remote_url":"https://example.test/second","token_path":token}
    })).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    let mut register = request(
        &repo,
        "repo.register",
        json!({"identity":"fixture","mode":"central","remote_url":"https://example.test/first","credential_profile":"first"}),
    );
    register.expected_binding_generation = Some(0);
    let mut binding = success(&service, register);
    binding["binding_generation"] = json!(2);
    binding["credential_profile"] = json!("second");
    let mut update = request(
        &repo,
        "repo.binding-update",
        json!({"binding":binding,"reason":"switch credential and remote"}),
    );
    assert_eq!(
        service.execute(update.clone()).error.unwrap().code,
        "invalid_configuration"
    );
    update.operation_id = Some(uuid::Uuid::new_v4().to_string());
    update.params["binding"]["remote_url"] = json!("https://example.test/second");
    let first = success(&service, update.clone());
    assert_eq!(first, success(&service, update));
    assert_eq!(
        service
            .repositories
            .binding(&repo)
            .unwrap()
            .credential_profile
            .as_deref(),
        Some("second")
    );
}
