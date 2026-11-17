# Operating aq-git

aq-git runs on Linux, Linux containers, and WSL2 using a Linux local filesystem. Install the Rust-built `aq-git` executable and `/usr/bin/git`, or select an administrator-controlled absolute executable with `--git-executable`. Use a filesystem with working advisory locks, atomic rename, and file/directory synchronization. NFS/SMB state roots and native Windows/macOS durability are unsupported. No AQ installation or remote account is needed for local operation.

Create a private state directory for one OS identity. Do not grant worker processes access to the runtime state, socket, shared pool, or credentials. Writable workspaces are separate directories. This trusted local interface does not isolate hostile processes sharing unrestricted OS credentials.

```sh
install -d -m 700 /path/to/aq-git-state
cargo build --release -p aq-git-cli
target/release/aq-git --state-dir /path/to/aq-git-state --json capabilities
```

Every normal direct invocation uses a lifecycle guard. The guard retains the root advisory lock, records an active incarnation before starting the engine, adopts descendants as a Linux subreaper, and terminates/reaps descendants before recording clean ownership. A second owner returns `state_busy`; use the existing service socket. Engine loss does not authorize replacement mutations while descendants remain. Guard loss leaves a durable fence that returns `recovery_blocked` even if the advisory lock becomes available.

Start the optional foreground service and use its Unix socket for repeated requests:

```sh
target/release/aq-git --state-dir /path/to/aq-git-state --listen /path/to/aq-git-state/runtime.sock serve
target/release/aq-git --socket /path/to/aq-git-state/runtime.sock --json capabilities
```

The wire format is versioned newline-delimited JSON. For operations with parameters, submit the complete envelope through standard input or `--input FILE`. Mutations require a fresh operation ID, repository ID, and expected binding generation. Reuse the same operation ID and identical payload after an interrupted response; changed mutation inputs require another ID. Terminal acknowledgements are historical records, not a fresh remote observation or renewed retention promise. Inspect current retention before using an old resource.

```sh
printf '%s\n' '{"protocol_version":1,"request_id":"example","operation":"capabilities","params":{}}' |
  target/release/aq-git --socket /path/to/aq-git-state/runtime.sock --json request
```

Registration initializes an empty managed pool without remote contact. An interrupted `repo.binding-update` keeps that repository’s admission fenced in the current owner until the exact transition is completed. Retry the same operation ID and identical payload, or restart the owner to finish its durable transition before admission. Unrelated repositories continue normally; rejected fresh mutations receive no journal intent. A proven failure before any transition intent was installed leaves the old binding authoritative. Central bindings may configure explicit allowed refs and publication targets; worker bindings carry no remote credentials. Synchronization is explicit. Exact OID reads never fetch. A failed refresh preserves the previous observation, and freshness requirements return a stale result rather than silently contacting a remote. Baseline repositories use complete SHA-1 history. Submodules, shallow/partial history, custom checkout/merge attributes, and credential/signing workflows without an implemented profile must be refused for affected operations. Consult actual `capabilities`; unsupported features do not become available through repository configuration.

A workspace is privately writable, but callers must stop writers before checkpoint/retirement and supply expected generation/HEAD. A checkpoint or transfer handle retains work independently of the workspace. Retire clean workspaces only after a retained checkpoint exists. Release handles explicitly using `retention.release` with `handle_id`, `expected_generation`, the inspected exact `refs`, and a reason. Live dependents block release. Explicitly releasing an independently pinned pending local creation durably cancels that creator before removing its pins, preventing a retry from recreating the resource. Publication uncertainty remains protected by its dependency and is never automatically abandoned.  Released tombstones and operation receipts remain; release does not erase history. Pool eviction is worker-only and requires expected storage generation, `reconstructible:true`, and a reason. It refuses active handles, workspaces, or unresolved operations. A new transfer can rehydrate an evicted pool; historical transfer replay cannot.

Artifact export accepts `source`, `base`, `retention_handle`, `confirmed_prerequisites`, optional `reference_only`, and opaque `caller_correlation`. OIDs use `{"algorithm":"sha1","hex":"<full-40-character-oid>"}`. Confirmed prerequisites mean the receiver explicitly acknowledged retaining those histories. A reference-only export requires confirmation of the exact source. An export returns its artifact handle and independently retained transfer handle. Preserve the producing handle until transport/handoff policy permits release.

The trusted transport copies complete `manifest.json` and `objects.bundle` files from `state/artifacts/<export-operation-id>/` to the receiver's configured staging root at `state/artifacts/incoming/<new-UUID>/`. Reference-only manifests omit the bundle. The import request contains only `artifact_handle`; manifests cannot select paths, configuration, hooks, remotes, or destination branches. Never expose partial transport downloads as completed staged artifacts. Independent installations must use the same logical repository identity and binding generation.

Import checks identity, size, SHA-256, exact advertised transfer ref/source, actual prerequisites, object connectivity, base ancestry, and supported tree features in quarantine. It snapshots verified bytes before invoking Git. Acceptance pins the source in the receiving managed pool and persists a receipt containing receiver installation ID, transfer ID, manifest digest, source, and retention handle. A digest detects corruption, not producer authorization; authentication and attempt acceptance belong to the transport/caller. Missing prerequisites require a fuller bundle. Retrying the same transfer manifest returns historical acceptance and current retention state without recreating released pins.

Default artifact limits are 1 GiB compressed bundle bytes, 10 GiB additional managed storage, and 256 MiB disk reserve. An administrator can install `state/artifact_limits.json` while admission is stopped:

```json
{"max_bundle_bytes":1073741824,"max_expanded_bytes":10737418240,"disk_reserve_bytes":268435456}
```

Compressed output is streamed with an exact byte cap. Import monitors cumulative files during Git execution and applies inherited per-file size limits. Aggregate disk monitoring samples between Git execution steps and can temporarily overshoot while Git writes; it is not a hard filesystem quota. Quota failures cannot acknowledge a retention receipt. Use a filesystem-enforced quota when a strict aggregate disk ceiling is required. Quarantine is removed after normal success/rejection; a killed process can leave an operation-owned directory for replay/inspection. Transport staging and completed output artifacts require caller-managed bounded storage; deleting artifact files does not release source pins.

Git children have bounded concurrent admission, output, stdin, execution time, and Linux address space. The default address-space ceiling is 1 GiB per child, including persistent object readers; decompression that exceeds it fails without an acceptance receipt. Administrator overrides in `state/git_limits.json` use the fields reported in `capabilities.process_limits`; unspecified fields keep their defaults. For example:

```json
{"max_children":16,"max_address_space_bytes":1073741824,"max_output_bytes":16777216,"timeout":{"secs":300,"nanos":0}}
```

Request budgets further shorten the configured process ceiling. Ordinary subprocesses wait in a FIFO with at most 64 queued callers, within the service’s 64-request admission bound. Queue time consumes the same request/command deadline; an expired waiter starts no Git child. Persistent-reader creation remains nonblocking, with idle readers capped below the process limit and transient readers used when only one child is allowed. The service uses four I/O workers; its advertised read, heavy-work, and output admission bounds remain separate. The owner snapshots the selected executable identity and checks it before every new Git child, including persistent-reader startup. Replacing that executable while the owner is live requires draining and restarting the owner; new spawns refuse until a fresh runner can perform the compatibility probes. Existing immutable cache hits do not add executable-stat checks.  Request deadlines constrain commands and persistent object reads. The managed pack profile fixes `pack.threads=2`, `pack.windowMemory=16m`, and `pack.deltaCacheSize=16m`, keeping pack work within the unchanged address-space ceiling. The local execution profile clears inherited Git/environment injection, disables prompting, user/global configuration, replacement refs, hooks, automatic maintenance, and executable external protocols, and requests object/reference fsync. Public errors do not include authenticated URLs or helper stderr. Remote credential profiles beyond the implemented feature set must not be assumed available.

After a guard crash, stop the old execution environment and all descendants through a trusted operator/runtime mechanism. Preserve the active incarnation record and diagnostics. Clear only that exact incarnation with `recovery clear-fence`, an explicit reason, and cleanup evidence; this is an audited local attestation. Do not infer cleanup from a missing PID or delete `owner.lock`/incarnation files. Local process cleanup never proves that an already dispatched remote push had no effect. Inspect the command's documented input envelope and retain the audit operation ID.

Completing or replaying an older publication receipt releases only that operation’s own destination hold. A successor publication’s hold and generation remain unchanged, including when an old terminal journal acknowledgement is recovered. Historical replay does not establish current target availability.

An indeterminate publication is never automatically pushed again. Reconcile through a read-only authoritative remote observation. Current absence or return to the old revision cannot prove a timed-out push never applied. Preserve candidate dependencies and journal evidence until an explicit reconciliation/resolution permits further target mutations. Historical acknowledgements remain historical even after external rewrites.

Stop admission and shut down the owner before backups, upgrades, or restores. Back up bindings, journal, incarnation evidence, retention records/refs, retained Git objects, workspaces, and artifacts required by transport policy together. Derived query caches may be discarded. Journal deletion loses idempotency and is not cache clearing. Unknown future state formats refuse startup; never install an older binary on an unsupported format. Restore into an isolated root, inspect ownership evidence, object connectivity, exact retention refs, and unresolved operations before permitting writes.

`doctor` and `maintenance` expose implemented local checks. Automatic object pruning and the rolling operation journal ([spec](specs/0001-aq-git.md) §§4, 13.2; aligned 2026-11-13 with [aq-server architecture 0006](../../aq-server/docs/architecture/0006-records-are-rolling.md)) are not implemented yet: today nothing prunes objects or journal records. Do not run arbitrary pruning, replace managed refs, or edit pool configuration externally. SIGKILL tests demonstrate process-crash ordering. Host power-loss durability requires a separate VM/filesystem crash rehearsal and is not established by these tests. Performance and hosted compatibility require separate evidence; passing local correctness tests does not establish either.

## Local release manifests and upgrades

Generate a development manifest for an existing binary without committing or publishing anything:

```sh
mkdir -p .local
scripts/release-manifest --binary target/release/aq-git --output .local/release-manifest.json
```

The manifest records the executable's SHA-256/size, workspace version, selected Rust toolchain, Git requirement and observed version, independent protocol/state/artifact/merge versions, and supported Linux/local-filesystem profile. Source identity is a SHA-256 over the actual sorted file inventory, including nonignored untracked implementation files, file modes and symlink targets. `HEAD` is recorded separately and is never substituted for current source. A checkout before its first commit is explicitly `unversioned`; modified or untracked source is `dirty`. `--require-clean` refuses both states. Ignored files and Git metadata are excluded and the policy is recorded. Stop edits before generation; the script rejects detected concurrent changes but does not claim an atomic filesystem snapshot.

Every generated manifest is `development_not_certified`. Hashing an existing binary and the current source independently does not prove that binary was built from that snapshot; `binary_source_binding.status` remains `not_attested`. Keep build logs, exact toolchain/configuration, test evidence and source snapshot with any candidate installation. The script does not build, package, publish, sign, install, or certify an artifact. Put output outside the source tree or in an ignored directory to avoid a self-referential source hash. Source inventory hashes and executable hashes can be compared between local copies; preserve the full manifest with the candidate binary.

For replacement or rollback, first stop new caller admission, drain or explicitly classify active operations, terminate/reap owned Git/helper descendants, and stop the service owner before replacing its executable. Preserve any active-incarnation fence and unresolved publication evidence. Take a consistent backup of bindings, journals, retention records/refs, retained objects, workspaces and required artifacts before changing versions. Inspect the candidate's format support and capability/behavior probes against an isolated restored copy before reopening writes. Unknown future state formats must refuse startup; a lower version number is never permission to migrate or delete records.

Rollback requires an older binary that explicitly supports every persisted format and in-flight operation/profile involved. Restore a compatible consistent backup into an isolated root when that cannot be established; never point an unsupported older binary at upgraded state, erase operation IDs, or clear an ownership fence to force startup. Local rollback cannot undo a remote publication. Reconcile indeterminate pushes separately, and retain historical acknowledgements even if a destination later moves. Upgrade/rollback rehearsals, previous-compatible-client checks after the first release, hosted behavior, power-loss durability and full performance acceptance remain release gates rather than properties granted by a checksum.

## Remote credential profiles

Provision `credential_profiles.json` in the private state root, or pass `--credential-profiles /absolute/path.json` to `serve` and direct commands to load an operator-held file such as a read-only vault mount, before selecting a name in a central repository binding. When the flag is given, the state-root file is ignored. The file is read on every authenticated call, so a replaced file takes effect without a restart. It must be a regular file owned by the service identity or root that group and others cannot write. This is administrator configuration, not a remote API for storing or rotating secrets. The file maps names to profiles containing paths only:

```json
{
  "github-deploy": {
    "kind": "ssh_deploy_key",
    "remote_url": "ssh://git@github.com/example/project.git",
    "private_key_path": "/private/aq-git/project-deploy-key",
    "known_hosts_path": "/private/aq-git/github-known-hosts"
  },
  "github-token": {
    "kind": "https_token",
    "remote_url": "https://github.com/example/project.git",
    "token_path": "/private/aq-git/project-token"
  }
}
```

The URL must exactly match the binding. HTTPS token URLs cannot contain query strings or fragments. Credential paths must be absolute regular files owned by the service identity, with no final symlink. Private keys and token files must have no group/other permissions (for example `0600`); pinned known-hosts files must not be writable by group/others and may be owned by the service identity or root. Keep parent directories private and administrator controlled. The runtime validates files at invocation time; administrators must not replace them during an active operation. Profiles and material are not copied into journals, bindings, workspaces or artifacts. Bindings expose only the selected profile name.

SSH uses OpenSSH at `/usr/bin/ssh`, an explicit deploy key, `IdentitiesOnly`, no agent, no inherited SSH configuration, batch authentication and strict pinned host verification. Provision the known-hosts entry from an independently verified host key; there is no trust-on-first-use or host-key update fallback. Keys requiring an interactive passphrase are unsupported. The host entry must match the remote hostname and optional nondefault port.

HTTPS reads a one-line printable token (an optional final LF is allowed) from its private file. Git's invocation-scoped credential helper returns `x-access-token` and the token only for the configured HTTPS host and repository path. It handles only credential `get`; store/erase do not persist anything. User/global helpers, interactive prompts and inherited Git tracing are disabled. HTTP redirects are disabled so credentials cannot follow a different destination. Normal TLS verification remains enabled. Token permissions and branch protection on GitHub remain administrator responsibilities.

Both transports apply to advertisement, fetch, push and reconciliation. New branches use the explicit absent-ref lease documented in [the API](api.md#remote-authentication-and-branch-creation). A failed authentication or host-key check returns `remote_auth_failed`; a dispatched publication remains fenced until reconciliation or explicit resolution. Never interpret a credential error as permission to resend a pending push.
