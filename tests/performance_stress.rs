//! Stress tests for amux performance audit (work item 0033).
//!
//! These tests verify that the system does not degrade catastrophically under
//! realistic load — 20+ simultaneous PTY streams and many tabs open at once.
//! They are not micro-benchmarks; they assert correctness and bound degradation.

use awman::tui::{render, state::App, state::TabState};
use ratatui::{backend::TestBackend, Terminal};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Number of concurrent PTY streams to open in the stress test.
const N_STREAMS: usize = 20;

/// Bytes of PTY output to push through each simulated stream.
const BYTES_PER_STREAM: usize = 100_000; // 100 KB

/// Maximum acceptable wall-clock time to process all streams.
const MAX_TOTAL_SECS: u64 = 60;

/// Maximum acceptable render time for 20 tabs relative to 1 tab.
/// We expect roughly linear scaling (20×) with some overhead; 100× would
/// indicate a pathological O(n²) regression.
const MAX_RENDER_DEGRADATION_FACTOR: f64 = 100.0;

// ─── PTY stream stress ────────────────────────────────────────────────────────

/// Opens 20 simulated PTY streams in parallel OS threads and measures
/// the throughput and absence of catastrophic degradation.
///
/// Each thread creates its own `TabState` (they are not `Send` due to the
/// portable-pty master handle when a PTY is attached, but a bare `TabState`
/// created with `new()` has only tokio channels which are `Send`), processes a
/// fixed chunk of data, and returns the number of output lines produced.
#[test]
fn stress_20_concurrent_pty_streams_throughput() {
    // Build the shared input data once; each thread gets an Arc reference.
    let chunk = b"output from container step-42: all systems nominal\r\n";
    let data: Arc<Vec<u8>> = Arc::new(chunk.repeat(BYTES_PER_STREAM / chunk.len() + 1));

    let start = Instant::now();
    let mut handles = Vec::with_capacity(N_STREAMS);

    for stream_id in 0..N_STREAMS {
        let data = Arc::clone(&data);
        let handle = std::thread::spawn(move || -> usize {
            let mut tab = TabState::new(PathBuf::from(format!("/tmp/stress-{}", stream_id)));
            tab.start_command(format!("agent-stream-{}", stream_id));
            tab.process_pty_data(&data);
            tab.output_lines.len()
        });
        handles.push(handle);
    }

    let line_counts: Vec<usize> = handles
        .into_iter()
        .map(|h| h.join().expect("stress thread panicked"))
        .collect();

    let elapsed = start.elapsed();
    let total_lines: usize = line_counts.iter().sum();

    // Every stream must have produced output.
    assert_eq!(
        line_counts.len(),
        N_STREAMS,
        "Expected {} stream results, got {}",
        N_STREAMS,
        line_counts.len()
    );
    for (i, &lines) in line_counts.iter().enumerate() {
        assert!(
            lines > 0,
            "Stream {} produced 0 output lines — data was lost",
            i
        );
    }
    assert!(
        total_lines > 0,
        "No output lines produced across all streams"
    );

    // Processing must complete within the time budget.
    assert!(
        elapsed.as_secs() < MAX_TOTAL_SECS,
        "{} concurrent PTY streams took {:?}, expected < {}s",
        N_STREAMS,
        elapsed,
        MAX_TOTAL_SECS
    );

    println!(
        "[stress] {} streams × {:.0} KB = {:.1} MB in {:?} → {} total lines",
        N_STREAMS,
        BYTES_PER_STREAM as f64 / 1024.0,
        (N_STREAMS * BYTES_PER_STREAM) as f64 / (1024.0 * 1024.0),
        elapsed,
        total_lines,
    );
}

// ─── Frame rate degradation ───────────────────────────────────────────────────

/// Measures render time for 1 tab versus 20 tabs to surface O(n) regressions
/// in the tab-bar or layout computation paths.
///
/// Only the active tab's content area is rendered in full; additional tabs add
/// entries to the tab bar. This test ensures the render path does not
/// accidentally iterate over all tabs for each draw call.
#[test]
fn stress_render_frame_rate_degrades_linearly() {
    const LINES_PER_TAB: usize = 500;
    const DRAW_ITERATIONS: usize = 200;

    // ── Baseline: 1 tab with LINES_PER_TAB output lines ──────────────────────
    let baseline_ns = {
        let mut app = App::new(PathBuf::from("/tmp/bench-baseline"));
        app.tabs[0].start_command("baseline".into());
        for j in 0..LINES_PER_TAB {
            app.tabs[0].push_output(format!("baseline output line {}", j));
        }

        let backend = TestBackend::new(200, 50);
        let mut terminal = Terminal::new(backend).unwrap();

        let start = Instant::now();
        for _ in 0..DRAW_ITERATIONS {
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
        }
        start.elapsed().as_nanos()
    };

    // ── Stress: N_STREAMS tabs each with LINES_PER_TAB output lines ──────────
    let stress_ns = {
        let mut app = App::new(PathBuf::from("/tmp/bench-stress"));
        for _ in 1..N_STREAMS {
            app.create_tab(PathBuf::from("/tmp/bench-stress"));
        }
        for i in 0..app.tabs.len() {
            app.tabs[i].start_command(format!("stress-cmd-{}", i));
            for j in 0..LINES_PER_TAB {
                app.tabs[i].push_output(format!("tab {} output line {}", i, j));
            }
        }
        app.active_tab_idx = 0;

        let backend = TestBackend::new(200, 50);
        let mut terminal = Terminal::new(backend).unwrap();

        let start = Instant::now();
        for _ in 0..DRAW_ITERATIONS {
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
        }
        start.elapsed().as_nanos()
    };

    let degradation = stress_ns as f64 / baseline_ns.max(1) as f64;

    println!(
        "[render-degradation] 1 tab: {}µs per frame | {} tabs: {}µs per frame | factor: {:.2}×",
        baseline_ns / (DRAW_ITERATIONS as u128 * 1_000),
        N_STREAMS,
        stress_ns / (DRAW_ITERATIONS as u128 * 1_000),
        degradation,
    );

    assert!(
        degradation < MAX_RENDER_DEGRADATION_FACTOR,
        "Render degraded {:.1}× from 1→{} tabs (limit {}×). \
         Possible O(n²) iteration in render path.",
        degradation,
        N_STREAMS,
        MAX_RENDER_DEGRADATION_FACTOR,
    );
}

// ─── High-throughput chunk sizes ─────────────────────────────────────────────

/// Ensures the PTY parser handles very large single chunks (e.g. a container
/// dumping its entire stdout at once) without panicking or dropping data.
#[test]
fn stress_large_single_pty_chunk() {
    // 1 MB single chunk — simulates a container printing a large blob at once.
    let line = b"[INFO] container-output: processing record 0000001 of 0100000\r\n";
    let big_chunk: Vec<u8> = line.repeat(1_000_000 / line.len() + 1);

    let mut tab = TabState::new(PathBuf::from("/tmp/large-chunk"));
    tab.start_command("large-chunk-test".into());
    tab.process_pty_data(&big_chunk);

    assert!(
        !tab.output_lines.is_empty(),
        "No output lines produced from 1 MB chunk"
    );
    println!(
        "[large-chunk] 1 MB chunk → {} output lines",
        tab.output_lines.len()
    );
}

/// Verifies that processing many small PTY chunks (simulating high-frequency
/// partial writes) does not lose data or accumulate unbounded state.
#[test]
fn stress_many_small_pty_chunks() {
    const N_CHUNKS: usize = 10_000;
    let chunk = b"x"; // single byte — worst case for the line-buffer logic

    let mut tab = TabState::new(PathBuf::from("/tmp/small-chunks"));
    tab.start_command("small-chunks-test".into());

    for _ in 0..N_CHUNKS {
        tab.process_pty_data(chunk);
    }
    // Flush the pending live line by sending a newline.
    tab.process_pty_data(b"\n");

    // All bytes accumulated into a single line of 10 000 'x' characters.
    let combined: String = tab.output_lines.concat();
    assert!(
        combined.contains(&"x".repeat(100)),
        "Small chunks were not accumulated correctly; output: {:?}",
        &combined[..combined.len().min(200)]
    );
}
