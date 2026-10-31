# Implementation and release evidence

This repository is a development implementation of the baseline design. It has not been published or deployed. The performance and host-power-loss acceptance gates are not certified. Do not infer deployed AQ integration or production readiness from standalone tests.

| Area | Implementation and evidence |
| --- | --- |
| M0 Protocol and executable | Rust workspace, versioned request/response schema, strict envelope decoding, CLI and bounded NDJSON transport. `aq-git-protocol` tests and CLI framing tests. |
| M1 Durable state and process ownership | Private no-follow metadata writes, file/directory sync, root lock, operation replay, retention primitives, deadline/output/storage budgets, internal process guard and audited recovery fence. `state_runner` and CLI `process` tests. |
| M2 Repository access | Explicit registration/sync/observations, drained binding updates, exact queries and batches, bounded shared cache and persistent object readers. `repository` tests. |
| M3 Workspaces | Independent private Git directories borrowing objects, explicit head/all checkpoints, source retention, guarded retirement. `workspace` and `lifecycle` tests. |
| M4 Handoff | Full/incremental/reference-only manifests, SHA-256 verification, immutable quarantine snapshots, prerequisites/connectivity/identity validation, durable retained receipts and pool rehydration. `artifact` tests. |
| M5 Candidates | Frozen ordered merges, fixed commit metadata, retained prefixes/inputs, explicit conflicts and historical replay. `candidate` tests. |
| M6 Publication | Central-only configured-target fast-forward publication, explicit expected-old lease, durable dispatched intent, uncertainty holds and audited reconciliation/resolution. `publication` tests include a real successful push with suppressed acknowledgement. |
| M7 Process integration | Direct/socket semantics, reconnect via operation IDs, bounded admission, private socket ownership, fake coordinator/worker integration tests and documented AQ adapter boundary. CLI integration tests. |
| M8 Release and performance | Test scripts, documentation checks, CI definition and benchmark drivers/evidence exist. Full performance acceptance, supported-host power-loss rehearsals, hosted transport/credential verification and release packaging remain separate gates. |

The test names above refer to Cargo integration targets; run `scripts/check-all` for the current full correctness suite. Build and benchmark results must be recorded for the actual source state being evaluated. A previous passing run does not certify later changes.

## Development validation, 2026-10-30

The full correctness suite passed 110 Rust tests and eight benchmark-driver tests on the local Linux development host with Rust 1.96.1 and Git 2.43.0. Formatting, strict Clippy, documentation checks, and the release build passed after the performance and recovery changes, including exact cached response-size accounting and response encoding on computation workers. Release-manifest checks independently verified binary and source hashes and refusal of unsafe output paths and falsely clean source claims.

Recovery regressions cover interrupted binding admission, unfinished eviction versus import, historical publication replay versus a newer uncertainty hold, and an actual engine crash after a real push but before acknowledgement. The restarted socket service does not push again. Additional checks cover malformed server responses, unavailable-remote reconciliation, TERM-resistant descendant cleanup, and Git merge-profile conformance.

An intermittent artifact `invalid_path` failure was diagnosed after a later combined run captured the quota scanner rejecting a temporary `tXXXXXX` symlink. [Independent Git 2.43 traces](../benchmarks/results/init-probe-diagnostic/results.json) confirm that `git init` creates this filesystem probe even with explicit `core.symlinks` settings; the original failing instance itself was not traced. Disk-budgeted commands now use strict checks before spawning and after child cleanup, while live sampling counts link bytes without following links. A deterministic regression covers transient probes, quota accounting, and rejection of pre-existing or residual links. Assertions and final path checks remain strict.

Short reference-sized performance runs are recorded in [benchmarking](benchmarking.md). They validate exact answers and expose performance misses; they do not satisfy the required measurement durations, repetitions, or complete release matrix.

## Acceptance evidence map

This map identifies the executable evidence for the design's acceptance IDs. A linked test establishes its tested cases, not every possible crash point or a release certification.

| IDs | Local evidence | Limits |
| --- | --- | --- |
| V1 | [`scripts/check-all`](../scripts/check-all), [CI checks](../.github/workflows/check.yml) | Current local Linux toolchain; broader compatibility remains pending. |
| V2, V3 | [Repository tests](../crates/aq-git-core/tests/repository.rs): fixed-input reads, warm batches, absent derived graph, explicit sync and shared fetch | Performance duration/repetition gates are separate. |
| V4 | [Workspace tests](../crates/aq-git-core/tests/workspace.rs): independent edits, modes, checkpoints, quiescence and retirement | Cooperative writer quiescence; no hostile-process confinement claim. |
| V4a | [Lifecycle tests](../crates/aq-git-core/tests/lifecycle.rs) and [artifact tests](../crates/aq-git-core/tests/artifact.rs): guarded eviction, history and rehydration | Object pruning remains unsupported. |
| V5, V6 | [Artifact tests](../crates/aq-git-core/tests/artifact.rs) and [public-socket coordinator scenario](../crates/aq-git-cli/tests/scenario.rs) | Local artifact copying; deployed AQ transport is separate. |
| V7 | [Journal tests](../crates/aq-git-core/tests/state_runner.rs), domain replay tests and [service recovery tests](../crates/aq-git-core/tests/service.rs) | Exhaustive persisted-boundary interruption is not complete. |
| V8 | [Candidate tests](../crates/aq-git-core/tests/candidate.rs): frozen merges, replay, retention, and plain-Git parity for renames, binary/symlink conflicts, executable modes and directory/file conflicts | Broader Git-version compatibility remains pending. |
| V9 | [Publication tests](../crates/aq-git-core/tests/publication.rs): leases, target races, up-to-date observations, lost acknowledgements and resolution | Local bare remotes; hosted transports remain pending. |
| V10 | [Process ownership tests](../crates/aq-git-cli/tests/process.rs), [post-publication engine-crash scenario](../crates/aq-git-cli/tests/scenario.rs), [durable-write fault tests](../crates/aq-git-core/tests/state_runner.rs), interrupted domain operations | Partial: not every persisted boundary has a process-kill case; host power loss is untested. |
| V11 | Protocol/domain refusal tests, [runner limits](../crates/aq-git-core/tests/state_runner.rs), [launch-helper tests](../crates/aq-git-cli/tests/spawn_helper.rs) | Capability restrictions are intentional; unsupported features are not silently enabled. |
| V12 | [Raw performance evidence and explanations](../benchmarks/results/evidence-summary.md) | Full P1–P7 qualification remains pending. |
| V13 | [Wire schema/fixture checks](../crates/aq-git-protocol/tests/contracts.rs), [API](api.md), [operations](operations.md) and [development](development.md) guides | No earlier compatible release exists; installation/rollback qualification remains pending. |
| V14 | [Binding tests](../crates/aq-git-core/tests/repository.rs) and [lifecycle tests](../crates/aq-git-core/tests/lifecycle.rs) | Fault cases must preserve the admission fence through recovery. |
| V15 | [Lifecycle](../crates/aq-git-core/tests/lifecycle.rs), [candidate](../crates/aq-git-core/tests/candidate.rs), [artifact](../crates/aq-git-core/tests/artifact.rs) and [publication](../crates/aq-git-core/tests/publication.rs) tests | Targeted dependency/release cases, not exhaustive crash certification. |

## Remaining release gates

- Demonstrate every P1–P7 target on the declared reference fixture, with the required repetitions and durations. Smoke measurements are diagnostic only. Retain misses and explanations alongside improved results.
- Complete the full compatibility and persisted-boundary crash matrix across supported Git versions. Existing targeted cases, including repair merges, executable replacement, and lost publication acknowledgements, do not constitute exhaustive V1–V15 certification.
- Exercise real filesystem/VM power loss before claiming host-power-loss durability. Process termination and injected sync/rename errors demonstrate different failure modes.
- Validate credential profiles and hosted HTTPS/SSH policy with explicit disposable repositories. Local-bare-remote tests do not certify a hosting provider.
- Finish release manifests/checksums and upgrade/rollback rehearsal for a versioned source revision. The initial release has no previous compatible client to test; that requirement starts with the second compatible release.

The cumulative artifact-storage budget is monitored during Git subprocess execution and checked before acknowledgement; sampling can temporarily overshoot the configured aggregate limit. Per-file limits and compressed streaming limits are separate. See [operations](operations.md) for the exact guarantee.

AQ authentication, polling, HTTP upload/download, assignment policy, and worker execution confinement remain consumer responsibilities. The standalone component exposes no worker network listener.

## Development manifests

[`scripts/release-manifest`](../scripts/release-manifest) generates local SHA-256 manifests for existing binaries and inventories the actual tracked plus nonignored untracked source. It explicitly labels dirty/unversioned source, records independent format/profile versions and platform/tool requirements, and always marks the artifact `development_not_certified`. It does not attest that a pre-existing binary was built from the inventoried files, publish artifacts, or satisfy the remaining upgrade/rollback and acceptance gates. See [local manifests and upgrade constraints](operations.md#local-release-manifests-and-upgrades).
