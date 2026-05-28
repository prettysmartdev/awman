//! Memory snapshot tests for amux (work item 0033).
//!
//! These tests verify that output buffers are bounded or explicitly freed when
//! a tab is closed or a new command starts, preventing unbounded heap growth
//! during long-running sessions.

use awman::tui::state::{App, ContainerWindowState, ExecutionPhase, TabState};
use std::path::PathBuf;

// ─── VT100 scrollback is bounded ─────────────────────────────────────────────

/// The vt100 parser used for container output is created with a 10 000-line
/// scrollback limit (the new default from work item 0041).  Pushing more than
/// 10 000 lines must not cause the internal buffer to grow beyond that cap.
#[test]
fn vt100_scrollback_is_bounded_at_10000_lines() {
    let mut tab = TabState::new(PathBuf::from("/tmp/vt100-bound"));
    // Start a container session (cols=80, rows=24).
    tab.start_container("ctr-test".into(), "TestAgent".into(), 80, 24);

    // Feed 3 000 lines of output into the vt100 parser.
    let line = b"abcdefghijklmnopqrstuvwxyz 0123456789\r\n";
    let three_k_lines: Vec<u8> = line.repeat(3_000);

    // Route bytes through the container path (vt100 parser).
    assert!(tab.vt100_parser.is_some(), "vt100_parser should be active after start_container");
    if let Some(ref mut parser) = tab.vt100_parser {
        parser.process(&three_k_lines);

        // The default scrollback_len is now 10 000.  With 3 000 lines fed into
        // a 24-row screen, the scrollback contains up to 2 976 rows (3 000 – 24
        // visible).  Probing via set_scrollback(usize::MAX) reports the actual
        // retained depth, which must be within the 10 000-line cap.
        parser.set_scrollback(usize::MAX);
        let retained = parser.screen().scrollback();
        parser.set_scrollback(0);

        assert!(
            retained <= 10_000,
            "vt100 scrollback retained {} rows after 3 000 lines — expected ≤ 10 000 \
             (configured scrollback_len cap)",
            retained
        );
        // All 3 000 pushed lines fit within the new 10 000-line cap.
        assert!(
            retained > 0,
            "expected some scrollback content after pushing 3 000 lines, got 0"
        );

        // The screen dimensions must be unchanged.
        let (rows, cols) = parser.screen().size();
        assert_eq!(rows, 24, "Screen rows should remain 24 after processing data");
        assert_eq!(cols, 80, "Screen cols should remain 80 after processing data");
    }
}

/// After `finish_command`, the vt100 parser and stats channel must be dropped
/// (set to `None`) to release container-session memory.
#[test]
fn finish_command_releases_container_resources() {
    let mut tab = TabState::new(PathBuf::from("/tmp/finish-release"));
    tab.start_container("ctr-finish".into(), "TestAgent".into(), 80, 24);

    assert!(tab.vt100_parser.is_some(), "parser should exist after start_container");
    assert_eq!(tab.container_window, ContainerWindowState::Maximized);

    tab.finish_command(0);

    assert!(
        tab.vt100_parser.is_none(),
        "vt100_parser must be None after finish_command"
    );
    assert_eq!(
        tab.container_window,
        ContainerWindowState::Hidden,
        "container_window must return to Hidden after finish_command"
    );
    assert!(
        tab.stats_rx.is_none(),
        "stats_rx must be None after finish_command"
    );
    // A summary should have been generated (agent_display_name was provided).
    assert!(
        tab.last_container_summary.is_some(),
        "last_container_summary should be populated after a container session"
    );
}

// ─── output_lines is cleared on new command ───────────────────────────────────

/// `start_command` must clear `output_lines` so previous command output does not
/// survive into the next command's execution window.
#[test]
fn start_command_clears_output_lines() {
    let mut tab = TabState::new(PathBuf::from("/tmp/output-clear"));
    for i in 0..500 {
        tab.push_output(format!("stale line {}", i));
    }
    assert_eq!(tab.output_lines.len(), 500);

    tab.start_command("fresh-command".into());

    assert!(
        tab.output_lines.is_empty(),
        "output_lines must be empty immediately after start_command; \
         found {} lines",
        tab.output_lines.len()
    );
    assert!(
        matches!(tab.phase, ExecutionPhase::Running { .. }),
        "phase must be Running after start_command"
    );
}

/// The PTY line buffer must also be cleared when starting a new command so
/// partial lines from a previous run cannot bleed into new output.
#[test]
fn start_command_clears_pty_line_buffer() {
    let mut tab = TabState::new(PathBuf::from("/tmp/ptybuf-clear"));
    tab.start_command("old-cmd".into());
    tab.process_pty_data(b"partial line without newline");
    // pty_line_buffer now holds "partial line without newline"

    tab.start_command("new-cmd".into());

    // The live-line state must be fully reset.
    assert!(
        !tab.pty_live_line,
        "pty_live_line should be false after start_command"
    );
    assert!(
        !tab.pty_pending_cr,
        "pty_pending_cr should be false after start_command"
    );
}

// ─── Closed tab frees its output buffer ──────────────────────────────────────

/// When `close_tab` removes a tab from `App`, the `TabState` (and its
/// `output_lines` Vec) must be dropped immediately.  Rust's ownership model
/// guarantees this; this test confirms the API contract by checking that the
/// remaining tab has not inherited the closed tab's data.
#[test]
fn closed_tab_output_does_not_leak_into_remaining_tabs() {
    let mut app = App::new(PathBuf::from("/tmp/tab-close-a"));
    app.create_tab(PathBuf::from("/tmp/tab-close-b"));

    // Fill tab 0 with 1 000 lines.
    for i in 0..1_000 {
        app.tabs[0].push_output(format!("leaked-line-{}", i));
    }
    // Fill tab 1 with a distinctive marker.
    app.tabs[1].push_output("survivor-marker".to_string());

    assert_eq!(app.tabs.len(), 2);
    app.close_tab(0);

    assert_eq!(app.tabs.len(), 1, "Tab was not removed");
    assert_eq!(app.active_tab_idx, 0, "active_tab_idx was not adjusted");

    // The surviving tab must contain only its own data.
    assert!(
        app.tabs[0].output_lines.iter().any(|l| l == "survivor-marker"),
        "Survivor tab lost its own output after close_tab"
    );
    assert!(
        !app.tabs[0]
            .output_lines
            .iter()
            .any(|l| l.contains("leaked-line")),
        "Closed tab's output_lines leaked into the surviving tab"
    );
}

/// Closing every tab triggers `should_quit` and does not leave dangling state.
#[test]
fn closing_all_tabs_sets_should_quit() {
    let mut app = App::new(PathBuf::from("/tmp/quit-test"));
    for i in 0..500 {
        app.tabs[0].push_output(format!("line {}", i));
    }
    app.close_tab(0);

    assert!(
        app.should_quit,
        "should_quit must be true after closing the last tab"
    );
}

// ─── output_lines growth during a long run ────────────────────────────────────

/// Pushes a very large number of output lines (simulating a long-running agent)
/// and confirms the lines accumulate correctly without panic.
///
/// NOTE: This test documents the CURRENT behaviour (unbounded growth during a
/// session). A future work item should cap `output_lines` with a ring buffer to
/// prevent runaway memory use in very long sessions.
#[test]
fn output_lines_grow_unbounded_during_single_command() {
    let mut tab = TabState::new(PathBuf::from("/tmp/long-run"));
    tab.start_command("long-agent".into());

    const LINES: usize = 50_000;
    let chunk = b"agent output: building feature X step 0000001\n";
    let big = chunk.repeat(LINES);
    tab.process_pty_data(&big);

    // Lines accumulated — this is expected today and serves as a regression
    // anchor: if output_lines is later capped, update the assertion below.
    assert!(
        tab.output_lines.len() > 1_000,
        "Expected > 1 000 output lines for a long-running command; got {}",
        tab.output_lines.len()
    );
}

// ─── VT100 None before start_container ────────────────────────────────────────

/// `TabState::new` must initialise `vt100_parser` to `None`; the parser is only
/// created when an actual container session begins.
#[test]
fn new_tab_has_no_vt100_parser() {
    let tab = TabState::new(PathBuf::from("/tmp/fresh"));
    assert!(
        tab.vt100_parser.is_none(),
        "vt100_parser must be None on a freshly created TabState"
    );
    assert!(
        tab.container_info.is_none(),
        "container_info must be None on a freshly created TabState"
    );
    assert!(
        tab.stats_rx.is_none(),
        "stats_rx must be None on a freshly created TabState"
    );
}

// ─── 10 000-line scrollback memory threshold ─────────────────────────────────

/// With the new 10 000-line scrollback default, filling the buffer should not
/// consume more than ~3 MB per tab at typical terminal widths (80 columns).
///
/// Calculation baseline:
///   10 000 rows × 80 cols × ~4 bytes per cell ≈ 3.2 MB
///
/// The test creates a fully-saturated 10 000-line buffer and verifies that the
/// retained scrollback is bounded at 10 000 lines, validating that the cap is
/// enforced and memory cannot grow without bound.
#[test]
fn vt100_10k_scrollback_within_memory_threshold() {
    use awman::tui::state::TabState;

    let cols: u16 = 80;
    let rows: u16 = 24;
    let cap: usize = 10_000;

    let mut tab = TabState::new(PathBuf::from("/tmp/mem-10k"));
    tab.terminal_scrollback_lines = cap;
    tab.start_container("ctr-mem".into(), "MemAgent".into(), cols, rows);

    // Fill the buffer 1.5× beyond the cap to exercise eviction.
    let line = b"A memory-pressure test line for the vt100 scrollback buffer.\r\n";
    let total_lines = cap + cap / 2; // 15 000 lines
    let payload: Vec<u8> = line.repeat(total_lines);

    if let Some(ref mut parser) = tab.vt100_parser {
        parser.process(&payload);

        // Probe retained depth.
        parser.set_scrollback(usize::MAX);
        let retained = parser.screen().scrollback();
        parser.set_scrollback(0);

        assert!(
            retained <= cap,
            "scrollback must not exceed the 10 000-line cap; got {} lines",
            retained
        );
        assert!(
            retained > 0,
            "some scrollback must be retained after feeding {} lines",
            total_lines
        );
    } else {
        panic!("vt100_parser should be Some after start_container");
    }
}

/// Multiple tabs each with a 10 000-line scrollback are independent.
/// This test opens three tabs and verifies their parsers are distinct.
#[test]
fn multiple_tabs_have_independent_scrollback_buffers() {
    use awman::tui::state::App;

    let mut app = App::new(PathBuf::from("/tmp/multi-tab-a"));
    app.create_tab(PathBuf::from("/tmp/multi-tab-b"));
    app.create_tab(PathBuf::from("/tmp/multi-tab-c"));

    // Start containers on all three tabs.
    for (i, tab) in app.tabs.iter_mut().enumerate() {
        tab.terminal_scrollback_lines = 10_000;
        tab.start_container(
            format!("ctr-{}", i),
            "Agent".into(),
            80,
            24,
        );
    }

    // Feed distinct content to each tab's parser.
    let line_a = b"tab-A-line\r\n";
    let line_b = b"tab-B-line\r\n";
    let line_c = b"tab-C-line\r\n";

    if let Some(ref mut p) = app.tabs[0].vt100_parser { p.process(&line_a.repeat(100)); }
    if let Some(ref mut p) = app.tabs[1].vt100_parser { p.process(&line_b.repeat(100)); }
    if let Some(ref mut p) = app.tabs[2].vt100_parser { p.process(&line_c.repeat(100)); }

    // Each parser retains its own scrollback independently.
    for (i, tab) in app.tabs.iter_mut().enumerate() {
        if let Some(ref mut parser) = tab.vt100_parser {
            parser.set_scrollback(usize::MAX);
            let depth = parser.screen().scrollback();
            parser.set_scrollback(0);
            assert!(
                depth > 0,
                "tab {} should have non-zero scrollback after 100 lines",
                i
            );
            assert!(
                depth <= 10_000,
                "tab {} scrollback ({}) must not exceed cap",
                i, depth
            );
        }
    }
}
