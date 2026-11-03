# A3a: aq-git v0.2.0 release evidence

Task: `eager-grove-45.2`. Baseline: `e05d4e6` (origin/main at task start).
The implementation plan was committed as `f5d1397` before editing release inputs.

## Current disposition

**Published:** [aq-git v0.2.0](https://github.com/ElectricJack/aq-git/releases/tag/v0.2.0), source/tag revision `e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`. The six release assets were downloaded from GitHub and independently verified against their sidecars and preserved build outputs. Both required correctness commands passed 121 tests with each Git version (2.43.0 and 2.56.0). The [publication receipt](A3a-checks/publication-receipt.json) records exact asset URLs, digests and sizes. AQ's direct-mode close owns integration into main; post-close ancestry is checked separately. Earlier publication holds below are historical and were superseded by the owner decision.

## Changes and boundaries

`Cargo.toml` workspace version changes from 0.1.0 to 0.2.0. `cargo check --workspace --offline` regenerated `Cargo.lock`; its only changes are the versions of aq-git-cli, aq-git-core and aq-git-protocol. A3a introduces no runtime or test changes of its own, and no aq-server pin or Dockerfile changes. The required merge of origin/main brings D0 remote credentials from its already delivered task; its runtime/protocol changes are qualified afresh rather than attributed to A3a.

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

## Original runtime qualification (historical)

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

The [post-merge docs-check repetition](A3a-checks/merged-check-docs.log) exits 1 on the same unchanged link. The docs failure is the existing link in `docs/aq-integration.md` to `../../aq-server/docs/architecture/0001-ecosystem-and-releases.md`. The same link exists at baseline e05d4e6. This checkout has no sibling aq-server; the checker treats that as a broken local link. Follow-up `vivid-bridge-93` owns standalone documentation-check policy. No assertion was skipped or weakened, and the failed check is not reported as a pass.

## Original publication authority (historical)

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

The required `aq git pull --branch main` fetched `f5af0b805db8270ccac9efb48787855c5217bc99` (D0 credential profiles, 27 changed files) after the original runtime had been qualified. Its merge stopped safely because release-status had uncommitted edits. The 110-test results above qualify the original e05d4e6 runtime with the version bump; they do **not** qualify the later D0 implementation. Supervisor guidance `msg-f27ff278de7a473da059702b7a23e7a5` requires the default candidate to include D0 and be requalified. Merge `bde7102` includes origin/main f5af0b8 and resolves only the documentation conflict, preserving D0 transport evidence and A3a release limits. The reference artifacts at the preparation directory root are explicitly marked DO-NOT-PUBLISH; the replacement belongs under merged-candidate/. No earlier qualification is relabelled as evidence for changed code.


## Merged candidate qualification method

The merged runtime baseline is `f5af0b805db8270ccac9efb48787855c5217bc99`, with the v0.2.0 version bump. Post-merge targeted publication and credential validation passed **18 tests** using:

```bash
AQ_TEST_SSHD=/tmp/aq-d0-sshd/extracted/usr/sbin/sshd \
LD_LIBRARY_PATH=/tmp/aq-d0-sshd/extracted/usr/lib/x86_64-linux-gnu \
CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1 \
cargo test --locked -p aq-git-core --test publication --test remote_credentials
```

The host's missing sshd was already supplied by the D0 worker as extracted Ubuntu packages at that path (see D0 evidence). No system service is installed or started. Fresh host matrix commands use those two fixture environment variables, `/usr/bin` first on PATH, `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1`, and run `cargo test --locked --workspace` followed by `scripts/check-all`.

For Git 2.56.0, a **test-only** image derived from the exact builder adds OpenSSH server, OpenSSL and Python for loopback fixtures and an unlocked uid-1000 aqtest account. [The Dockerfile](A3a-checks/qualification.Dockerfile) records the recipe. The actual image ID is `sha256:a244094e2b3118966392fc56feb11b24547646d0c3120579b285006686b881b9`; package versions are OpenSSH client/server `1:9.2p1-2+deb12u10`, OpenSSL `3.0.22-1~deb12u2`, Python `3.11.2-1+b1`. The test executable at both PATH and `/usr/bin/git` remains the verified upstream Git 2.56.0 installation. This test image is **not** used for the release executable: that build still uses the original unmodified pinned Rust image.

The container invocation uses the same read-only source, writable isolated target, public registry-only mount and `/usr/bin/git` mount as the earlier recipe, replaces the image with the immutable test image ID above, and mounts `.local/A3a/merged-container-logs` at `/qualification-logs`. Its exact child command is:

```bash
sh -ec 'git --version; /usr/bin/git --version; rustc -Vv; id; dpkg-query -W openssh-server openssh-client openssl python3; cargo test --locked --workspace > /qualification-logs/merged-git-2.56-cargo-test.log 2>&1; scripts/check-all > /qualification-logs/merged-git-2.56-check-all.log 2>&1'
```

Post-merge formatting and strict Clippy passed again. All eight benchmark-driver tests passed again. Changed Markdown documents have valid local links and balanced fences; the unmodified full docs checker still has the separately recorded sibling-checkout failure. This focused document check does not replace or relabel that failure.

## Merged candidate qualification results

Both foreground matrix processes exited 0. The following fresh checks cover merged runtime D0 f5af0b8 plus the release-version bump; the earlier 110-test results are historical only.

| Git | Command | Result and complete log |
| --- | --- | --- |
| 2.43.0 | `cargo test --locked --workspace` | 121 passed, [log](A3a-checks/merged-git-2.43-cargo-test.log) |
| 2.43.0 | `scripts/check-all` | 121 passed, [log](A3a-checks/merged-git-2.43-check-all.log) |
| 2.56.0 | `cargo test --locked --workspace` | 121 passed, [log](A3a-checks/merged-git-2.56-cargo-test.log) |
| 2.56.0 | `scripts/check-all` | 121 passed, [log](A3a-checks/merged-git-2.56-check-all.log) |

[Environment receipt](A3a-checks/merged-qualification-env.log), [formatting](A3a-checks/merged-fmt.log), [strict Clippy](A3a-checks/merged-clippy.log), [eight benchmark tests](A3a-checks/merged-benchmark-tests.log) and [18 focused transport/publication tests](A3a-checks/postmerge-related.log) are retained. Documentation-only qualification evidence is committed after the runtime checks; it does not change the tested Cargo inputs. The subsequent artifact section identifies the immutable clean source snapshot used by both git archive and the observed locked build.

## Prepared release assets and observed build

Qualified source revision: **`e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`**. It contains origin/main D0 `f5af0b805db8270ccac9efb48787855c5217bc99`, the v0.2.0 version bump and fresh qualification evidence. This immutable snapshot is the input to both the archive and executable. Later commits only record artifact receipts and handoff instructions; they are not substituted for the build's source revision.

All six assets are preserved outside the resettable worker slot at:

```text
/home/jkern/dev/aq-platform-agents/release-preparation/aq-git/eager-grove-45.2/merged-candidate/assets/
```

| Asset (plus same-name `.sha256`) | SHA-256 |
| --- | --- |
| `aq-git-v0.2.0-source.tar.gz` | `f64891f03e2d28263cf3af20b38123935ae08d6917699becb9d77e76af2fd3c6` |
| `aq-git-v0.2.0-linux-x86_64` | `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` |
| `aq-git-v0.2.0-build-record.json` | `94e318a2285369002c1df8ea9844ca0295ce3b867fa35691d201cc77f1c7852f` |

The [build record](A3a-build-record.json) embeds the existing development manifest, locked dependency inventory, source inventory, exact toolchain/builder/target, artifact digests and fresh qualification log digests. Its certification limits remain unchanged. The [assembly and independent verification script](A3a-checks/assemble-artifacts.py) is copied from the stable preparation directory; execute it there alongside its `source/`, `assets/`, `build.log` and `development-manifest.json`.

The archive's embedded commit ID equals the qualified revision. All 366 inventoried source files/symlinks match the git archive contents, and the canonical source inventory and binary hashes were recomputed independently. `sha256sum -c ./*.sha256` passed all three pairs. ELF inspection identifies x86-64; [version inspection](A3a-checks/merged-release-elf.log) finds maximum required GLIBC **2.34**. Running the executable inside the unmodified pinned bookworm image reports `aq-git 0.2.0`. This is local verification, not a download from GitHub.

The clean detached checkout was made with `git clone --no-hardlinks` and `git checkout --detach e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`. The exact build and manifest commands, with `release_stage` set to the stable merged-candidate directory above and run from the assigned worktree, were:

```bash
git -C "$release_stage/source" archive --format=tar --prefix=aq-git-v0.2.0/ e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a | gzip -n > "$release_stage/assets/aq-git-v0.2.0-source.tar.gz"
docker run --rm --user "$(id -u):$(id -g)" \
  -e RUSTUP_TOOLCHAIN=1.96.1 -e CARGO_BUILD_JOBS=2 -e CARGO_NET_OFFLINE=true \
  --mount "type=bind,src=$release_stage/source,dst=$PWD,readonly" \
  --mount "type=bind,src=$PWD/.local/A3a/container-target,dst=$PWD/target" \
  --mount 'type=bind,src=/home/jkern/.cargo/registry,dst=/usr/local/cargo/registry,readonly' \
  --workdir "$PWD" \
  rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 \
  sh -ec 'rustc -Vv; cargo -V; cargo build --release --locked --target x86_64-unknown-linux-gnu'
cp .local/A3a/container-target/x86_64-unknown-linux-gnu/release/aq-git "$release_stage/assets/aq-git-v0.2.0-linux-x86_64"
scripts/release-manifest --require-clean --source-dir "$release_stage/source" \
  --binary "$release_stage/assets/aq-git-v0.2.0-linux-x86_64" \
  --output "$release_stage/development-manifest.json"
python3 "$release_stage/record-assets.py"
(cd "$release_stage/assets" && sha256sum -c ./*.sha256)
```

The [complete observed build log](A3a-checks/merged-release-build.log) is retained. The source mount is read-only; writable Cargo target and public registry cache are separate, with no host credentials or daemon environment mounted. The existing target cache accelerates compilation, while Cargo evaluates changed sources and the committed lockfile; this is an observed locked build, not a claim of two independent bit-reproducible builds.

## Original owner publication handoff (historical)

The original session stopped under supervisor message `msg-fd7181f07599404b916c3cbf32554fed`, which prohibited worker publication pending the owner's decision. All six prepared assets were preserved outside the slot, and task branch `90b5a5a` was pushed. The operator later released that hold and authorized in-task tag/release publication. The recovery and published receipts below replace the original pending-publication disposition; the qualified revision and artifact bytes remain unchanged.

## Publication recovery, 2026-11-02

The latest human task comment authorizes agents to create release tags and publish releases in-task. This supersedes the original supervisor publication hold. The assigned branch was updated using `aq git pull --branch main`, bringing in the standalone docs-check policy and other documentation/configuration changes. `git diff --exit-code e75f7b4 HEAD -- Cargo.toml Cargo.lock crates` passes: the qualified Cargo inputs are unchanged. Release artifacts remain tied to their exact immutable source revision, not the newer documentation commits.

Fresh [local integrity verification](A3a-checks/publication-local-verification.json) checks all three SHA-256 pairs, the archive's embedded commit ID and all 366 inventoried files, canonical inventory digest, lockfile digest, builder digest, binary version/GLIBC requirement and all four qualification-log hashes/test counts. The build record remains byte-for-byte identical to the preserved record; its pending-publication fields describe the state at build time.

`scripts/check-docs` now [passes](A3a-checks/publication-check-docs.log) with the delivered standalone policy; eight links to absent sibling repositories are explicitly reported as unchecked. `python3 -m unittest discover -s scripts -p 'test_check_docs.py'` [passes all nine tests](A3a-checks/publication-test-check-docs.log). These results resolve the earlier isolated-checkout documentation failure without changing runtime tests or release artifacts.

## Published release and acceptance receipt

Supervisor message `msg-158a13d011f54607b561c23dbe68a419` confirms that direct-mode task close integrates main and authorizes publishing from the already pushed qualified revision immediately before close. The attempted `aq git merge --branch-name aq/eager-grove-45.2 --default-branch main --brief` was refused with `out of scope: git_merge`; it was reported, not bypassed. No raw main push was performed.

`gh release create v0.2.0 --repo ElectricJack/aq-git --target e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a --title 'aq-git v0.2.0' --notes-file RELEASE_STAGE/release-notes.md RELEASE_STAGE/assets/ASSET...` uploaded exactly the six assets listed below. `RELEASE_STAGE` is the stable merged-candidate directory recorded above. Publication time: **2026-11-03T01:11:01Z** (2026-11-02 in the host timezone).

| Artifact | Download | SHA-256 |
| --- | --- | --- |
| Tagged source archive | [source](https://github.com/ElectricJack/aq-git/releases/download/v0.2.0/aq-git-v0.2.0-source.tar.gz), [checksum](https://github.com/ElectricJack/aq-git/releases/download/v0.2.0/aq-git-v0.2.0-source.tar.gz.sha256) | `f64891f03e2d28263cf3af20b38123935ae08d6917699becb9d77e76af2fd3c6` |
| Linux x86_64 executable | [binary](https://github.com/ElectricJack/aq-git/releases/download/v0.2.0/aq-git-v0.2.0-linux-x86_64), [checksum](https://github.com/ElectricJack/aq-git/releases/download/v0.2.0/aq-git-v0.2.0-linux-x86_64.sha256) | `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` |
| Build record | [record](https://github.com/ElectricJack/aq-git/releases/download/v0.2.0/aq-git-v0.2.0-build-record.json), [checksum](https://github.com/ElectricJack/aq-git/releases/download/v0.2.0/aq-git-v0.2.0-build-record.json.sha256) | `94e318a2285369002c1df8ea9844ca0295ce3b867fa35691d201cc77f1c7852f` |

Remote readback commands all passed:

```bash
gh release view v0.2.0 --repo ElectricJack/aq-git --json tagName,targetCommitish,assets,url,publishedAt,isDraft,isPrerelease
git ls-remote origin refs/tags/v0.2.0
gh release download v0.2.0 --repo ElectricJack/aq-git --dir "$release_stage/download-verification"
(cd "$release_stage/download-verification" && sha256sum -c ./*.sha256)
```

The release is published (not draft or prerelease), has exactly six uploaded assets and names the qualified source revision. The remote tag is exactly `e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`. All six downloaded SHA-256 digests equal the preserved files, all three [checksum pairs pass](A3a-checks/publication-download-checksums.log), the downloaded build record identifies the exact source, and the downloaded archive embeds the same commit ID. The [machine-readable receipt](A3a-checks/publication-receipt.json) records these results and all URLs. The build record's publication-at-build-time/pending-owner fields remain historical observations; the release notes explain that the record is retained unchanged to preserve its digest. Its development-manifest certification limits also remain unchanged.

The remaining P1–P7, exhaustive crash/compatibility, power-loss, live hosted transport, deployed AQ integration and second-release upgrade/rollback limits remain as listed above. No aq-server pin or Dockerfile was changed. After accepted close, verify `e75f7b4` ancestry against the current GitHub main tip and report the integration receipt to the supervisor; do not retag if integration is delayed.
