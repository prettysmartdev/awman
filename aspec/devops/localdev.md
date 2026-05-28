# Local Development

Development: local
Build tools: make, cargo, docker

## Workflows:

Developer Loop:
- running `make all` should build the aspec CLI binary using the local Rust/Cargo toolchain
- running `make install` should run `make all` and then install the aspec CLI to /usr/local/bin/


Local testing:
- running `make test` should run all tests in the project

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