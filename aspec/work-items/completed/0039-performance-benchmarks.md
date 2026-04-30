# Work Item: Task

Title: Add Performance Benchmarks (criterion)
Issue: issuelink

## Summary:

Add a `benches/` directory with `criterion`-based benchmarks for the three performance-sensitive paths identified in the 0033 audit: TUI frame draw time, PTY byte processing throughput, and DAG topological sort latency. These benchmarks establish a pre-optimisation baseline and will detect regressions after follow-on work items (0034–0038) are implemented.

## User Stories

### User Story 1:
As a: developer

I want to:
run `cargo bench` to get frame time, PTY throughput, and DAG latency numbers before and after an optimisation change

So I can:
confirm that the change actually improved performance and did not regress other areas

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Phase 7 of the audit plan).

### Dependencies

Add to `[dev-dependencies]` in `Cargo.toml`:

```toml
criterion = { version = "0.5", features = ["html_reports"] }
```

Add to `Cargo.toml`:

```toml
[[bench]]
name = "render"
harness = false

[[bench]]
name = "pty_parse"
harness = false

[[bench]]
name = "dag"
harness = false
```

### Benchmark 1: `benches/render.rs`

Measures frame draw time at varying tab counts and output line counts.

```rust
// Pseudocode sketch
fn bench_draw_n_tabs(c: &mut Criterion) {
    // Create App with N tabs, each with M output lines
    // Benchmark: terminal.draw(|f| render::draw(f, &mut app))
    // Input variations: N ∈ {1, 5, 10, 20}, M ∈ {100, 1_000, 10_000}
}
```

Use a `TestBackend` (Ratatui provides one) to avoid needing a real terminal.

### Benchmark 2: `benches/pty_parse.rs`

Measures `process_pty_data()` throughput.

```rust
// Pseudocode sketch
fn bench_pty_parse_throughput(c: &mut Criterion) {
    // Construct 1MB and 10MB of realistic PTY output bytes
    // (mix of plain text, ANSI escape sequences, \r\n, spinner lines)
    // Benchmark: tab.process_pty_data(&bytes)
    // Report: throughput in MB/s
}
```

### Benchmark 3: `benches/dag.rs`

Measures DAG operations at varying workflow sizes.

```rust
// Pseudocode sketch
fn bench_topological_order(c: &mut Criterion) {
    // Build workflows of 10, 50, 100, 200 steps
    // Benchmark: topological_order(&steps), ready_steps(&steps, &completed), detect_cycle(&steps)
}
```

### Baseline Numbers to Record

After implementing, run `cargo bench` and record baseline numbers in a comment in each bench file. These serve as the "before" numbers for subsequent optimisation work items.

## Edge Case Considerations:
- Benchmarks must not depend on Docker or external processes — pure in-process tests only.
- `TestBackend` size should reflect a realistic terminal (e.g. 220×50).
- PTY parse benchmark should include both "no ANSI" and "heavy ANSI" variants.

## Test Considerations:
- Benchmarks compile and run cleanly (`cargo bench --no-run` in CI to verify compilation).
- No correctness assertions in benchmarks — those belong in unit tests.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `criterion` is a `[dev-dependencies]` entry only and does not affect the release binary.
- Benchmark output (HTML reports under `target/criterion/`) is already `.gitignore`d by Cargo conventions.
