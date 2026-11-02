# Authenticated Git remotes Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task-by-task in the assigned isolated worktree. The task already authorizes execution and AQ owns delivery.

**Goal:** Fetch and publish using named file-backed SSH deploy keys or HTTPS tokens, including creating branches and typed authentication failures.

**Architecture:** Administrator configuration `credential_profiles.json` in the private state root maps names to profiles containing only paths and remote scope. Bindings select a name; a remote-only runner path prepares SSH options or a one-invocation HTTPS helper without exposing material in arguments, environment, diagnostics or journals. Existing publication retains its durable no-redispatch boundary and uses an all-zero expected-old SHA-1 for absent targets.

**Tech Stack:** Rust 1.96, existing serde/serde_json, bounded GitRunner, Git, OpenSSH, temporary local remote fixtures.

**Spec:** Task crisp-meadow-36.13 (D0), complete task description from `aq prime`; owning API documentation in `docs/api.md`.

## Global Constraints

- Each repository builds and tests alone; Rust uses the committed lockfile with `--locked`.
- No secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions. Credentials remain central and outside worker bindings.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- Run formatting, locked clippy with warnings denied, and locked tests for the touched workspace; record evidence in `docs/implementation/D0.md`.
- Only the integration owner delivers; push the assigned branch with `aq git push`.
- Do not edit `docs/specs/*`; do not implement D2 secret storage/rotation or D1b publication policy/PR creation.
- Named test filters must execute an actual passing test. No adversarial review is marked for D0.

### Task 1: File-backed credentials and authenticated remote operations

**Files:** Create `crates/aq-git-core/src/git/credentials.rs` and `crates/aq-git-core/tests/remote_credentials.rs`; modify `git/mod.rs`, `git/runner.rs`, `repository/mod.rs`, `repository/binding.rs`, `service.rs`.

**Interfaces:** Profile JSON is a map of names to tagged entries: `kind=ssh_deploy_key`, `remote_url`, `private_key_path`, `known_hosts_path`; or `kind=https_token`, `remote_url`, `token_path`. Exact URL scope prevents reusing a credential for a different repository. `RepositoryManager::remote_run(binding, args, timeout)` invokes bounded Git and returns sanitized output or `remote_auth_failed`.

- [x] Write real SSH fixture tests before implementation, starting with:

```rust
#[test]
fn ssh_deploy_key_profile_fetches_and_publishes() {
    let fixture = SshFixture::new();
    let sync = fixture.sync();
    assert!(sync.ok, "{:?}", sync.error);
    fixture.publish_existing_and_new_branch();
}
```

The fixture generates independent host/client keys, binds an unprivileged loopback OpenSSH server, pins its host key, and serves a temporary bare Git repository. Missing sshd fails with setup instructions instead of skipping. Add wrong-key and wrong-host-key refusals.

- [x] Add `credential_material_never_logged` using a diagnostic-emitting fake Git executable and a synthetic file-backed token; assert captured child arguments/environment contain no material, and responses, errors and persisted journals contain none. Add `remote_auth_failure_is_typed` against denied SSH authentication. Add HTTPS helper protocol checks for get/store/erase and exact URL matching.
- [x] Run `cargo test --locked --workspace --test remote_credentials`; confirm tests fail on named-profile rejection.
- [x] Implement typed profiles loaded from private administrator state. Validate absolute file paths, profile names, transport and exact remote scope. Parse failures use fixed error messages. Worker bindings still refuse profiles. Unknown names refuse explicitly.
- [x] Add a private remote runner configuration path (do not widen the public extra-env allowlist). SSH uses `/usr/bin/ssh -F /dev/null`, `IdentitiesOnly=yes`, `IdentityAgent=none`, `BatchMode=yes`, `StrictHostKeyChecking=yes`, explicit identity/known-hosts paths and no global known-hosts fallback. HTTPS invokes an invocation-scoped shell credential helper that reads the token file only on `get`, matches protocol/host/path and never stores. Disable HTTP redirects and inherit no external helper/tracing environment.
- [x] Use the remote path for sync advertisement and fetch. Discard remote diagnostics before exposing responses and map authentication denial to `remote_auth_failed`; retain timeout/limit error codes.
- [x] Run the remote tests and existing repository/service/runner suites. Commit the coherent implementation.

### Task 2: Publication, absent-ref leases, and evidence

**Files:** Modify `crates/aq-git-core/src/publication.rs`, `crates/aq-git-core/tests/publication.rs`, `docs/api.md`, `docs/operations.md`, `docs/development.md`, `docs/release-status.md`; create `docs/implementation/D0.md`.

**Interfaces:** `publish.expected_old` keeps its existing typed SHA-1 shape; forty zeroes require an absent branch. Observations, pushes and reconciliation use the binding's profile. A new branch uses an exact absent-ref lease and recognizes Git's `*` porcelain acknowledgement.

- [x] Add failing publication tests for absent target creation, replay, existing-target rejection and a racing creator. Add auth denial checks on both preflight and push while preserving uncertain transport outcomes.

```rust
let mut create = fixture.publish();
create.params["target"] = json!("refs/heads/new");
create.params["expected_old"] = oid(&"0".repeat(40));
assert_eq!(success(fixture.service.execute(create.clone()))["outcome"], "applied");
assert_eq!(success(fixture.service.execute(create))["outcome"], "applied");
```

- [x] Probe exact target presence with authenticated `ls-remote`; fetch present targets before ancestry checks. Skip expected-old ancestry only for the absent sentinel. Preserve dispatched intent, unknown-effect fencing, allowlists and target holds. Never automatically retry a dispatched push.
- [x] Run each named acceptance command verbatim, plus focused related suites; verify nonzero test counts. Run the workspace definition-of-done checks and docs validation, recording exact results and limits.
- [x] Document administrator profile setup, permission expectations, host pinning, HTTPS token helper, new-branch sentinel, errors, local fixture dependencies and the distinction between local qualification and live GitHub testing.
- [x] Reconcile scope and deliverables. Squash task implementation to one commit preserving the tree and attribution, merge the supervisor-approved `origin/main` (the repository has no `origin/dev`), rerun touched tests if needed, guarded-push, record final SHA, close task with evidence and drain acknowledgement.

Execution note: the acceptance tests live in `tests/publication.rs`, with shared real SSH/TLS fixtures under `tests/support/`; credential isolation and refusal cases live in `tests/remote_credentials.rs`. This keeps the existing candidate/publication fixture intact. Final integration uses the supervisor-authorized main baseline; see D0 evidence.
