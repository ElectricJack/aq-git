# aq-git

aq-git is the central Git service of AQ v2. It keeps the repository state that AQ needs behind one local Rust executable: repository bindings and sync, exact-revision queries, isolated workspaces and checkpoints, bundle handoff between worker and central pools, frozen merge candidates, and conditional publication to the configured remote, with recoverable operation records. The supervising runtime, which is aq-server, starts it as a service and talks to it over a Unix socket. It can also expose a second Unix socket that serves Git smart HTTP for task attempts, reachable only through aq-server. For the overview of AQ v2 and the other components, see the umbrella repository, [aq-platform/agent-queue](https://github.com/aq-platform/agent-queue), which includes this repository as the `aq-git/` submodule.

Git remains authoritative for committed code and published refs. aq-git amortizes repeated queries through a persistent local service, exact-revision caching, shared computations, and persistent Git object readers. It also works standalone through a CLI and a direct mode, and its correctness tests need no AQ process, database server, Docker daemon, broker, or hosting account.

## Build and verify

Requires Linux, Rust 1.96.1, and Git 2.43 or later with the required features. The pinned Rust toolchain is selected by `rust-toolchain.toml`. The executable defaults to `/usr/bin/git`; pass `--git-executable` with an absolute path to use another Git. The remote transport tests also need Python 3, OpenSSL and an OpenSSH server (see [development](docs/development.md)).

```sh
cargo build --release
scripts/check-fast
scripts/check-all
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

The executable is `target/release/aq-git`. Choose exclusive direct execution or a persistent service:

```sh
aq-git --state-dir /private/aq-git --json capabilities
aq-git --state-dir /private/aq-git --listen /private/aq-git.sock serve
# optional, `serve` only: add --git-http-socket /private/aq-git-http.sock for the smart-HTTP endpoint
aq-git --socket /private/aq-git.sock --json capabilities
```

Mutation requests use explicit operation IDs and generations; JSON input is a complete versioned request envelope. See [the API guide](docs/api.md) for request examples and operation fields. Consumers should use the socket service to share caches and Git readers across requests.

## Scope and evidence

v0.2.0 is released and correctness-qualified; [release status](docs/release-status.md) records the evidence and the remaining gates. The performance targets and host power-loss durability are not certified, and the capabilities response reports `performance_acceptance: not_certified`. [Benchmark documentation](docs/benchmarking.md) identifies reproducible commands, actual evidence, and missing acceptance runs.

Supported storage is a private local Linux filesystem. The current workspace/merge profile rejects attributes, submodules, LFS-managed checkout, shallow/partial history, and required signing. Object pruning, distributed publication fencing, and native Windows/macOS are not supported. Remote credentials come from file-backed, URL-scoped profiles; live hosting-provider policy is not qualified. This repository holds no AQ authentication, polling, upload or assignment logic: those belong to the consumers, as described in the integration guide. Check credentials and optional capability limits against the API and operations guides before deployment.

- [Design specification](docs/specs/0001-aq-git.md)
- [Implementation plan](docs/plans/0001-aq-git-implementation.md)
- [Development and focused checks](docs/development.md)
- [Operations and recovery](docs/operations.md)
- [AQ integration boundary](docs/aq-integration.md)

Related components in an umbrella checkout: [aq-server](../aq-server/README.md), [aq-worker](../aq-worker/README.md), and [aq-graph](../aq-graph/README.md). aq-server is the consumer the code is written for (see the git-http module and the [integration guide](docs/aq-integration.md)); these links are context, not build dependencies.
