# aq-git agent guide

Development commands are in [`docs/development.md`](docs/development.md). AQ task sessions also get the project notes in
[`.aq/PRIME.md`](.aq/PRIME.md).

## Build health

These are owner requirements (2026-11-05), the same in every AQ v2 Rust
repository. By November 2026 builds had become the bottleneck for AQ v2. In
aq-server an edit followed by one test rerun took 127 s, and CI ran every test
binary one after another. No build budget existed, so each change that made
things a little slower was locally reasonable (aq-server retrospective W72).
The rules below keep that from happening again. Follow them in interactive
sessions and in AQ task sessions.

### Interactive builds

- **Incremental.** The dev and test profiles are incremental, so a rebuild
  after an edit recompiles only what changed. Do not export
  `CARGO_INCREMENTAL=0` in an interactive session. Switching between
  incremental and non-incremental builds rebuilds the workspace crates, so stay
  on one setting per target directory. CI and one-shot builds set
  `CARGO_INCREMENTAL=0` themselves.
- **Disk cost.** `target/debug/incremental` grows to gigabytes per checkout.
  When disk is short, delete your own `target/debug/incremental`. Never touch
  another worktree's target.
- **Private target directory per worktree.** Each worktree builds into its
  own `CARGO_TARGET_DIR` (the default `./target`, or a path only that worktree
  uses). Never share a target directory across worktrees or branches. Worktrees
  produce identical artifact hashes and cargo judges freshness by mtime, so a
  shared target links another tree's code into your tests and their green
  result means nothing (aq-server retrospective W67).
- **Do not alternate `cargo build` and `cargo test` without a reason.**
  Dev-dependencies enable extra features (`tokio/test-util` in aq-git-cli), so
  `cargo build` and `cargo test` resolve different feature sets and each switch
  rebuilds the crates that depend on them.
  Iterate with `cargo test` on the test binary you are working on. Run
  `cargo build` only when you need a binary itself, and `cargo clippy` once at
  the end.
- **Test one binary at a time while iterating:**
  `timeout 900 cargo test -p aq-git-core --test state_runner [filter]`.
  Run the whole suite only when the task needs it.
- **Every `cargo test` in an agent session runs under a timeout**, for example
  `timeout 900 cargo test ...`, so a hung test ends the command instead of the
  session.

### Tests

- **New integration tests go into an existing test binary.** Add a module or
  a test function to the existing `tests/*.rs` binary for that area. Do not add
  a new `tests/*.rs` file: each one is another executable that links the whole
  library and all its dependencies. CI rejects an increase in the number of
  test binaries (see the budgets below).
- **Serialization.** A `static` mutex serializes tests only inside one test
  process. If tests must never run concurrently under a per-process runner
  such as nextest, declare a nextest test group (`.config/nextest.toml`), as
  aq-server does.

### Budgets

`build-budget.toml` holds ratchets, and `scripts/build-budget.py` (CI) fails
when a number goes above its recorded value:

| Budget | Recorded 2026-11-05 | Rule |
| --- | --- | --- |
| Integration test binaries in the workspace | 14 | May only go down. |
| Lines in one crate's lib (`src/`, minus bin targets) | target 40,000 | No crate lib may grow past the target. |

- Check locally with `python3 scripts/build-budget.py`.
- **When a number goes down**, lower the recorded value in the same PR. Run
  `python3 scripts/build-budget.py --update`, which only ever lowers values.
  CI fails if the test binary count is below its recorded value, so a removed
  binary cannot be quietly replaced by a new one later. Line ratchets print a
  note instead.
- **Raising a number** needs an explicit edit of `build-budget.toml` in its own
  commit, a reason in the commit message, and the owner's review. Prefer
  moving code into another crate or tests into an existing binary.

### CI wall time

Each job in `.github/workflows/check.yml` has a budget of 10 min. The whole workflow (push to last
green job) has a budget of 20 min. `timeout-minutes` is a hang guard, not the
budget. A job that goes over budget on main is a defect, not a fact of life.
Open a task the same day with the measured times from the run and the cause.
Until the job is back under budget, do not add work to it. Fix it with caching,
parallel jobs, nextest, consolidating test binaries or a cheaper fixture, not by
raising the budget. Raising a budget needs the owner's review.

### Docker images

- **No shared cargo target cache without the touch step.** A Dockerfile that
  mounts a BuildKit cache over the cargo target directory, shared across trees,
  must run `find <path crate dirs> -type f -exec touch {} +` in the same `RUN`
  as `cargo build`, or use a cache id unique to the exact source. Without
  either, cargo's mtime check reuses another branch's build (aq-server
  458f682d).
- **Release-only settings stay out of CI images.** ThinLTO lives only in
  `[profile.release]`. Images built only for CI or tests use the `ci-image`
  profile (release without LTO). aq-git has no Dockerfile of its own; aq-server
  consumes the verified release executable.
