# aq-git design specification

**Status:** Draft for review  
**Date:** 2026-10-30  
**Target:** Standalone baseline 0.1, protocol v1, local state format v1  
**Implementation:** [Companion implementation specification](../plans/0001-aq-git-implementation.md)

aq-git provides a fast, self-contained repository interface for agent hosts. Its unit of correctness is an explicit Git revision or a conditional repository operation. Agent Q decides which work to run and which changes may integrate; aq-git executes repository operations and reports verifiable outcomes.

The system must work without Agent Q, aq-graph, a database server, or a public hosting account. One Rust executable supplies a human CLI, a machine interface, and an optional persistent local process. A separate repository provides independent releases and tests without introducing another mandatory infrastructure service.

## 1. Motivation and requirements

AQ v1 experienced divergence between repository state and database state describing the same code-delivery facts. Requiring fresh Git evidence throughout orchestration then made repository access a significant operational concern. The v2 design preserves source-control authority while separating immutable questions from synchronization of moving references.

The existing v1 implementation already contains immutable-fact caches and shared observations. This specification does not assume the absence of caching explains its latency. The new implementation must measure subprocess, synchronization, queue, lock, filesystem, and graph-query costs independently.

The primary product goal is an order-of-magnitude reduction in the repeated repository-query latency agents pay to use Git as their source of truth. Correctness alone is insufficient for adoption. Reuse immutable facts, amortize process and protocol costs, and share explicit synchronization across consumers without weakening freshness, durability, or publication preconditions. Section 14 defines comparative and absolute acceptance targets; these are requirements to demonstrate, not existing measurements.

| ID | Requirement |
| --- | --- |
| R1 | The designated Git repository is authoritative for committed code and published refs; no independently editable delivered flag substitutes for Git evidence. |
| R2 | Exact-revision reads perform no implicit network access. Observations of branch names disclose revision, scope, and freshness. |
| R3 | A warm immutable-query cache hit performs no Git subprocess; repeated consumers share expensive work. |
| R4 | Mutations have stable operation identities, bounded execution, durable intent, and explicit recovery behavior. |
| R5 | Publication checks an explicit expected destination revision and permits only fast-forward changes in the baseline. |
| R6 | Candidates freeze base, ordered sources, merge configuration, and commit metadata. |
| R7 | Multiple agents receive independent writable workspaces while a node reuses repository objects. |
| R8 | Source handoff remains usable after the originating worker becomes unavailable. |
| R9 | Workers require only outbound AQ communication; aq-git performs local operations without node discovery or inbound worker RPC. |
| R10 | The baseline adds no database, broker, cache server, container runtime, or new hosted provider dependency. |
| R11 | The complete correctness suite runs with Rust, Git, and a local filesystem; performance and live-host verification are separate suites. |
| R12 | A versioned public protocol keeps consumers independent of AQ internals and aq-git's private files. |
| R13 | Unknown, incomplete, stale, and conflicting results remain distinct from negative facts or successful writes. |
| R14 | Repository and operation retention is explicit; cache eviction cannot destroy submitted or unpublished work. |
| R15 | Repeated agent reads meet section 14's latency, throughput, work-sharing, and mixed-load requirements against reproducible equivalent Git baselines. |

## 2. Scope and component ownership

| aq-git owns | Its caller owns |
| --- | --- |
| Repository binding, local object storage, remote synchronization | Project discovery and which repository a task uses |
| Commit, tree, diff, ancestry, and observed-ref queries | Task lifecycle, dependency policy, and knowledge |
| Workspace creation, inspection, checkpoints, and retirement | Agent launch, execution isolation, process termination, and capacity |
| Bundle export/import and durable local transfer receipts | Authenticated network upload/download and cross-machine placement |
| Candidate construction and explicit conflicts | Train membership, ordering policy, repairs, and test selection |
| Conditional publication and operation reconciliation | Approval, test sufficiency, assignment authority, and release policy |
| Local protocol, limits, metrics, and repository recovery | Human login, worker enrollment, heartbeats, and authorization at AQ APIs |

aq-git does not read AQ or aq-graph tables, load playbooks, launch agents, execute tests, or decide that a task is complete. A caller correlation field may name a task or attempt, but it is opaque metadata and grants no authority.

The baseline excludes a new VCS, distributed repository database, high-availability publishers, multi-user network service, semantic merge, automatic rebase, squash-equivalence delivery inference, remote workspace execution, and v1 migration. Jujutsu, GitButler, and Worktrunk are evaluation references; none is a required dependency or initial backend.

## 3. Deployment and dependency budget

### 3.1 One executable in two locations

The same `aq-git` binary runs centrally and on each worker. The CLI can execute directly against an idle state directory. `aq-git serve` keeps a bounded cache and Git process pool alive and accepts local requests over a Unix-domain socket. Agent Q supervises that child process and connects through the public protocol. No automatic daemon installation, TCP listener, TLS endpoint, or separate service discovery is required.

Initial support is Linux, Linux containers, and Linux under WSL2 using its Linux filesystem. Native Windows and macOS require later filesystem and durability verification. State lives on a local filesystem with working file locks, atomic rename, and durable file/directory synchronization. Shared NFS/SMB state directories are unsupported.

Runtime dependencies are a supported Git executable, ordinary filesystem facilities, and configured credentials only where remote access is enabled. OpenSSH or an HTTPS credential helper is needed only for a selected remote transport. The baseline has no SQLite, PostgreSQL, Redis, event broker, FUSE, Docker socket, or Git hosting SDK dependency. Cargo dependencies are implementation libraries, bounded separately from infrastructure dependencies.

### 3.2 State ownership

Exactly one aq-git process owns a state directory through an OS advisory lock held for its lifetime. A direct CLI invocation refuses a busy root and tells the operator to use `--socket`; it never opens a second writer. Independent state roots can run concurrently. The persistent owner supports concurrent immutable reads and bounded mutations internally.

Lock acquisition alone is insufficient after an unclean owner exit: Git children or helpers may still be running. Before spawning any child, durably record the owner incarnation and establish supervised child ownership. Both direct CLI and service execution must keep replacement recovery/mutations fenced until all local descendants from the old incarnation have stopped. An internal guard supplied by the same executable provides this lifecycle independently of AQ; no external service manager is required. If the guard also dies and cleanup cannot be established, startup returns `recovery_blocked` with diagnostic evidence instead of guessing from a missing PID. A documented, audited recovery procedure must establish that the old execution environment is stopped before clearing this fence. Local cleanup never proves that a dispatched remote push had no effect. The implementation plan specifies guard behavior and parent-only crash tests.

The socket and state root are private to the node runtime's OS identity. This is a local trusted-process API, not a tenant security boundary. Agent-visible grants exclude the runtime socket/state, central credentials, and direct writes to the shared object pool. The initial worker design uses harness-configured workspace confinement with explicit optional folder grants; it does not require a separate container per agent. If agents share an unrestricted OS identity, aq-git cannot enforce a hostile-process boundary against them. The worker declares the actual confinement strength. Supervisor agents live centrally; the local node runtime is ordinary lifecycle software. See the [worker framework design](../../../aq-worker/docs/specs/0001-worker-framework.md).

## 4. Source of truth and persistence

### 4.1 Authorities

| Information | Authority and reconstruction |
| --- | --- |
| Published branch value | Configured remote ref at the time of an authoritative observation/write |
| Commit and tree contents | Git objects, addressed by full object IDs |
| Local unsubmitted work | The worker's workspace and any explicit retained checkpoints |
| Task and attempt state | External caller; never inferred from a Git operation receipt |
| Git-derived query answer | Rebuildable cache keyed by immutable inputs and query semantics |
| Repository binding and configured policy | aq-git configuration, versioned by a binding generation |
| Requested operation and received acknowledgement | Durable operation journal; describes intent and observed outcomes, not the current remote branch |
| Handoff availability | Retained objects and local acceptance record at the receiving installation |

The baseline keeps the existing configured remote authoritative for shared branches. Local pools are replicas and retained stores, never competing shared-branch authorities. A publisher may use a local bare repository as its authoritative remote for standalone use and tests. Changing authority requires an explicit new binding after unresolved operations are dealt with; no automatic mirror promotion is supported.

Deleting derived caches must preserve all work and allow equivalent repository facts to be recomputed. Deleting the operation journal is not cache clearing: it loses caller intent, idempotency records, and historical acknowledgements. Recovery must distinguish those cases.

### 4.2 Storage layout

Conceptual private layout; consumers must not depend on it:

```text
state/
  format.json
  owner.lock
  operations/<operation-id>.json
  repositories/<repo-id>/
    binding.json
    pool.json
    objects.git/
    observations/<observation-id>.json
    workspaces/<workspace-id>.json
    candidates/<candidate-id>.json
    transfers/<transfer-id>.json
    staging/
  workspaces/<workspace-id>/
  artifacts/<artifact-id>/
  logs/
```

The journal uses one versioned JSON snapshot per operation, written to a same-directory temporary file, flushed, atomically renamed, and followed by a directory flush. Record generations prevent an older in-memory transition from overwriting a newer one. An acknowledged write requires all referenced records and Git retention refs needed to recover it to be durable. Multi-record operations use ordered steps with recovery rules; a filesystem rename is not a transaction across Git and journal files.

Persistent IDs and generated commit timestamps are fixed in the first durable intent. Recovery reuses them. Unknown future formats cause startup refusal without mutation. Journals retain completed operation IDs indefinitely in the baseline; growth is measured and reported. Compacting terminal records into small idempotency tombstones is allowed only if exact replay results remain available or the protocol returns `operation_expired` and never re-executes them. Automatic idempotency expiry is deferred.

## 5. Repository model and compatibility

A repository binding contains an opaque repository ID, logical caller identity, binding generation, mode (`central` or `worker`), object format, allowed observed refs, allowed publication targets, canonical remote URL where applicable, and credential-profile name. URLs supplied in ordinary read/write requests cannot override the binding.

`repo register` initializes an empty managed bare pool and metadata. It does not implicitly clone or contact a remote. Central `repo sync` explicitly fetches configured refs. Worker pools normally receive bundle artifacts and need no hosting credentials.

### 5.1 Binding lifecycle

Registration requires `expected_binding_generation=0`, meaning local absence, separately from the generation being installed. Central registration starts at generation 1; worker registration supplies the positive logical generation from the authenticated caller's assignment. An artifact cannot register or update a binding.

`repo binding-update` takes a new operation ID, expected current generation, complete replacement binding, and reason. Central updates advance exactly one generation; a worker may adopt a higher caller-supplied central generation, including skipped generations. Mode, repository identity, and object format cannot change in place. Remote URL, credential profile, allowed scopes/targets, and supported feature profile may change through this operation only. Changing repository identity or mode requires a separately registered repository ID.

The baseline uses a drained transition: stop new repository admission, drain readers and Git children, and refuse the update if any live retention handle, workspace, or other unresolved operation remains. Retired workspace records are historical and do not block the update. Return the blocking IDs; never release work automatically to make an update succeed. A caller that must preserve old work can keep the old binding and register a new repository ID. AQ is responsible for pausing assignments and coordinating workers; there is no distributed atomic binding update.

Persist old/new bindings and transition intent, install the new binding durably, invalidate observations/caches and restart object readers, then acknowledge. Recovery finishes the recorded transition before reopening admission. Retain old binding versions and historical receipts for diagnosis, but they grant no authority under the new generation. Objects may remain as unpinned reusable data. New requests/artifacts with an old generation return `binding_conflict`; exact replay of a terminal old operation still returns its explicitly historical result. Worker adoption cannot relabel an old workspace or source artifact. Test an update racing an import/publication and a worker missing multiple central generations.

### 5.2 Compatibility

Initial supported repositories use complete SHA-1 history, regular files, executable bits, symlinks, and standard Git merge semantics. OIDs are typed with an algorithm field so later SHA-256 support does not require a misleading 40-character assumption in the public schema; unsupported algorithms return `unsupported_feature` at registration/import. Shallow and partial repositories, submodules, LFS-managed content, custom merge drivers, required checkout filters, and mandatory commit-signing flows are rejected for affected baseline operations with a named capability error. Do not silently operate on unresolved gitlinks or LFS pointers as if they were complete workspaces. Feature support can expand through explicit compatibility fixtures.

Read plain Git objects with replacement refs and grafts disabled. Managed bare pools do not inherit user aliases, hooks, filters, or executable repository configuration. Normalize merge-affecting options into a versioned profile. Source trees containing attribute features unsupported by the profile must be rejected before workspace creation or merge. Standard text/binary attributes supported by the profile need conformance tests.

Ordinary Git tools remain usable within agent workspaces. They do not gain authority to update the central destination. Private managed pools and private refs are runtime-owned. An operator changing them outside the interface invalidates aq-git's assumptions and requires inspection/recovery.

## 6. Observations, queries, and fast reads

### 6.1 Explicit consistency

Exact queries take full OIDs. They never fetch missing objects automatically. A missing object produces `object_unavailable`, with a suggestion to synchronize or import it. `contains` means strict commit ancestry under the configured complete-history semantics; it does not mean patch equivalence, current behavioral presence, or task completion. A revert can preserve ancestry while removing behavior.

`repo observation` returns the latest completed local observation for a requested set of refs: observation ID, binding generation, ref-to-OID map, started/completed timestamps, covered refs, and last refresh error. A failed refresh does not replace a previous known map with an empty map. Absence is authoritative only within a successful observation covering that ref.

`repo sync` produces a new observation after fetching the requested configured scope. It joins an in-progress synchronization only when the caller's optional minimum observation requirement can be satisfied. Consumers may reuse a completed observation; ordinary reads do not demand a fetch begun after each request. An observation is a scoped result of Git fetch, not a globally atomic snapshot of every ref on a remote host. Candidate inputs become consistent by freezing their actual OIDs.

A `max_age_ms` condition on an observation read returns `observation_stale` when unmet; it does not silently trigger a network request. The caller may explicitly synchronize. No local observation promises knowledge of later external writes. Publication checks the authoritative destination separately.

### 6.2 Query and cache contract

Initial query operations are commit inspection, bounded tree listing, bounded diff, strict ancestry, and bulk execution of these reads. A batch contains at most 100 independent items; each returns its own result. Branch expressions, arbitrary revsets, shell fragments, and unrestricted Git options are not accepted as OIDs.

Cache keys include repository identity/binding generation, object format, operation, exact OIDs, query options, and semantics version. Merge/diff answers additionally include Git/profile version where it can change results. Mutable workspace status is never cached as an immutable Git fact. Missing-object failures are not permanent negative ancestry results. Imported objects invalidate relevant availability failures.

Keep bounded in-memory caches with size metrics, LRU eviction, and single-flight computation for identical concurrent requests. Cache lifetime is independent of reference-observation lifetime. Use persistent Git batch processes for object reads. Queries that can run against bare objects do not materialize a checkout. Long history traversals, diffs, and bundle work have concurrency, byte, and time budgets.

No full-fleet status operation recursively scans every workspace. Workspace inspection is explicit, scoped, and separately measured. Filesystem notifications and persistent disk query caches are later optimizations if measured need justifies them.

## 7. Worker nodes and outbound communication

### 7.1 Worker contract

Agent Q workers are containers on cloud or desktop machines. Each connects outward to the central daemon, reports capacity and heartbeats, polls for assignments/cancellation, and posts progress/results. AQ owns that protocol and its assignment generations. aq-git does not open worker ports, maintain tunnels, run heartbeats, or subscribe to remote push events.

An AQ assignment supplies repository identity, binding generation, base commit, workspace requirements, an opaque attempt correlation, and artifact destinations. AQ translates those values into aq-git calls. Both the node runtime and the central daemon can run a local aq-git service without linking its Rust internals.

```mermaid
sequenceDiagram
    participant W as Worker node runtime
    participant A as Central AQ daemon
    participant WG as Worker aq-git
    participant CG as Central aq-git
    participant R as Authoritative Git remote
    W->>A: Poll with identity and free capacity
    A-->>W: Assignment, pinned base, artifact references
    W->>A: Download base bundle when needed
    W->>WG: Import base and create workspace
    WG-->>W: Workspace path and base receipt
    Note over W: Run agent, then stop workspace writers
    W->>WG: Checkpoint and export result bundle
    W->>A: Upload result artifact
    A->>CG: Validate and import artifact
    CG-->>A: Durable retention receipt
    W->>A: Poll or retry result report
    A-->>W: Artifact retained; attempt disposition
    A->>CG: Build candidate, later publish approved revision
    CG->>R: Conditional fast-forward push
```

### 7.2 Transfer path selected for the baseline

Use Git bundles plus a versioned manifest as the transport-neutral artifact. AQ supplies outbound authenticated download/upload endpoints, quotas, and restart/resume behavior. aq-git only creates or ingests completed local artifact files. Its standalone test driver transfers those files between independent state roots. No AQ endpoint is implemented in this repository.

Central bundles bootstrap a worker's base. Worker bundles return source commits. Complete bundles provide initial history; incremental bundles exclude only prerequisite histories the receiver has explicitly confirmed and retained. Both ends verify actual prerequisites at import. A missing prerequisite returns a structured request for a fuller bundle, never a successful partial import. Initial bootstrap cost is measured separately from warm operation cost. The baseline accepts the cost of a complete first bootstrap within configured limits; sparse/history-virtualized transfer is deferred.

Workers do not need remote write credentials. Only the central publisher has the configured destination credentials. Direct worker pushes can be a future transport adapter with explicit permission enforcement; they are not an implicit fallback.

### 7.3 Three independent milestones

1. **Execution finished:** AQ reports an agent outcome.
2. **Artifact retained:** receiving aq-git has validated the source and retained it durably outside the worker.
3. **Code published:** a source-control write was acknowledged or the resulting current state was observed with the precision described in section 10.

AQ must not collapse these into one completed flag. The receiving artifact receipt proves local durability on the named central installation, not geographic replication or disaster recovery. Off-host backup is an installation responsibility. An HTTP upload acknowledgement before import is not a retention receipt.

When an assignment becomes stale, AQ may decline to use its source while retaining it for recovery. aq-git does not decide assignment validity. Cancellation cannot revoke a network write already dispatched; in the default architecture workers cannot initiate shared-branch publication at all.

## 8. Workspaces and checkpoints

Each workspace starts at an explicit retained base. The baseline uses a private Git directory and writable files per workspace, with object reuse from the node's managed pool through Git alternates. Workspace Git operations read borrowed objects and write new objects privately. Each workspace has its own HEAD, index, config, refs, and new objects. This avoids making shared writable Git metadata the coordination mechanism between agents.

The pool retains base histories and never prunes borrowed objects while any workspace references it. Private workspace objects must be imported into the pool and pinned before the workspace is retired. The worker configures harness grants to exclude sibling workspaces and direct pool mutation; where OS permissions can additionally expose borrowed objects read-only, use that capability. Filesystem layout and alternates alone do not enforce isolation, and the cooperative baseline does not claim they do.

`workspace create` is idempotent, creates a temporary private directory, checks out the pinned base, records the base pin and ownership generation, then atomically exposes the final path. A crash leaves a recoverable allocation or a named incomplete workspace. It must not silently reuse another operation's directory.

Agents may commit with normal Git. `workspace checkpoint` runs only after the caller has stopped all workspace writers and supplies the expected workspace generation and HEAD. Baseline checkpoint modes are `head` (require a clean workspace and preserve an existing commit) and `all` (explicitly include tracked changes and nonignored untracked files, recording deletions and executable/symlink modes). The default is `head`; dirty work returns a diagnostic, not an implicit commit. Commit identity/message are explicit, and commit timestamps are fixed in the durable operation intent. The assignment base must be an ancestor of the submitted source.

The quiescence declaration is a caller precondition, not a Git-enforced process lock. Inspecting HEAD or status twice cannot prove an unrestricted writer has stopped. If quiescence is violated, refuse when detectable and preserve the workspace for diagnosis; do not promise atomic snapshots of concurrent arbitrary filesystem writes.

Checkpoint imports the source into the managed pool and creates a retention ref before acknowledging. No-change work returns the existing base/source identity without inventing a code change. A workspace remains retained until explicit retirement. Retirement requires stopped writers, the expected generation/HEAD, a clean workspace, and a retained checkpoint matching HEAD. Dirty workspace deletion and forced cleanup are outside the baseline API. aq-git does not infer that an execution lease expiry authorizes deleting files.

## 9. Artifacts and candidates

### 9.1 Transfer manifest

A manifest identifies `format_version`, repository ID and binding generation, transfer ID, object format, base OID, source OID, prerequisite OIDs, bundle size and SHA-256, producer installation ID, and opaque caller correlation. A digest detects corruption; it does not authenticate the producer. AQ authenticates and authorizes upload/download against the assigned repository and attempt. Imported metadata cannot choose remote URLs, destination refs, executable hooks, or arbitrary filesystem paths.

An artifact consists of `manifest.json` and `objects.bundle`; the protocol refers to an artifact handle inside a configured staging root. An unchanged source can use a `reference_only` manifest with no bundle if the receiver already retains the exact source. In that case the receiver still verifies availability and creates the transfer retention pin.

Validate manifest limits and the complete file digest before running Git import. Verify prerequisites, object connectivity, allowed advertised ref, source identity, and base ancestry in an isolated import repository. Reject unexpected refs and unsupported repository features. Import validated objects into the managed pool, create `refs/aq-git/transfers/<id>`, and durably persist the receipt before acknowledging. Incoming Git configuration and hooks are never installed. Imported objects remain bounded by configured byte, time, and storage quotas.

Retrying the same transfer ID and canonical manifest returns the original receipt. Reusing it with different inputs is a conflict. A crash after pin creation but before receipt persistence is recoverable from the journal and exact pin; an existing pin pointing elsewhere fails closed.

### 9.2 Frozen candidates

`candidate create` accepts base OID, ordered source OIDs, merge profile, explicit commit identity/message, and caller correlation. It uses one operation ID to fix candidate ID and commit timestamps. All sources must already be retained locally; creation performs no remote synchronization.

Merge each source into the accumulating candidate using Git's ordinary three-way merge semantics. Preserve ancestry with two-parent merge commits when required. Sources already ancestral to the accumulated candidate produce an explicit no-op step. A source that is a descendant may fast-forward that step. An equal tree alone never implies equivalent history. No automatic squash, cherry-pick, or patch-ID equivalence is used.

Use bare-object merges; do not allocate a workspace for clean candidate construction. A conflict returns `conflicted`, the step index, input OIDs, and bounded conflict paths. Retain the last clean prefix and source pins. Do not publish the conflicted merge-tree output. A caller can create a repair workspace, produce a resolved source, and request a new candidate with a `supersedes` correlation. Candidate membership and prior results remain immutable.

For an ancestry-preserving repair, start from the retained clean prefix, merge the conflicting source in the repair workspace, resolve it, and commit the actual merge parents. Verify that the resolved commit contains both inputs. A replacement candidate can then use the resolved commit followed by the remaining ordered sources; its receipt records those new inputs and its relationship to the old candidate. Copying the resolved files into a single-parent commit does not establish integration of the conflicting source and must not be reported as such.

A successful candidate receipt includes ordered inputs, step results, resulting commit/tree, Git/profile versions, and retained refs. Its revision is the revision to test. aq-git never reuses a test result itself and never regenerates code through a project shell hook. If regeneration is required, the caller creates and tests a new candidate revision.

## 10. Publication and recovery semantics

### 10.1 Ownership and preconditions

Only a central-mode installation with a publication-enabled repository binding exposes `publish`. Worker-mode installations reject it before invoking Git. For the baseline, one central publisher deployment owns a destination; automatic active/active failover and distributed fencing are out of scope. Replacing a publisher requires stopping its process tree and reconciling dispatched operations before issuing new writes for that target. A time-based lease alone is not a fence.

Publication takes an operation ID, expected binding generation, full destination ref, expected old OID, and retained candidate OID. Only configured existing `refs/heads/` destinations are allowed. Branch creation/deletion and history rewrites require later explicit operations. Verify expected-old ancestry of the candidate locally. The caller authorizes the candidate and confirms required checks; aq-git does not interpret external test or approval IDs.

Persist intent and then a `dispatched` state before starting the remote push. Use explicit source/destination refspecs and an explicit expected-old lease, never a lease inferred from a remote-tracking ref, `--force`, a matching push, or an arbitrary caller refspec. Git documents the explicit expected-value form of `--force-with-lease`; the local ancestry check makes the allowed operation a fast-forward even though that Git option can also permit rewrites. See [Git push](https://git-scm.com/docs/git-push).

### 10.2 Outcomes and precision

| Outcome | Meaning |
| --- | --- |
| `applied` | A parsed successful Git response confirms the requested target update. Persist its acknowledgement before returning. |
| `already_present` | The candidate was observed at or within the target without proof that this invocation performed the update; include observed target and timestamp. |
| `rejected` | A definite precondition, policy, or remote rejection is established; retain the candidate. |
| `indeterminate` | The request may have reached the remote, but its effect cannot be conclusively established. Block automatic redispatch for that target until reconciled or explicitly resolved. |

Git may report an up-to-date ref without sending a ref update. Treat that as `already_present`, never as proof that the expected-old transition happened. A successful fetch or preflight read cannot prevent a subsequent external update. All observed-state outcomes identify the observation time and OID; they do not promise perpetual branch state or continuous presence.

There is no transaction spanning Git's remote and local JSON records. No exactly-once remote execution or unconditionally reconstructible publication history is promised. The baseline does not require custom remote hooks or atomic multi-ref publication receipts. This keeps ordinary hosting usable while making ambiguity explicit.

### 10.3 Reconciliation

An operation that never reached `dispatched` may resume its local preparation automatically. A `dispatched` operation without a durable terminal acknowledgement becomes `indeterminate` on restart. Do not automatically resend it because a timeout occurred or because the old ref appears unchanged.

`operation reconcile` explicitly contacts the remote and records new evidence. If the candidate is the observed target or its ancestor, report `already_present` with that observation; do not fabricate an `applied` event. If the target differs or is unavailable, retain ambiguity unless a definite rejection is available. The same old target appearing again does not prove that a write never occurred, especially if an operator rewrote history.

The caller can request `operation resolve` after stopping old publishers and inspecting evidence, with a required reason and a disposition of `abandon` or `permit_new_operation`. This records a decision without declaring a historical write successful or unsuccessful. A new attempt gets a new operation ID and current explicit expected revision. Late remote effects remain subject to the Git ref precondition; no API can retract an already accepted update.

An `applied` acknowledgement remains historical evidence if the branch later moves. Current reachability must be queried separately. External force-pushes/deletions invalidate mutable observations and can make prior code unreachable; they never rewrite the operation journal. The default integration policy should protect publication branches against rewrites, but detecting an unexpected update must work even when that policy is bypassed.

## 11. Local API and CLI

### 11.1 Protocol envelope

Protocol v1 uses bounded newline-delimited JSON over a Unix socket; one request and response per line. The CLI's `--json` mode uses the same result schema. Large blobs and bundles use file artifacts, not base64 JSON. The socket service advertises supported protocol/state versions, operations, limits, and Git capabilities through `capabilities`.

```json
{
  "protocol_version": 1,
  "request_id": "client-request-17",
  "operation": "contains",
  "repository_id": "repo-opaque-id",
  "params": {
    "ancestor": {"algorithm": "sha1", "hex": "<full-oid>"},
    "descendant": {"algorithm": "sha1", "hex": "<full-oid>"}
  }
}
```

Mutations additionally require `operation_id`; repository mutations require `expected_binding_generation`, and workspace mutations also require their expected generation. Root-level recovery uses its expected owner incarnation instead. Request IDs correlate calls; operation IDs identify durable effects. The server hashes a canonical typed payload excluding request ID, deadline, and transport fields. Same ID/same payload returns the recorded result or current state. Same ID/different payload returns `idempotency_conflict`. Mutation JSON rejects unknown fields so ignored fields cannot disguise different intent.

Operation IDs are unique within one installation state root, including across repositories. Registration uses an expected binding generation of zero for absence; root-level capability/service/recovery requests omit repository/binding fields. `operation reconcile` and `operation resolve` use a new command operation ID plus `target_operation_id` and an expected target-record generation. They never reuse the publication's ID for a different payload. A journal replay returns the historical receipt with its timestamps, not a fresh claim about the current remote state. Any current retention status is a separately timestamped response field outside the immutable recorded result; fetching that local status performs no implicit remote access.

Every response contains protocol version, request ID, `ok`, result or structured error, and operation ID/state where applicable. Errors have stable code, concise message, details, and retry advice: `safe_same_operation`, `refresh_inputs`, `reconcile_first`, or `manual`. Error text never doubles as the machine classifier.

### 11.2 Public operations

| CLI / operation | Important inputs and result |
| --- | --- |
| `capabilities` | Protocol, state, Git features, limits, mode |
| `repo register` | Caller repository ID, binding, mode; returns binding generation |
| `repo show` | Binding and local availability without remote access |
| `repo binding-update` | Expected current generation, replacement binding and reason; drained, durable generation transition |
| `repo sync` | Configured ref subset; returns completed observation |
| `repo observation` | Scope/minimum age requirement; returns observation or explicit stale/unavailable |
| `repo cache-evict` | Worker-mode disposable pool, expected binding/storage generation and reason; guarded eviction preserving registration and journal |
| `inspect` | Commit/tree OID and bounded field/path selection |
| `diff` | Two OIDs and bounded options; returns changed paths/stat or patch |
| `contains` | Two commit OIDs; returns strict ancestry or unknown/error |
| `query batch` | At most 100 immutable query items; per-item results |
| `workspace create` | Base OID; returns ID, generation, path, receipt |
| `workspace inspect` | Scoped live status with HEAD and generation |
| `workspace checkpoint` | Expected generation/HEAD, quiescence assertion, mode and commit metadata |
| `workspace retire` | Expected generation/HEAD and retained checkpoint; refuses dirty/live work |
| `artifact export` | Retained source/base and acknowledged prerequisites; returns manifest/artifact handle |
| `artifact import` | Staged manifest/bundle handle; returns durable local retention receipt |
| `candidate create` / `candidate show` | Frozen inputs / recorded receipt |
| `publish` | Target, expected old, candidate, binding generation |
| `operation show` / `operation reconcile` / `operation resolve` | Durable outcome, explicit remote observation, or audited ambiguity disposition |
| `retention inspect` | Bounded handle listing/lookup, lifecycle, exact refs/OIDs, dependents, and release blockers |
| `retention release` | Handle ID, expected handle generation and exact refs/OIDs, reason; releases only this resource when no dependents remain |
| `doctor` | Read-only root, refs, journal, disk, and workspace consistency report |
| `recovery clear-fence` | Root-level direct command: expected owner incarnation, cleanup attestation/evidence and reason; journals explicit clearance after the old local execution environment is stopped |
| `maintenance` | Explicit bounded repack/index/log work; exclusive destructive maintenance |
| `serve` | State root, private socket, bounded process settings |

Global options select exactly one of `--state-dir` for exclusive direct execution or `--socket` for the persistent service. Mutation inputs can be supplied through `--input FILE` or stdin to avoid shell quoting and secret arguments. Credentials are configured by profile, never provided as JSON token values. No arbitrary shell/Git passthrough endpoint is exposed.

Initial error codes include `unsupported_protocol`, `unsupported_feature`, `state_busy`, `state_incompatible`, `binding_conflict`, `idempotency_conflict`, `object_unavailable`, `observation_stale`, `prerequisite_missing`, `artifact_invalid`, `workspace_dirty`, `workspace_changed`, `conflicted`, `target_moved`, `operation_indeterminate`, `limit_exceeded`, `deadline_exceeded`, `overloaded`, and `io_error`. A false ancestry answer is a successful result, not an exit error.

Lifecycle refusals additionally use `recovery_blocked`, `binding_in_use`, `retention_in_use`, and `retention_released`, with blocking resource IDs and safe next actions. Historical receipt replay includes current retention lifecycle separately; a recorded acceptance is never presented as a fresh availability guarantee.

## 12. Concurrency, limits, and process behavior

Immutable reads run concurrently against retained objects. A per-repository mutation queue protects local ref and metadata changes; a per-destination publication queue prevents overlapping dispatched writes. Unrelated repositories progress independently. Synchronization takes a narrow mutation lock for ref/observation installation; network preparation and ordinary immutable reads should not monopolize it. Maintenance cannot prune objects while readers, exports, candidates, imports, or workspaces retain them.

Default bounds: 64 admitted requests per state root (running plus queued), up to 32 concurrent immutable read requests, 16 Git child processes per state root, 4 heavy operations, 1 fetch per repository, 100 items per query batch, 1 MiB request frame, 8 MiB response frame, 10,000 diff paths, and 64 sources per candidate. Reserve read admission/progress so queued or running heavy operations cannot consume all slots; cap batch fan-out within the same execution budgets. Bound connections and buffered input/output separately and advertise those limits. Overflow returns explicit truncation for pageable reads or a structured refusal for mutations. Initial artifact compressed and expanded-storage quotas are configurable and reported before transfer; defaults are 1 GiB per bundle and 10 GiB additional unpacked storage, with disk reserve checks. Quarantine is removed after definite rejection or retained with an operation for diagnosis under a bounded policy.

Defaults are 10 seconds for metadata operations, 120 seconds for workspaces/candidates, and 300 seconds for fetch/import/export/push. Clients may request lower deadlines. Timeouts kill and reap owned process groups, with a short TERM then KILL grace period. Git command completion and remote write completion are different: killing a push process can still leave an indeterminate remote outcome. Disconnecting a client does not discard a dispatched mutation; callers recover through operation IDs.

Git is invoked with argument arrays and bounded stdin/stdout/stderr, no shell. Disable interactive prompts in service mode. Credential helpers are named local administrator configuration, with constrained environments. Remote URLs, path components, refs, and OIDs have separate validation; an identifier is never interpreted as a flag. File-transport remotes are allowed only for explicitly registered local deployments/tests. Arbitrary remote URL protocols and upload-controlled paths are rejected.

## 13. Retention and operations

Retain references for active workspaces, checkpoints, transfer receipts, candidates, and unresolved publications under private `refs/aq-git/` namespaces. These refs are local and never pushed by wildcard. Artifact files can be regenerated while source objects and manifest inputs remain retained; incomplete uploads/downloads are AQ transport state.

Explicit release removes a named retention handle only when dependent local records no longer need it. Releasing one handle cannot remove an object still needed by another handle or borrowed workspace. TTLs apply only to disposable caches/logs and abandoned staging with no live operation. No automatic deletion follows a worker heartbeat timeout, task completion, or source publication.

### 13.1 Retention lifecycle and dependencies

A retention handle owns an exact set of private refs/OIDs and has its own generation and `active → releasing → released` lifecycle. Resource receipts identify their handles. `retention inspect` exposes a bounded dependency graph; callers never infer dependencies by reading private files. Historical receipts and terminal journal records alone do not keep a handle active.

| Resource | Live dependency and release rule |
| --- | --- |
| Workspace | Owns its base/borrow pin and depends on its latest checkpoint handle. Safe retirement removes these workspace dependencies and releases its borrow pin; the checkpoint remains active until explicitly released. |
| Checkpoint | Owns a source pin independently of the workspace. Replacing the latest checkpoint removes the workspace's dependency on the old one, but does not release the old checkpoint. |
| Transfer | Owns a receiver source pin. An upload acknowledgement, consumer task acceptance, or publication does not release it; the caller explicitly releases it after handoff policy permits. |
| Candidate, including a conflict | Owns input pins and clean-prefix/final pins independently of the input checkpoint/transfer handles. Creation acquires these pins before input dependencies can be dropped. Explicit release retires its live retained state, while candidate metadata remains queryable. |
| In-flight operation | Holds dependencies on its inputs until durable terminal completion or explicit safe resolution. A dispatched/indeterminate publication holds its candidate until reconciled or resolved. Terminal acknowledgement alone does not release the candidate itself. |

Starting an export, candidate, publication, or other reader of retained work acquires the required dependency under repository coordination before release can proceed. A successor resource may replace a dependency with its own durable pins, but cannot drop the dependency first. Dependency edges are acyclic and refer to resource identities, not just equal OIDs. A retired workspace's record and a completed publication's historical receipt are not live dependents.

`retention release` is an explicit authorization to end this resource's retention guarantee. It checks expected generation and exact refs/OIDs and refuses with dependent IDs if still in use. Persist release intent, remove only the matching refs, durably mark the handle/resource released, then acknowledge. Recovery resumes missing steps; unexpected ref contents fail closed. Ref deletion does not promise immediate object reclamation. A released candidate cannot be published or exported as an active candidate even if its bytes remain; create a new retained resource through a new operation after verifying availability. Replaying an old creation/import/release returns historical evidence plus current handle state and never resurrects released pins.

The normal worker cleanup sequence is: obtain the central retention receipt, retire the clean workspace, explicitly release local checkpoint/transfer handles no longer needed, then request pool eviction. The caller decides when handoff policy allows release. Test the complete sequence, partial releases, shared inputs, conflicted candidates, unresolved publications, and crashes during release; a resource must neither become unreleasable solely because its history is retained nor become collectible while a live dependent exists.

### 13.2 Maintenance, eviction, and backup

Automatic aggressive Git garbage collection is disabled in managed pools. Baseline maintenance may repack and update commit-graph indexes. Pruning requires exclusive maintenance, no borrowing workspaces, reconciliation of retention refs/journals, and a grace period; first release may refuse pruning entirely if these preconditions are not implemented.

`repo cache-evict` supports bounded worker storage without arbitrary directory deletion. It is worker-mode only and requires the caller to classify the pool as reconstructible from its configured central artifact source. Under exclusive repository mutation access, refuse if any workspace borrows the pool, any live retention handle exists, or any unresolved operation/import/export depends on its objects. Drain readers and object processes. Persist eviction intent, move only the managed pool into private trash, increment storage generation, clear derived caches/observations, durably mark the pool absent, then reclaim trash. Keep binding, operation journal, and historical receipts. `pool.json` records local storage generation and lifecycle separately from binding generation. A crash resumes that local sequence without deleting another generation's pool. Later import reinitializes an empty pool with the same binding and a new storage generation. Rehydration uses a new import operation and newly identified transfer; replaying a released transfer's historical receipt does not recreate its former retention handle. Bundle bytes can still be reused through their digest. The worker must release protected handles through normal APIs before eviction; age or disk pressure cannot override those guards. Central retained pools are outside this automatic worker cache operation.

Back up journals, bindings, retained Git objects/refs, and worker workspace files that have not been handed off. A central crash can recover from its durable volume; loss of that volume requires the installation backup. Remote-published code remains recoverable from the remote, but unsubmitted code and historical intent cannot be reconstructed from it. Restore runs doctor before allowing new mutations and quarantines unresolved writes for reconciliation.

## 14. Performance and observability

### 14.1 Acceptance targets

Performance is a release gate alongside correctness. The following initial targets apply on a declared Linux reference machine using the persistent socket service and the reproducible workloads below. They are not measurements or promises about arbitrary machines/repositories. A miss blocks the performance acceptance claim until implementation improves or an explicit design revision changes the target with evidence. Do not silently substitute the earlier, looser warm-read budgets.

| ID | Required result |
| --- | --- |
| P1 Warm latency | At 1, 8, and 32 clients, warm bounded immutable reads have p95 <=5 ms and p99 <=20 ms; 100-item warm batches have p95 <=25 ms and p99 <=100 ms. Measure client send through complete decoded response, including socket transit and server queueing. |
| P2 Comparative speedup | At 32 clients, the repeated-read workload has at least 10x lower p95 latency and 5x higher sustained successful logical-query throughput than the equivalent process-per-query Git baseline. Report individual operation classes as well as the aggregate. |
| P3 Useful throughput | Sustain at least 1,000 successful single-item queries/s for 60 seconds at 32 clients while meeting P1, with zero errors/timeouts/overload refusals in this within-capacity run. Batch items count as logical queries only in separately reported batch runs. |
| P4 Shared work | Fully warm hits execute zero Git commands, including zero exchanges with existing Git readers, and zero remote calls. A barrier-controlled burst of 32 identical cold reads computes once. Equivalent concurrent syncs that share a scope and freshness requirement perform one fetch. |
| P5 Mixed-load isolation | With 32 read clients and four admitted heavy operations across at least two repositories, warm single-read p99 remains <=50 ms and successful read throughput stays >=80% of the read-only run at the same offered load. A stalled remote fetch cannot hold the local read path; queued mutations do not block unrelated cached reads. |
| P6 Miss-path overhead | For unique local reads with cold aq-git caches and warm filesystem caches, p95 per operation class is no worse than 2x the equivalent Git baseline plus 2 ms at one client. Report truly cold-start/filesystem measurements separately. |
| P7 Bounded service | Cache bytes, queued requests, children, and artifact storage stay within advertised configured bounds. During a 30-minute churn/overload run, show eviction, bounded admission, and recovery to P1 within 5 seconds after excess load/heavy work stops and the declared working set is rewarmed. Report peak RSS and steady-state drift; no growth proportional to completed read count is acceptable. |

Cached results still require the same repository generation, exact inputs, semantics, and applicable availability rules. No target permits stale observations to masquerade as current, missing objects to become negative facts, skipped fsyncs, or weaker expected-old publication checks.

### 14.2 Workloads and comparison rules

Commit seeded fixture generators and a machine-readable workload manifest before tuning. Use at least two repositories, a 100,000-commit/10,000-ref history with branches/merges, and a separate 10,000-file workspace. The repeated-read mix is 40% commit inspection, 40% ancestry (both true and false), 10% bounded tree listing, and 10% bounded diff summaries. Limit each result to 64 KiB and tree/diff summaries to 100 entries; keep a declared working set of at least 1,000 distinct query keys within the configured cache. Batch responses stay within 1 MiB for the benchmark. Publish seeds, query distributions, response sizes, cache budgets, warmup, and all raw results. Large patches, full traversals, and oversized responses have separate workloads.

The mandatory standalone baseline uses installed Git directly against identical local objects, matching query results/options and freshness. It starts one Git process per logical read where that suffices, avoids shell and CLI formatting overhead, and uses the same concurrency and filesystem-cache conditions. It performs no artificial per-read fetch. Also report an optimized persistent/batched Git baseline to distinguish process-amortization gains from cache/index gains; P2's numerical gate applies to the process-per-query baseline, not an invented claim of 10x over every Git consumer. Compare a representative existing AQ access path when available, but neither sibling code nor a deployed AQ service is required for standalone release. Claim a speedup over that path only with its own evidence.

Add an agent decision-loop scenario: 32 consumers share an explicit ref observation, issue batched commit/ancestry/diff reads for pinned revisions, then repeat while a controlled writer advances refs. Use the same synchronization schedule and freshness requirements in both implementations. Check identical answers at each observation boundary and measure complete round latency, sync count, Git work, and bytes. Separate the remote refresh latency from the local decision work; no simulated network delay may inflate P2. Include missing objects, generation changes, cache eviction, and retained-resource release. The fake coordinator supplies the scenario without AQ.

Run P5 with declared export, import, and candidate workloads plus a barrier-stalled fetch; use separate repositories/artifacts where serialization requires it. Measure both same-repository and cross-repository read interference. Record workload sizes and progress, rather than satisfying the test by never admitting heavy work. Run fixed-arrival-rate tests as well as closed-loop clients so queueing is measured from intended arrival and slow responses do not disappear from the sample. Report all successes, errors, refusals, and timeouts separately; failed work never contributes to throughput gains. Repeat measured runs at least five times and publish per-run percentiles and spread, with no selection of only the fastest run.

Workspace checkout, cold bootstrap, candidate merging, artifact transfer, and publication must have separately reported latency/CPU/I/O/byte budgets on named fixtures before pilot adoption. Reused-worker preparation must demonstrate no full-history transfer when acknowledged prerequisites suffice; no-change handoff uses reference-only transfer when the receiver retains the source. Do not promise a generic millisecond bound on arbitrary repository sizes or remote writes.

### 14.3 Instrumentation

Record p50/p95/p99, successful logical throughput, error rates, cache hits/misses/evictions, single-flight joins, Git spawns and batch exchanges, remote calls, bytes transferred, lock/queue waits, and phase timings. Measure CLI startup separately from persistent-client latency. Record CPU, peak/steady RSS, disk usage, reference hardware, filesystem, Git/Rust versions, and durability configuration. Cache memory and admission limits are reported through capabilities; eviction may affect performance but never weaken retention. Performance runs remain separate from the fast correctness suite.

Structured logs correlate request, operation, repository, workspace, candidate, and transfer IDs. Include operation phase and timing breakdown. Avoid source content and credentials. `doctor` distinguishes unavailable data from corruption and pending writes from failed writes. Metrics must be readable locally without a monitoring server; an optional metrics adapter may follow later.

## 15. Verification and release acceptance

| ID | Demonstration |
| --- | --- |
| V1 | Run all correctness tests using only Rust, Git, temporary files, and local sockets. |
| V2 | Rebuild every derived cache and obtain the same fixed-input answers without modifying authoritative refs. |
| V3 | Show ordinary status/exact reads do not fetch; batch requests share computations and warm hits launch no Git children. |
| V4 | Create concurrent independent workspaces, preserve modes/symlinks/deletions, and refuse unsafe retirement. |
| V4a | Refuse pool eviction with live users/retention; evict a disposable worker pool, preserve history, and rehydrate at the same repository binding. |
| V5 | Transfer base and result bundles between isolated central/worker roots, including missing prerequisites and corrupt manifests. |
| V6 | Delete/stop the worker only after a central retention receipt and still construct the candidate. |
| V7 | Retry identical mutations across restarts; reject same-ID/different-payload reuse. |
| V8 | Build deterministic frozen candidates, expose conflicts, and keep later source changes out of existing candidates. |
| V9 | Reject moved-target and non-fast-forward publication; distinguish successful update, up-to-date observation, rejection, and uncertainty. |
| V10 | Kill processes at every persisted mutation boundary, including owner-only SIGKILL with surviving Git children; fence replacement ownership until cleanup is established, without false success, lost retained objects, or automatic uncertain push replay. |
| V11 | Refuse worker publication, unsupported repository features, incompatible formats, quota overruns, and unauthorized paths. |
| V12 | Demonstrate P1–P7 with raw comparative, warm/cold, agent-loop, mixed-load, and churn evidence; disclose every missed target and block the performance acceptance claim until resolved. |
| V13 | Test previous compatible protocol clients against the release and verify installation/update/recovery documentation. |
| V14 | Advance/adopt bindings with drained transitions, stale-input refusal, historical replay, and crash recovery; retained work blocks changes without being deleted or relabeled. |
| V15 | Exercise the retention dependency/release lifecycle through worker cleanup and eviction, shared candidate inputs, conflicts, unresolved publication, concurrent use, and interrupted release. |

The standalone baseline is complete when these demonstrations pass, the CLI and local service use the same domain operations, and the release includes a binary, protocol schema/fixtures, state-format declaration, test evidence, and benchmark report. Agent Q's authenticated polling/upload adapter is a separate integration deliverable; its absence does not prevent standalone testing, but it must exist before claiming end-to-end deployed AQ worker support.

## 16. Follow-on decisions and references

Fixed baseline decisions are Rust, installed Git, no database dependency, local Unix socket, complete SHA-1 repositories, private workspace Git directories with object reuse, outbound AQ artifact transfer, existing remote authority, and one central publisher. Implementation may not silently replace these choices to satisfy a test.

Pilot decisions still requiring installation-specific values are reference hardware/workload budgets, allowed repository features, actual artifact quotas, volume/backup placement, remote credential profile, and destination branch protections. Features deferred to later specs include signed commits, LFS/submodules, sparse/partial storage, native non-Linux support, alternative agent clients, direct worker Git transfer, network aq-git service, and distributed publication fencing.

Primary implementation references:

- [Git bundle](https://git-scm.com/docs/git-bundle): full/incremental object transfer and prerequisites.
- [Git merge-tree](https://git-scm.com/docs/git-merge-tree): three-way merges without a checkout or index.
- [Git update-ref](https://git-scm.com/docs/git-update-ref): expected-value updates and local ref transactions.
- [Git push](https://git-scm.com/docs/git-push): explicit expected-old leases and server outcomes.
- [Git cat-file](https://git-scm.com/docs/git-cat-file): batch object access.
- [Git clone](https://git-scm.com/docs/git-clone): shared-object/alternate retention implications.
- [Agent Q foundation](../../../aq-server/docs/specs/0001-infrastructure-foundation.md) and [aq-graph integration](../../../aq-graph/docs/aq-integration.md): sibling context only; relative links assume side-by-side checkouts.
