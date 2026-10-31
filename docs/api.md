# aq-git protocol v1

The executable accepts the same request envelope directly or over a private Unix socket. Read `capabilities` before selecting operations. The operation list describes this build; unsupported operations return `unsupported_feature`. Protocol, state, artifact and merge-profile versions are independent. Performance acceptance remains unmeasured until the benchmark gate passes.

```sh
cargo build --release
# Direct invocation owns the state root exclusively.
target/release/aq-git --state-dir /absolute/state capabilities --json
# A service is started explicitly and owns the same root for its lifetime.
target/release/aq-git --state-dir /absolute/state --listen /absolute/aq-git.sock serve
# A thin client never starts a daemon implicitly.
target/release/aq-git --socket /absolute/aq-git.sock --input request.json request --json
```

Commands can also name their operation: `repo register`, `workspace checkpoint`, `candidate create`, `operation reconcile`. `--input FILE` contains a complete request envelope, including a matching operation; `--input -` reads stdin. `request` reads stdin by default. `--json` writes response envelopes to stdout; the default human mode prints named result fields and useful next commands. Diagnostic text goes to stderr. Exit codes are 0 for success (including false ancestry and a candidate conflict result), 2 for invalid input/protocol/configuration, 3 for domain or precondition refusal, 4 for temporary resource/availability failure, 5 for indeterminate mutations, and 1 for unexpected internal failure. `--json` also renders startup failures as response envelopes. `--git-executable /absolute/path` selects a trusted administrator-controlled executable, and is excluded from request parameters.

A request is one JSON object followed by a newline:

```json
{"protocol_version":1,"request_id":"query-42","operation":"contains","repository_id":"11111111-1111-4111-8111-111111111111","params":{"ancestor":{"algorithm":"sha1","hex":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"descendant":{"algorithm":"sha1","hex":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}
```

Envelope fields are `protocol_version`, `request_id`, `operation`, optional `repository_id`, `operation_id`, `expected_binding_generation`, `params` (default `{}`), and optional positive `deadline_ms`. Unknown envelope fields and unknown mutation parameters are refused. IDs are canonical lowercase hyphenated UUIDs, except request IDs, which are caller strings of 1–128 characters without control characters. Object IDs require `{algorithm:"sha1",hex:"<40 lowercase hex digits>"}`. Full refs are validated independently of binding allowlists. No shell fragments, revision expressions, arbitrary Git flags, remote URL overrides or credentials are accepted as operation inputs.

Repository mutations require an operation UUID and expected binding generation; registration uses zero for local absence. Read requests may specify a generation to reject stale bindings. Root capability/service/recovery requests omit repository fields. Mutations use durable operation identities across the entire installation. Exact retries keep the same operation ID and payload; changed payloads return `idempotency_conflict`. Request IDs and deadlines can change on retry.

Responses contain `protocol_version`, `request_id`, `ok`, and exactly one of `result` or `error`. Mutation responses include `operation_id` and journal `operation_state` where available. `resource_status` contains separately timestamped current local retention evidence; historical result receipts remain unchanged. Error objects contain stable `code`, concise `message`, structured `details`, and `retry` (`safe_same_operation`, `refresh_inputs`, `reconcile_first`, or `manual`). False ancestry is an ordinary successful result. Clients reject unsupported response versions and contradictory success/error envelopes. An explicit null success result is valid; a missing result is not. Present operation IDs must match the request, while a pre-dispatch failure may omit one. For a malformed or incompatible mutation reply, the CLI preserves the caller’s operation ID and reports its outcome as unknown rather than assuming no write occurred.

| Operation | Parameters |
| --- | --- |
| `capabilities`, `service.metrics`, `doctor` | `{}`; no implicit remote access |
| `repo.register` | `{binding:{identity,mode,binding_generation?,object_format?,allowed_refs?,allowed_targets?,remote_url?,credential_profile?,allow_file_transport?,feature_profile?}}`; the flat binding form is also accepted |
| `repo.show` | `{}` |
| `repo.binding-update` | `{binding:<complete replacement including repository_id>,reason}`; drained transition with live-resource blockers |
| `repo.sync` | `{refs?:[full_ref]}`; explicit remote synchronization |
| `repo.observation` | `{refs?:[full_ref],max_age_ms?:integer}`; stale observations never trigger a fetch |
| `inspect`, `tree` | `{oid}`; tree additionally accepts `limit`, `recursive` |
| `diff` | `{base,target,limit?}`; exact typed OIDs |
| `contains` | `{ancestor,descendant}`; exact typed commit OIDs |
| `query.batch` | `{items:[{operation,params}]}`; up to 100 independent immutable queries |
| `workspace.create` | `{base}` |
| `workspace.inspect` | `{workspace_id}` |
| `workspace.checkpoint` | `{workspace_id,expected_generation,expected_head,quiescent:true,mode?:"head"|"all",commit?:{name,email,message}}`; `all` requires explicit commit metadata |
| `workspace.retire` | `{workspace_id,expected_generation,expected_head,quiescent:true}`; clean retained checkpoint required |
| `artifact.export` | `{source,base,retention_handle,confirmed_prerequisites?:[OID],reference_only?:bool,caller_correlation?:value}` |
| `artifact.import` | `{artifact_handle}`; a previously staged manifest/bundle handle |
| `candidate.create` | `{base,sources:[{oid,retention_handle}],commit:{name,email,message},merge_profile?:"git-ort-v1",supersedes?:UUID}`; up to 64 frozen ordered sources |
| `candidate.show` | `{candidate_id}`; returns ready/conflicted state and frozen evidence |
| `publish` | `{candidate_id,target,expected_old}`; ready retained candidate, configured central existing branch, explicit typed old OID |
| `operation.show` | `{target_operation_id}` |
| `operation.reconcile` | `{target_operation_id,expected_generation}`; new command operation UUID, explicit remote observation, no push |
| `operation.resolve` | `{target_operation_id,expected_generation,disposition:"abandon"|"permit_new_operation",reason,old_publishers_stopped:true,evidence}`; nonempty reason and string/object evidence |
| `retention.inspect` | `{handle_id?,after?,limit?}` |
| `retention.release` | `{handle_id,expected_generation,refs:[{name,oid}],reason}`; here refs use exact private ref names and raw 40-digit hex values returned by inspection |
| `repo.cache-evict` | `{expected_storage_generation,reconstructible:true,reason}`; worker-only after explicit retention release |
| `maintenance` | `{kind?}`; inspect capabilities and named refusals before selecting work |

Candidate IDs equal their creation operation IDs. `ready` results expose `candidate`, `clean_prefix`, `retention_handle`, Git version, profile and frozen sources. `conflicted` results expose a null candidate, zero-based `conflict_step`, retained clean prefix and bounded paths encoded as `{encoding:"hex",value}`. Conflict paths are capped at 10,000 entries and 1 MiB of encoded path data; `conflict_paths_truncated` and `conflict_path_count` disclose omissions and the number observed. A conflict is an operation result, and cannot be published. New source order or repair requires a new operation ID. The current feature profile refuses `.gitattributes`, gitlinks and unsupported checkout features rather than changing their semantics.

Publication outcomes are `applied`, `already_present`, `rejected`, and an `operation_indeterminate` error for unknown dispatched outcomes. Only parsed successful target-update output creates `applied`. Up-to-date or containment observations produce `already_present`, with a timestamp and no causation claim. Missing acknowledgement blocks the target, retains the candidate, and never causes automatic re-push. Reconciliation observes current presence; an unchanged old target preserves uncertainty. Audited resolution preserves historical ambiguity and permits a separately identified later operation.

The transport bounds requests at 1 MiB, responses at 8 MiB, connections at 64, concurrent immutable requests at 32, and concurrent mutation computations at four, and total admitted requests (including response output) at 64. Computation slots are released before response bytes become visible; a separate admission permit bounds queued output. A connection may pipeline requests, and responses may arrive out of order; correlate by `request_id`. Frames are bounded while reading, including frames without a newline. A partial frame has a 30-second assembly deadline. A silent connection closes after 30 idle seconds only once its pending requests and responses have finished; a stalled response writer is bounded at ten seconds. Overload returns a structured refusal. Read admission remains available while mutation slots are full. Synchronization has a separate 32-caller lane for coalescing, while its leaders share the core heavy-operation budget with other heavy work. Clients must reconnect and replay durable mutations after a disconnect rather than inventing a new operation identity.

The service binds a mode-0600 socket. Existing live sockets are never replaced. Stale removal requires this root's durable matching path/device/inode ownership record; an unknown socket is refused. Socket reuse is verified before shutdown removal. The state root stays private to the runtime OS identity.

`recovery clear-fence` is direct-only and intentionally bypasses normal guarded startup. Supply a complete envelope with its own operation UUID, no repository fields, and params `{expected_incarnation,reason,evidence,old_environment_stopped:true}`. The operator first stops the old execution environment and gathers evidence. Clearance checks the exact active incarnation and journals the attestation; it does not prove or resolve any remote push outcome. Normal commands use an internal same-executable guard: the guard owns the lock, records an active incarnation, supervises the engine as a Linux subreaper, kills/reaps surviving descendants, records clean evidence, then releases ownership. A missing guard with no cleanup proof returns `recovery_blocked`.

Schemas live in [`protocol/v1`](../protocol/v1/), with request fixtures under [`examples`](../protocol/v1/examples/). The envelope references `operations.schema.json` for every advertised operation, including nested bindings, OIDs and commit metadata. Regenerate declarations and deterministic request/response/error fixtures with `python3 protocol/generate-schemas.py`. `cargo test -p aq-git-protocol --test contracts` checks the maintained schema vocabulary and fixture catalogue, and core fixtures verify every advertised handler rejects unknown parameters before invoking Git. This structural checker is deliberately limited to the vocabulary used here; it is not a general JSON Schema implementation. The checked-in documents also validate with a Draft 2020-12 validator. Rust handlers enforce further semantic conditions including ref syntax, allowlists, retention dependencies and ancestry.
