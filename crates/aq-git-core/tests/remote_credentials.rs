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

#[test]
fn operator_profile_file_registers_labels_and_reports_them_without_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    fs::create_dir(&vault).unwrap();
    let secret = "github_pat_synthetic_named_profile_do_not_report";
    let token = vault.join("token");
    fs::write(&token, secret).unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let key = vault.join("deploy-key");
    fs::write(&key, "synthetic private key").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let hosts = vault.join("known_hosts");
    fs::write(&hosts, "github.com ssh-ed25519 AAAA\n").unwrap();
    fs::set_permissions(&hosts, fs::Permissions::from_mode(0o644)).unwrap();
    let config = vault.join("credential_profiles.json");
    fs::write(&config, serde_json::to_vec(&json!({
        "gh-token":{"kind":"https_token","remote_url":"https://github.com/example/project.git","token_path":token},
        "gh-deploy":{"kind":"ssh_deploy_key","remote_url":"ssh://git@github.com/example/project.git","private_key_path":key,"known_hosts_path":hosts}
    })).unwrap()).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        RepositoryService::open(temp.path().join("relative-check"))
            .unwrap()
            .with_credential_profiles("relative.json".into())
            .is_err()
    );
    let service = RepositoryService::open(temp.path().join("state"))
        .unwrap()
        .with_credential_profiles(config.clone())
        .unwrap();
    // The state-root file is ignored once an operator file is selected.
    service.root.write_json("credential_profiles.json", &json!({"state-only":{"kind":"https_token","remote_url":"https://github.com/example/project.git","token_path":token}})).unwrap();
    let capabilities = service.capabilities();
    assert_eq!(capabilities["git"]["credential_profiles"], true);
    assert_eq!(
        capabilities["git"]["credential_profile_labels"],
        json!(["gh-deploy", "gh-token"])
    );
    assert_eq!(
        capabilities["git"]["credential_profile_config"],
        "available"
    );
    let reported = capabilities.to_string();
    for hidden in [secret, vault.to_str().unwrap()] {
        assert!(!reported.contains(hidden), "capabilities exposed {hidden}");
    }
    let register = |label: &str, remote: &str| {
        let mut register = request(
            &uuid::Uuid::new_v4().to_string(),
            "repo.register",
            json!({"identity":"fixture","mode":"central","remote_url":remote,"credential_profile":label}),
        );
        register.expected_binding_generation = Some(0);
        register
    };
    let binding = success(
        &service,
        register("gh-token", "https://github.com/example/project.git"),
    );
    assert_eq!(binding["credential_profile"], "gh-token");
    let ssh = success(
        &service,
        register("gh-deploy", "ssh://git@github.com/example/project.git"),
    );
    assert_eq!(ssh["credential_profile"], "gh-deploy");
    for (label, remote, code) in [
        // A profile is bound to its transport scheme and exact remote.
        (
            "gh-deploy",
            "https://github.com/example/project.git",
            "invalid_configuration",
        ),
        (
            "gh-token",
            "ssh://git@github.com/example/project.git",
            "invalid_configuration",
        ),
        (
            "gh-token",
            "https://github.com/example/other.git",
            "invalid_configuration",
        ),
        (
            "state-only",
            "https://github.com/example/project.git",
            "unsupported_feature",
        ),
        (
            "gh-token",
            "https://x-access-token:inline@github.com/example/project.git",
            "invalid_request",
        ),
    ] {
        let response = service.execute(register(label, remote));
        assert_eq!(response.error.unwrap().code, code, "{label} {remote}");
    }
    assert_tree_has_no_secret(service.root.path(), secret.as_bytes());
    // A writable operator file is unusable: capabilities report no labels and
    // registration refuses rather than trusting it.
    fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
    let capabilities = service.capabilities();
    assert_eq!(capabilities["git"]["credential_profile_labels"], json!([]));
    assert_eq!(
        capabilities["git"]["credential_profile_config"],
        "invalid_configuration"
    );
    assert_eq!(
        service
            .execute(register(
                "gh-token",
                "https://github.com/example/project.git"
            ))
            .error
            .unwrap()
            .code,
        "invalid_configuration"
    );
}
