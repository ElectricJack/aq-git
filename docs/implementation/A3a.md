# A3a: aq-git v0.2.0 release evidence

Task: `eager-grove-45.2`. Baseline: `e05d4e6` (origin/main at task start).
The implementation plan was committed as `f5d1397` before editing release inputs.

## Current disposition

Qualification of the original runtime baseline passed both Git matrices and both scripts/check-all runs. Release publication is **pending owner**: supervisor message `msg-fd7181f07599404b916c3cbf32554fed` explicitly forbids this worker from pushing main or creating the tag/release. No v0.2.0 publication or successful download verification is claimed here. The final release build record must identify the exact source revision; a task-branch checkpoint alone is not default-branch delivery.

## Changes and boundaries

`Cargo.toml` workspace version changes from 0.1.0 to 0.2.0. `cargo check --workspace --offline` regenerated `Cargo.lock`; its only changes are the versions of aq-git-cli, aq-git-core and aq-git-protocol. Protocol, state/artifact formats, merge profile, runtime source and tests remain unchanged. No aq-server pin or Dockerfile is edited.

A release acceptance assertion using Python `tomllib` failed against 0.1.0 before the edit, then passed with workspace version 0.2.0 and exactly three locked aq-git packages at 0.2.0.

## Environment and verified inputs

- Host: x86_64 Linux, WSL2 kernel `6.18.33.2-microsoft-standard-WSL2`; Ubuntu glibc 2.39.
- Host Git: `/usr/bin/git`, `git version 2.43.0`.
- Rust: `rustc 1.96.1 (31fca3adb 2026-06-26)`, host/target `x86_64-unknown-linux-gnu`; Cargo `1.96.1 (356927216 2026-06-26)`.
- Docker: 29.3.1.
- Builder: `rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663`. Docker pull and RepoDigests verified the required digest.
- Git 2.56.0 input: `https://www.kernel.org/pub/software/scm/git/git-2.56.0.tar.xz`, independently verified SHA-256 `26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3`.

## Qualification method

Cargo runs are foreground task work with `CARGO_BUILD_JOBS=2`. `aq test --help` exposes pytest arguments, so the task's explicit Rust acceptance commands are run with Cargo directly, with bounded jobs/test threads. Long commands are preceded by `aq task heartbeat eager-grove-45.2`.

The production defaults and several fixtures explicitly use `/usr/bin/git` (`crates/aq-git-core/src/service.rs`, `crates/aq-git-cli/src/main.rs` and integration tests). PATH alone cannot establish Git 2.56 engine coverage. The 2.56 qualification must therefore also bind the verified executable over `/usr/bin/git` inside the bookworm test container, with its installation tree mounted at its compiled prefix. The source is read-only, the target directory is separate, and only the Cargo registry is mounted; host credentials and daemon environment are not passed into the container.

Git was built in the pinned image rather than installing missing host development packages. The first host build failed on missing OpenSSL headers. The first container build reached git-gui and failed because Tcl was unavailable. The completed build uses `NO_GETTEXT=YesPlease NO_TCLTK=YesPlease`, retaining the normal Git command-line, merge and transport implementation.

## Qualification results

| Command | Environment | Result |
| --- | --- | --- |
| Release-version assertion | Host | Expected failure before bump; pass after bump |
| `cargo check --workspace --offline` | Rust 1.96.1 | Pass |
| `cargo fmt --check` | Rust 1.96.1 | Pass |
| `cargo clippy --locked --all-targets -- -D warnings` | Rust 1.96.1 | Pass |
| `python3 -m unittest discover -s benchmarks -p 'test_*.py'` | Host | Pass, 8 tests |
| `scripts/check-docs` | Isolated checkout | Fails on an unchanged cross-repository link; see below |
| `cargo test --locked --workspace` | Host Git 2.43.0, two test threads | Initial failure in the one-second timeout/recovery case; retained below |
| `cargo test --locked --workspace` | Host Git 2.43.0, one test thread | Pass, 110 tests |
| `cargo test --locked --workspace` | Git 2.56.0 at both PATH and `/usr/bin/git`, pinned bookworm container, one test thread | Pass, 110 tests |
| `scripts/check-all` | Host Git 2.43.0, one test thread | Pass, 110 tests |
| `scripts/check-all` | Git 2.56.0 at both PATH and `/usr/bin/git`, pinned bookworm container, one test thread | Pass, 110 tests |
| `cargo test --locked -p aq-git-core --test candidate frozen_merge_conflict_replay_and_independent_retention -- --exact` | Host Git 2.43.0, one test thread | Pass, 1 test, assertions unchanged |

The first full run failed at `crates/aq-git-core/tests/candidate.rs:373`, asserting that the left source retention handle's dependencies are empty after a deliberately timed-out candidate creation. The exact case passed alone unchanged (8.77 seconds). The complete serialized host rerun then passed all 110 tests. The independent Git 2.56.0 suite also passed all 110 tests; the passing isolated case was not substituted for either full qualification run.

The docs failure is the existing link in `docs/aq-integration.md` to `../../aq-server/docs/architecture/0001-ecosystem-and-releases.md`. The same link exists at baseline e05d4e6. This checkout has no sibling aq-server; the checker treats that as a broken local link. Follow-up `vivid-bridge-93` owns standalone documentation-check policy. No assertion was skipped or weakened, and the failed check is not reported as a pass.

## Publication authority

AQ prime requires integration-owner delivery and prohibits independent main merges/pushes. It also requires an `origin/dev` merge before close, but this repository initially has only origin/main. The supported `aq git` worker surface has no tag/release publisher; `aq integration delegate-publication` is an operator/owning-supervisor command. Pre-close integration/publication routing was requested through `msg-d1061d8d4bec48bdb634090560eab619` (project supervisor) and `msg-7b97bfed9b32477eaa4abf753c3cfead` (dashboard). No capability denial will be bypassed.

## Remaining limits

A versioned release does not certify P1–P7 performance, the full compatibility/persisted-boundary crash matrix, host power-loss durability, hosted HTTPS/SSH transport or credentials, or deployed AQ integration. The initial release has no previous compatible release for an upgrade/rollback rehearsal; that gate begins with the second compatible release. Existing repository-profile restrictions remain in force. The manifest's `development_not_certified` and `not_attested` fields must remain truthful; the supplemental build record ties this specific build to its locked source and pinned builder without asserting those broader certifications.

A3a is not marked `review: adversarial` in the task contract and uses direct integration. No cross-family review is required.

## Reproduction commands

From the aq-git worktree, the input and Git build commands were:

```bash
mkdir -p .local/A3a
curl --fail --location --retry 2 --output .local/A3a/git-2.56.0.tar.xz https://www.kernel.org/pub/software/scm/git/git-2.56.0.tar.xz
echo '26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3  .local/A3a/git-2.56.0.tar.xz' | sha256sum -c
tar -xJf .local/A3a/git-2.56.0.tar.xz -C .local/A3a
docker pull rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
docker run --rm --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$PWD/.local/A3a,dst=$PWD/.local/A3a" \
  --workdir "$PWD/.local/A3a/git-2.56.0" \
  rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 \
  make -j2 "prefix=$PWD/.local/A3a/git-2.56" NO_GETTEXT=YesPlease NO_TCLTK=YesPlease all install
.local/A3a/git-2.56/bin/git --version
```

The host qualification uses the system Git ahead of other PATH entries:

```bash
PATH="/usr/bin:$PATH" git --version
PATH="/usr/bin:$PATH" CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1 cargo test --locked --workspace
PATH="/usr/bin:$PATH" CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1 scripts/check-all
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy --locked --all-targets -- -D warnings
python3 -m unittest discover -s benchmarks -p 'test_*.py'
scripts/check-docs
```

The full 2.56 container command (run once with `cargo test --locked --workspace`, then with `scripts/check-all`) is:

```bash
mkdir -p .local/A3a/container-target
docker run --rm --user "$(id -u):$(id -g)" \
  -e RUSTUP_TOOLCHAIN=1.96.1 -e CARGO_BUILD_JOBS=2 -e RUST_TEST_THREADS=1 -e CARGO_NET_OFFLINE=true \
  -e "PATH=$PWD/.local/A3a/git-2.56/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
  --mount "type=bind,src=$PWD,dst=$PWD,readonly" \
  --mount "type=bind,src=$PWD/.local/A3a/container-target,dst=$PWD/target" \
  --mount 'type=bind,src=/home/jkern/.cargo/registry,dst=/usr/local/cargo/registry,readonly' \
  --mount "type=bind,src=$PWD/.local/A3a/git-2.56/bin/git,dst=/usr/bin/git,readonly" \
  --workdir "$PWD" \
  rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 \
  sh -c 'git --version && /usr/bin/git --version && rustc -Vv && cargo test --locked --workspace'
```

The local registry mount supports offline locked builds and contains no host Cargo configuration or credentials. On another host, use its registry cache path or allow Cargo to fetch the same locked public dependencies into an isolated Cargo home.

## Integration source change discovered during final preparation

The required `aq git pull --branch main` fetched `f5af0b805db8270ccac9efb48787855c5217bc99` (D0 credential profiles, 27 changed files) after the original runtime had been qualified. Its merge stopped safely because release-status had uncommitted edits. The 110-test results above qualify the original e05d4e6 runtime with the version bump; they do **not** qualify the later D0 implementation. The supervisor has been asked whether the release should retain that immutable qualified snapshot or include and requalify new main. No earlier qualification will be relabelled as evidence for changed code.
