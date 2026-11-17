# Developing aq-git

The workspace contains `aq-git-protocol`, `aq-git-core`, and `aq-git-cli`. Rust 1.96, Git, Python 3, OpenSSL, OpenSSH client/server and a Linux local filesystem are sufficient for the correctness fixtures. Tests create temporary local repositories and independent state roots; they require no sibling AQ checkout, external network Git service, configured credentials, container daemon, or hosted account. The design specification and implementation plan under `docs/specs/` and `docs/plans/` remain the acceptance authority.

```sh
export CARGO_TARGET_DIR="$PWD/target"
cargo build --workspace
scripts/check-fast
scripts/check-all
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
scripts/check-docs
```

Run these commands from the workspace root. In an AQ slot, keep `CARGO_TARGET_DIR` at that slot's own `$PWD/target`; do not point it at the base checkout or another slot. To reclaim build space, run `cargo clean --target-dir "$PWD/target"` only in your own slot.

Development and test builds retain workspace file/line backtraces with `debug = "line-tables-only"`; third-party dependencies omit debug info. Interactive builds are incremental (Cargo's dev-profile default), which makes an edit-and-retest loop several times faster at the cost of gigabytes of `target/debug/incremental`; do not alternate incremental and non-incremental builds in one target directory, since each switch rebuilds the workspace crates. CI sets `CARGO_INCREMENTAL=0` because its builds are one-shot. See the "Build health" section of `AGENTS.md`. The `aq test` command currently runs pytest; run Rust checks with Cargo or the check scripts, keeping `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1` in AQ slots.

The fast script runs library/binary tests, protocol contracts, and small real-Git state, repository, and service invariants. The complete script runs every integration target, including process ownership and Unix sockets. Development normally uses a focused target:

```sh
cargo test -p aq-git-core --test state_runner
cargo test -p aq-git-core --test artifact
cargo test -p aq-git-core --test repository
```

Use real Git for repository correctness. Keep deterministic author/committer identities and timestamps, scrub user Git configuration, and isolate mutable fixture state. Fake executables are useful for deterministic subprocess timeout, output/sink limits, and disk-budget tests. They do not establish Git merge, bundle, reference, or ancestry semantics. Process-crash tests must kill the engine separately from its guard and exercise surviving descendants; killing an entire test process group does not demonstrate the crash fence.

`StateRoot` owns an advisory root lock through shared lifetime ownership. Metadata reads, directory creation, temporary writes, and atomic replacement use directory-relative operations with no-follow traversal. A durable write flushes the file, renames in the same directory, and flushes that directory. Newly created directories also flush their parents. Records and temporary files are private. Unsupported state/record versions refuse interpretation. JSON records are bounded to 64 MiB.

Operation IDs are reserved across the root. Canonical hashes sort object keys and preserve arrays; the validated mutation payload includes repository identity, expected generation, and mutation-affecting fields. Journal generations reject stale in-memory updates. Generated resource IDs and commit timestamps are fixed by durable intent. Domain operations must record recoverable phases before effects and treat I/O/timeout/unknown effects conservatively. Publication must persist dispatched intent before remote execution and never resend automatically during recovery.

`WriteFault::{BeforeRename,AfterRename}` can be injected through `StateRoot::inject_write_failure_once` or `inject_write_failure_after(successful_writes,point)`. Faults are scoped to the root instance and do not depend on process-global environment variables. Tests should verify both retained old snapshots and installed-but-unacknowledged new snapshots. Artifact tests also fail after independent transfer pin creation but before export manifest/import receipt persistence, then replay the same operation. These checks verify ordering and recoverability, not power-loss behavior.

Retention primitives store active/releasing/released records, expected generations, exact refs/OIDs, and live dependency edges. Domain adapters create/harden objects and refs before acknowledging a handle. The initial correctness adapter verifies object connectivity and synchronizes existing pool object files/directories before new pins, including objects originating through ordinary workspace Git. This conservative path is unbenchmarked; later optimizations must preserve its durability evidence. A release must persist intent, delete only matching expected refs, then persist its released tombstone. Dependency updates must share repository coordination with use/release. Historical records do not themselves retain objects. Export first acquires the source dependency, then its independent pin, then drops the temporary source dependency; subsequent transport failure cannot strand the original source dependency.

`GitRunner` accepts an absolute administrator-selected executable and an argument vector. Its local environment is explicit and scrubbed. Commands have bounded concurrent child permits, output/input bytes, execution deadlines, and inherited Linux `RLIMIT_AS` (default 1 GiB per Git process). A helper test attempts a 512 MiB allocation under a configured 128 MiB ceiling and verifies refusal; persistent readers use the same resource profile. A single nonblocking `ppoll` pump handles stdin/stdout/stderr without fixed sleep polling. Child completion uses Linux pidfds, with a tested blocking `waitid(WNOWAIT)` waiter/socket fallback when pidfds are unavailable. Ordinary child admission has a FIFO bound of 64 waiters, and queue time consumes the same absolute deadline as execution. Persistent-reader release, startup failure, and ordinary completion all notify admission waiters. Admission/wait metrics distinguish queue time from execution time. Disk-budget commands retain periodic storage sampling. This prevents a child that ignores stdin or retains output pipes from blocking cleanup. Linux `waitid(...,WNOWAIT)` keeps the leader unreaped until owned process-group cleanup, preventing PID/PGID reuse during that step. The executable guard supplies subreaper descendant ownership across engine death; process groups alone do not establish lifecycle cleanup.

`DeadlineScope` constrains synchronous Git work on a request thread, nests without extending an outer deadline, and restores the previous deadline on drop. Persistent readers must consult `remaining_deadline()` as well. Internal worker threads do not implicitly inherit thread-local deadlines; keep budget enforcement in their owning request loop or pass the budget explicitly.

Artifact limits can be made small per fixture through `artifact_limits.json`. Streamed output enforces a strict compressed byte ceiling. Additional managed disk bytes are sampled during Git with per-file resource limits and checked before acknowledgement; sampling permits temporary aggregate overshoot. This limitation must remain explicit in capabilities/operations documentation. Imports validate a digest-checked immutable quarantine snapshot, exact refs, connectivity, ancestry, identity/generation, and feature support before touching managed retention refs.

Benchmarking and live hosting are separate from local correctness. Use `scripts/bench` and `docs/benchmarking.md` for measured workloads. Record raw answers, failures, machine/filesystem details, cache state, versions, and every run. Do not infer the specification's speedup, throughput, mixed-load, churn, or hosted compatibility gates from a local smoke test. Release evidence must list pending gates and unsupported capabilities explicitly.


The remote fixtures start short-lived servers bound only to `127.0.0.1`. SSH tests generate throwaway client/host keys and use an unprivileged `sshd`; install `openssh-server` or set `AQ_TEST_SSHD` to an absolute server executable path. The tests fail with setup instructions if it is missing; they never silently skip. HTTPS tests use Python's standard-library TLS server and real `git http-backend`, with a throwaway certificate trusted only by a fixture Git wrapper. No TLS verification is disabled. Both fixtures clean up their server processes on drop. These tests qualify local transport/authentication behavior, not live GitHub policy or deployment.

## Documentation validation

`scripts/check-docs` checks Markdown code fences and relative links in `docs/` and `README.md`, plus JSON syntax under `protocol/` and the benchmark reference fixture. It needs only Python 3 and works in an isolated checkout or worktree with no sibling repositories. Links that point into this repository must resolve; any broken one fails.

Links that leave the repository are cross-repository context references. They must name `aq-server`, `aq-worker` or `aq-graph` directly beside this checkout (`<checkout>/../<sibling>/...`). A typo, an unknown repository, or a path escaping above that level fails. A cross-repository link is checked when its sibling checkout exists under the sibling root. Otherwise it is listed as not checked, and validation still passes. The sibling root defaults to the checkout's parent directory. A worktree or CI job whose siblings live elsewhere sets `--sibling-root DIR` or `AQ_GIT_SIBLING_ROOT`; pass `--require-siblings` to fail on any absent sibling instead. Keep cross-repository links written for side-by-side checkouts so they stay useful to readers.

```sh
scripts/check-docs                                   # standalone; absent siblings reported, not failed
scripts/check-docs --sibling-root ~/src --require-siblings   # full cross-repository check
python3 -m unittest discover -s scripts -p 'test_*.py'
```
