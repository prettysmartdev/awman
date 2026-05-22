//! Baseline performance benchmarks for amux (work item 0033).
//!
//! Run with: `cargo bench`
//!
//! These benchmarks establish baselines for:
//! - TUI render frame time at various tab counts
//! - PTY output parse throughput
//! - Subprocess spawn overhead (proxy for Docker API call latency)
//!
//! Results are written to `target/criterion/` as HTML reports.

use awman::tui::{render, state::App, state::TabState};
use awman::workflow::{dag, parser::WorkflowStep};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ratatui::{backend::TestBackend, Terminal};
use std::path::PathBuf;

// ─── Render frame time ────────────────────────────────────────────────────────

/// Measures how long a single `draw` call takes as the number of open tabs grows.
///
/// Only the active tab's content is rendered in full; additional tabs contribute
/// to the tab bar. This benchmark detects O(n) or worse scaling in the render path.
fn bench_render_frame_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("render_frame_time");

    for n_tabs in [1usize, 5, 10, 20] {
        group.bench_with_input(
            BenchmarkId::new("tabs", n_tabs),
            &n_tabs,
            |b, &n| {
                // Setup: build App with n tabs, each containing 200 output lines.
                let mut app = App::new(PathBuf::from("/tmp/bench"));
                for _ in 1..n {
                    app.create_tab(PathBuf::from("/tmp/bench"));
                }
                for i in 0..app.tabs.len() {
                    app.tabs[i].start_command(format!("bench-cmd-{}", i));
                    for j in 0..200 {
                        app.tabs[i].push_output(format!(
                            "output line {} from tab {}",
                            j, i
                        ));
                    }
                }

                let backend = TestBackend::new(200, 50);
                let mut terminal = Terminal::new(backend).unwrap();

                b.iter(|| {
                    terminal
                        .draw(|frame| render::draw(frame, &mut app))
                        .unwrap();
                });
            },
        );
    }

    group.finish();
}

// ─── PTY parse throughput ─────────────────────────────────────────────────────

/// Measures the throughput of `process_pty_data` for plain-text and ANSI-escaped
/// byte streams. Uses `iter_batched` to start from a fresh `TabState` each round,
/// preventing unbounded `output_lines` growth from skewing later iterations.
fn bench_pty_parse_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("pty_parse_throughput");

    // ~10 KB of plain newline-terminated text.
    let plain_chunk = b"Hello, world! This is a line of output from the PTY.\n";
    let plain_10kb: Vec<u8> = plain_chunk.repeat(10 * 1024 / plain_chunk.len() + 1);

    // ~10 KB of ANSI-coloured text (simulates coloured compiler / agent output).
    let ansi_chunk =
        b"\x1b[32mSuccess\x1b[0m \x1b[1;34mbuilding\x1b[0m target/debug/amux\r\n";
    let ansi_10kb: Vec<u8> = ansi_chunk.repeat(10 * 1024 / ansi_chunk.len() + 1);

    // ~10 KB of spinner-style carriage-return-only lines (progress indicators).
    let cr_chunk = b"\rProcessing... [=====     ] 50%";
    let cr_10kb: Vec<u8> = cr_chunk.repeat(10 * 1024 / cr_chunk.len() + 1);

    group.throughput(Throughput::Bytes(plain_10kb.len() as u64));
    group.bench_function("plain_text_10kb", |b| {
        b.iter_batched(
            || {
                let mut tab = TabState::new(PathBuf::from("/tmp/bench"));
                tab.start_command("bench".into());
                tab
            },
            |mut tab| {
                tab.process_pty_data(&plain_10kb);
                tab
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.throughput(Throughput::Bytes(ansi_10kb.len() as u64));
    group.bench_function("ansi_colored_10kb", |b| {
        b.iter_batched(
            || {
                let mut tab = TabState::new(PathBuf::from("/tmp/bench"));
                tab.start_command("bench".into());
                tab
            },
            |mut tab| {
                tab.process_pty_data(&ansi_10kb);
                tab
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.throughput(Throughput::Bytes(cr_10kb.len() as u64));
    group.bench_function("carriage_return_overwrite_10kb", |b| {
        b.iter_batched(
            || {
                let mut tab = TabState::new(PathBuf::from("/tmp/bench"));
                tab.start_command("bench".into());
                tab
            },
            |mut tab| {
                tab.process_pty_data(&cr_10kb);
                tab
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ─── Docker API call latency (subprocess spawn) ───────────────────────────────

/// Proxy benchmark for Docker API call latency.
///
/// Each Docker CLI call (`docker stats`, `docker run`, `docker inspect`, …) is a
/// subprocess spawn on the host.  This benchmark measures the minimum latency floor
/// for any such operation by spawning a trivial `echo` process.  The real Docker
/// overhead will be higher, but this gives a lower bound.
///
/// Run `docker stats --no-stream <container>` manually and compare with
/// `target/criterion/subprocess_spawn/docker_echo` to estimate Docker overhead.
fn bench_subprocess_spawn(c: &mut Criterion) {
    let mut group = c.benchmark_group("subprocess_spawn");

    group.bench_function("echo_hello", |b| {
        b.iter(|| {
            std::process::Command::new("echo")
                .arg("benchmark")
                .output()
                .expect("echo must be available");
        });
    });

    // Spawn + capture stdout, matching the `docker stats --no-stream` access pattern.
    group.bench_function("echo_with_stdout_capture", |b| {
        b.iter(|| {
            let out = std::process::Command::new("echo")
                .arg("cpu=2.5%,mem=128MiB")
                .output()
                .expect("echo must be available");
            // Simulate the parsing work done after each docker stats call.
            let _s = String::from_utf8_lossy(&out.stdout);
        });
    });

    group.finish();
}

// ─── DAG topological sort latency ────────────────────────────────────────────

/// Builds a linear chain of `n` steps: step_0 ← step_1 ← … ← step_{n-1}.
/// This is the worst case for the DFS-based topological sort (longest critical path).
fn linear_chain(n: usize) -> Vec<WorkflowStep> {
    (0..n)
        .map(|i| WorkflowStep {
            name: format!("step_{}", i),
            depends_on: if i == 0 {
                vec![]
            } else {
                vec![format!("step_{}", i - 1)]
            },
            prompt_template: String::new(),
        })
        .collect()
}

/// Measures `dag::topological_order()` at realistic and stress-test workflow sizes.
///
/// In practice workflows have <20 steps; 200 steps is a stress upper bound.
/// The benchmark detects if memoization becomes necessary at larger sizes.
fn bench_dag_topological_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("dag_topological_order");

    for n_steps in [10usize, 50, 100, 200] {
        let steps = linear_chain(n_steps);
        group.bench_with_input(
            BenchmarkId::new("linear_chain", n_steps),
            &steps,
            |b, steps| {
                b.iter(|| {
                    dag::topological_order(steps);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_render_frame_time,
    bench_pty_parse_throughput,
    bench_subprocess_spawn,
    bench_dag_topological_order,
);
criterion_main!(benches);
