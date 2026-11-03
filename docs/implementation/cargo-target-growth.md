# Cargo target growth measurement

Task `eager-bridge-83`, measured on 2026-11-02 in aq-git AQ `slot-0`, starting at `b90780623c8fb70529c48b907423d3789978ce15`. Toolchain: rustc 1.96.1, Cargo 1.96.1, Linux x86_64.

Both builds used the same dedicated target directory inside this slot. It did not exist before the baseline, and `cargo clean` emptied only that directory between builds. No other slot's target was touched. The fixed command was:

```sh
CARGO_TARGET_DIR="$PWD/.local/cargo-target-measure" CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1 \
  cargo test --locked -p aq-git-core --test state_runner --no-run
```

The baseline used the original manifest and no `CARGO_INCREMENTAL` override. The second build used `profile.dev.debug = "line-tables-only"`, `profile.dev.package."*".debug = false`, and `.cargo/config.toml` with `build.incremental = false`. The test profile inherits these development settings. Measurements were taken immediately after each no-run build, before running tests or building the backtrace probe.

| Measurement | Before | After |
| --- | ---: | ---: |
| Target apparent bytes (`du -sb`) | 311,683,051 | 122,227,158 |
| Target allocated KiB (`du -sk`) | 306,940 | 121,052 |
| `state_runner` executable bytes (`stat -c %s`) | 26,834,648 | 12,573,328 |
| Clean build elapsed time (Cargo report) | 37.37 s | 30.27 s |

Target apparent size decreased **60.78%**. No incremental files were produced by the second build. This is a focused test-target measurement, not a full-workspace size or performance claim.

Validation:

- With the same target directory, `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=1 cargo test --locked -p aq-git-core --test state_runner`: **21 passed**, none failed or ignored.
- A temporary `aq-git-protocol` example calling `std::backtrace::Backtrace::force_capture()` was built through Cargo's dev profile. Its output resolved `target_backtrace_probe::main` to `./crates/aq-git-protocol/examples/target_backtrace_probe.rs:2:20`. The probe source was removed after verification. This confirms workspace file/line backtraces with the new profile, as described by the [Cargo profile documentation](https://doc.rust-lang.org/cargo/reference/profiles.html#debug).
- `bash -n scripts/check-fast scripts/check-all`: passed. A temporary fake Cargo executable recorded all four calls from both scripts: each ran at the workspace root with `CARGO_INCREMENTAL=0`, even when the scripts inherited `CARGO_INCREMENTAL=1`.
- `cargo fmt --all --check`: passed.
- `scripts/check-docs`: passed; eight cross-repository links were reported as unchecked because sibling checkouts were absent.

`aq test --help` currently invokes pytest, so the Rust checks used foreground Cargo with bounded jobs and test threads. Raw local logs and size readings are under `.local/cargo-target-evidence/` in this slot; the measurements and results above are retained here for review after slot cleanup.
