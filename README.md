# aq-git

A standalone Rust repository interface for Agent Q and other agent hosts. Git remains authoritative for committed code and published refs. aq-git amortizes repeated queries through a persistent local service, exact-revision caching, shared computations, and persistent Git object readers.

The implementation provides a CLI and Unix-socket API for repository bindings and synchronization, immutable queries and batches, isolated workspaces and checkpoints, bundle handoff, frozen merge candidates, conditional fast-forward publication, and recoverable operation records. It requires no AQ process, database server, Docker daemon, broker, or hosting account for correctness tests.

## Build and verify

Requires Linux, Rust 1.96.1, and Git 2.43 or later with the required features. The pinned Rust toolchain is selected by `rust-toolchain.toml`.

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
aq-git --socket /private/aq-git.sock --json capabilities
```

Mutation requests use explicit operation IDs and generations; JSON input is a complete versioned request envelope. See [the API guide](docs/api.md) for request examples and operation fields. Consumers should use the socket service to share caches and Git readers across requests.

## Scope and evidence

This is a development implementation, with release acceptance tracked in [release status](docs/release-status.md). Performance targets are requirements, not a claim that every benchmark has passed. [Benchmark documentation](docs/benchmarking.md) identifies reproducible commands, actual evidence, and missing acceptance runs.

Supported storage is a private local Linux filesystem. The current workspace/merge profile rejects attributes, submodules, LFS-managed checkout, shallow/partial history, and required signing. Object pruning, distributed publication fencing, native Windows/macOS, and deployed AQ transport are not enabled. Credentials and optional capability limits must be checked against the API and operations guides before deployment.

- [Design specification](docs/specs/0001-aq-git.md)
- [Implementation plan](docs/plans/0001-aq-git-implementation.md)
- [Development and focused checks](docs/development.md)
- [Operations and recovery](docs/operations.md)
- [AQ integration boundary](docs/aq-integration.md)

Related projects: [Agent Q](../aq-server/README.md), [aq-worker](../aq-worker/README.md), and [aq-graph](../aq-graph/README.md). These are context links, not build or runtime dependencies.
