# Local Development

Development: local
Build tools: make, cargo, docker

## Workflows:

Developer Loop:
- running `make all` should build the aspec CLI binary using the local Rust/Cargo toolchain
- running `make install` should run `make all` and then install the aspec CLI to /usr/local/bin/


Local testing:
- running `make test` should run all tests in the project that are safe to run on a developer's own machine
- every `make test*` target runs through `tools/isolated-test.sh`: a throwaway `HOME`, no global git config, no inherited `AWMAN_*` or GitHub token variables, stdin from `/dev/null`, and `AWMAN_TEST_ISOLATION=1`, under which awman uses an in-memory keychain and clipboard, never starts daemons through launchd or `systemd --user`, and reaches no network beyond loopback. Unit tests (`cfg(test)`) are always isolated. No test may read or change the developer's real keychain, clipboard, daemons, home directory or git setup
- tests that drive a real container, sandbox CLI, or builtin guest are opt-in: they run only with `AWMAN_TEST_DOCKER=1`, `AWMAN_TEST_APPLE_CONTAINER=1`, `AWMAN_TEST_SBX=1`, or `AWMAN_TEST_BUILTIN=1`; otherwise awman and the tests treat those runtimes as unavailable. `make test-full` sets `AWMAN_TEST_DOCKER=1`; it does not opt into builtin guest execution. Set `AWMAN_TEST_BUILTIN=1` only on a supported host with Apple Hypervisor.framework access or Linux KVM, and use a short, private `AWMAN_BUILTIN_STATE_DIR` for test state and sockets.
- Builtin integration tests must distinguish capability preflight from guest execution. A missing `/dev/kvm`, denied KVM access, missing Apple hypervisor entitlement, unverified target payload, or unsupported host is SKIP/BLOCKED with the concrete prerequisite recorded; it is never a passing boot test. Hermetic backend, archive, registry-wire, and transport tests remain in the ordinary isolated suite.
- Guest network and memory-pressure scenarios add two opt-ins beyond `AWMAN_TEST_BUILTIN=1`: `AWMAN_TEST_BUILTIN_NETWORK=1` for the egress/DNS/host-port/isolation guest tests (they start their own loopback fixtures — no outbound network is required by the tests themselves, but the host proxy variables `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` must be scrubbed for the run so a passing proxy dependency can't hide as a pass) and `AWMAN_TEST_BUILTIN_PRESSURE=1` for the bounded guest-OOM scenario (a small, fixed-size VM), both needing the same hardware prerequisites as `AWMAN_TEST_BUILTIN`. **As of this work item these test bodies (`tests/builtin_runtime/{network_resources,lifecycle,acquisition}.rs`) exist but are not yet registered in `tests/builtin_runtime/main.rs`** — `cargo test --test builtin_runtime -- --list` does not discover them, so `autotests=false` makes them zero coverage regardless of these opt-ins until that registration lands.
- When changing SQLite resolution or native dependencies, rerun the recorded SQLite resolution checks (`bash tools/oci-runtime-spike/sqlite-resolution/checks.sh auto`): confirm exactly one bundled `libsqlite3-sys`, combined awman/msb linking, store tests, msb migrations, cross-driver transactions, and old/new catalog round trips on supported native targets. Missing spike fixtures or hardware are reported as BLOCKED, not inferred from a successful compile.
- Inspect final feature artifacts for dynamic dependencies and extracted-helper candidates (`bash tools/release-artifact-check.sh`, `bash tools/measure-release-artifact.sh`). The runtime must work offline with a minimal executable search path after build; no production runtime path may depend on `tools/` scripts, Cargo caches, or temporary spike artifacts.
- `bash tools/reproducible-build-audit.sh [output-dir]` checks, on a native checkout with usable Git metadata, that build inputs (`.cargo/config.toml`, Cargo manifests/lock, patches, CI inputs) are Git-tracked and free of stray temp/cache paths. `make native-builtin-ci TARGET=<triple>` builds `awman` and its synthetic guest driver from a clean isolated `CARGO_HOME`/target dir and asserts the typed provider's info/version entrypoints on real hardware; it does not yet boot the actual `awman` entrypoint against a real guest (see the open request in `aspec/work-items/0121-complete-builtin-runtime-and-native-verification.md` for that remaining gap). Both fail closed (BLOCKED) rather than infer a pass when Git metadata, hardware, or verified payloads are unavailable.
- The builtin driver uses the vendored `build_lazy_isolated()` entry point on pinned `microsandbox 0.7.2`. It starts from independent defaults and awman-owned paths without merging installed SDK config. `third_party/microsandbox-0.7.2/upstream.diff` contains the one-method patch selected in Cargo.toml and Cargo.lock; WI 0120 records its removal gate. Feature compilation and hostile-config tests do not establish native coexistence. Do not replace this with process-global environment mutation.

Version control:
- Git is used for this project

Documentation:
- After every work item is implemented, documentation should be written within the docs/ folder. Do not create one document per work item, but instead author a comprehensive set of documentation that explains to the user how to use the aspec tool in its entirety. Each work item should trigger an inspection of the entire docs/ folder to update and/or add relevant usage information.

## Profiling and Benchmarking

### Criterion benchmarks

The `benches/performance.rs` file contains micro-benchmarks using `criterion`:

| Benchmark group | What it measures |
|---|---|
| `render_frame_time` | Frame draw time at 1, 5, 10, 20 tabs |
| `pty_parse_throughput` | `process_pty_data()` throughput for plain text, ANSI, and CR-overwrite streams |
| `subprocess_spawn` | Subprocess spawn latency (lower bound for Docker API call cost) |
| `dag_topological_order` | `topological_order()` latency at 10, 50, 100, 200 workflow steps |

Run all benchmarks:

```sh
cargo bench
```

Criterion writes HTML reports to `target/criterion/` (requires the `html_reports` feature, which is enabled in `Cargo.toml`).

### tokio-console (task lifetime visualisation)

> **Planned — work item 0040.** The `tokio-console` feature flag and `console-subscriber` dependency have not yet been added to `Cargo.toml`. The steps below describe the intended usage once work item 0040 is implemented.

`tokio-console-subscriber` will be gated behind a `tokio-console` Cargo feature flag so it is never compiled into release builds.

Once implemented, enable it with:

```sh
cargo run --features tokio-console
```

Then in a separate terminal:

```sh
tokio-console
```

This shows all live Tokio tasks, their poll counts, and idle/busy times — useful for diagnosing task starvation or orphaned tasks.

**Install tokio-console CLI:**

```sh
cargo install tokio-console
```

### Flamegraph profiling

Install `cargo-flamegraph`:

```sh
cargo install flamegraph
```

Produce a CPU flamegraph for a specific benchmark or binary:

```sh
# Profile a benchmark
cargo flamegraph --bench render -- --bench

# Profile the binary directly (requires sudo on Linux for perf)
sudo cargo flamegraph -- implement 0001 --non-interactive
```

The output is `flamegraph.svg` in the current directory.

### Heap profiling

On Linux, use `heaptrack` to measure heap allocations:

```sh
# Install heaptrack (Ubuntu/Debian)
sudo apt install heaptrack

# Profile awman
heaptrack ./target/release/awman implement 0001 --non-interactive
heaptrack_gui heaptrack.awman.*.zst
```

On macOS, use `dhat` (compile-time heap profiler) by enabling the `dhat-heap` dev-dependency (see `Cargo.toml`).
