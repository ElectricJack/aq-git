#![allow(dead_code)]
use aq_git_core::RepositoryService;
use aq_git_protocol::Request;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub service: RepositoryService,
    pub repository: String,
    pub source: PathBuf,
    pub remote: PathBuf,
    pub base: String,
}
pub fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}
pub fn request(repo: &str, operation: &str, params: Value) -> Request {
    Request {
        protocol_version: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        operation: operation.into(),
        repository_id: Some(repo.into()),
        operation_id: Some(uuid::Uuid::new_v4().to_string()),
        expected_binding_generation: Some(1),
        params,
        deadline_ms: None,
    }
}
pub fn oid(hex: &str) -> Value {
    json!({"algorithm":"sha1","hex":hex})
}
pub fn success(service: &RepositoryService, request: Request) -> Value {
    let response = service.execute(request);
    assert!(response.ok, "{:?}", response.error);
    response.result.unwrap()
}
impl Fixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("file.txt"), "base\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        let remote = temp.path().join("remote.git");
        git(
            temp.path(),
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        let service = RepositoryService::open(temp.path().join("state")).unwrap();
        let repository = uuid::Uuid::new_v4().to_string();
        let mut register = request(
            &repository,
            "repo.register",
            json!({"binding":{"identity":"fixture","mode":"central","allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"],"remote_url":remote,"allow_file_transport":true}}),
        );
        register.expected_binding_generation = Some(0);
        success(&service, register);
        success(
            &service,
            request(
                &repository,
                "repo.sync",
                json!({"refs":["refs/heads/main"]}),
            ),
        );
        Self {
            temp,
            service,
            repository,
            source,
            remote,
            base,
        }
    }
    pub fn create_workspace(&self) -> Value {
        success(
            &self.service,
            request(
                &self.repository,
                "workspace.create",
                json!({"base":oid(&self.base)}),
            ),
        )
    }
}
