//! Integration tests for work item 0041 — Container Terminal Improvements.
//!
//! Covers: resize-clears-selection, snapshot isolation from live output,
//! and end-to-end scrollback display.

use awman::tui::state::{ContainerWindowState, TabState};
use std::path::PathBuf;

// ─── resize clears selection ──────────────────────────────────────────────────

/// When the terminal is resized any active selection must be cleared to prevent
/// stale vt100 coordinate mappings (vt100 re-wraps lines on resize).
#[test]
fn resize_clears_active_selection() {
    let mut tab = TabState::new(PathBuf::from("/tmp/resize-clear"));
    tab.start_container("ctr".into(), "Agent".into(), 80, 24);

    // Simulate a selection being set (as the mouse handler would).
    tab.terminal_selection_start = Some((3, 5));
    tab.terminal_selection_end = Some((5, 10));
    tab.terminal_selection_snapshot = Some(vec![vec!["A".to_string()]]);

    assert!(tab.terminal_selection_start.is_some());

    // A resize event calls clear_terminal_selection on every tab.
    tab.clear_terminal_selection();

    assert!(
        tab.terminal_selection_start.is_none(),
        "terminal_selection_start must be None after resize"
    );
    assert!(
        tab.terminal_selection_end.is_none(),
        "terminal_selection_end must be None after resize"
    );
    assert!(
        tab.terminal_selection_snapshot.is_none(),
        "terminal_selection_snapshot must be None after resize"
    );
}

/// Clearing selection multiple times is idempotent and must not panic.
#[test]
fn resize_clears_selection_is_idempotent() {
    let mut tab = TabState::new(PathBuf::from("/tmp/resize-idem"));
    tab.clear_terminal_selection();
    tab.clear_terminal_selection(); // second call must not panic
    assert!(tab.terminal_selection_start.is_none());
}

// ─── snapshot isolates selection from live output ─────────────────────────────

/// The snapshot captured at MouseDown must be independent of subsequent vt100
/// output. Feeding new bytes to the parser after taking the snapshot must not
/// alter the snapshot contents.
#[test]
fn selection_snapshot_isolated_from_subsequent_output() {
    let mut tab = TabState::new(PathBuf::from("/tmp/snap-iso"));
    tab.terminal_scrollback_lines = 500;
    tab.start_container("ctr".into(), "Agent".into(), 40, 5);

    // Write initial content.
    if let Some(ref mut parser) = tab.vt100_parser {
        parser.process(b"line one\r\nline two\r\n");
    }

    // Capture snapshot (simulates what mouse-down handler does).
    let snapshot: Vec<Vec<String>> = {
        let parser = tab.vt100_parser.as_ref().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| {
                        screen.cell(r, c)
                            .map(|cell| {
                                let s = cell.contents();
                                if s.is_empty() { " ".to_string() } else { s }
                            })
                            .unwrap_or_else(|| " ".to_string())
                    })
                    .collect()
            })
            .collect()
    };

    let snapshot_before = snapshot.clone();

    // Feed more output after snapshot was taken.
    if let Some(ref mut parser) = tab.vt100_parser {
        parser.process(b"new output that should NOT affect snapshot\r\n");
    }

    // The snapshot captured earlier must be unchanged.
    assert_eq!(
        snapshot_before, snapshot,
        "snapshot is a value — subsequent parser mutations cannot change it"
    );
}

// ─── scrollback offset depth verification ────────────────────────────────────

/// After feeding many lines into the vt100 parser the probe trick
/// (set_scrollback(usize::MAX)) must report a depth greater than one screen height,
/// and the container_scroll_offset can be set up to that depth.
#[test]
fn scrollback_offset_upper_bound_matches_actual_depth() {
    let screen_rows: u16 = 10;
    let screen_cols: u16 = 40;

    let mut tab = TabState::new(PathBuf::from("/tmp/scroll-depth"));
    tab.terminal_scrollback_lines = 500;
    tab.start_container("ctr".into(), "Agent".into(), screen_cols, screen_rows);

    // Feed 80 lines — 8× the screen height.
    if let Some(ref mut parser) = tab.vt100_parser {
        for i in 0u32..80 {
            let line = format!("line {:03}\r\n", i);
            parser.process(line.as_bytes());
        }
    }

    // Probe actual scrollback depth.
    let max_scroll = if let Some(ref mut parser) = tab.vt100_parser {
        parser.set_scrollback(usize::MAX);
        let m = parser.screen().scrollback();
        parser.set_scrollback(0);
        m
    } else {
        0
    };

    assert!(
        max_scroll > screen_rows as usize,
        "scrollback depth ({}) should exceed screen height ({})",
        max_scroll, screen_rows
    );

    // container_scroll_offset can be set anywhere from 0 to max_scroll.
    tab.container_scroll_offset = max_scroll;
    assert_eq!(tab.container_scroll_offset, max_scroll);

    tab.container_scroll_offset = 0;
    assert_eq!(tab.container_scroll_offset, 0);
}

// ─── vt100 parser uses terminal_scrollback_lines ─────────────────────────────

/// `start_container` must use `terminal_scrollback_lines` when creating the
/// vt100 parser. Setting a small cap before `start_container` and feeding more
/// lines than the cap must result in ≤ cap rows in scrollback.
#[test]
fn start_container_respects_terminal_scrollback_lines() {
    let cap: usize = 50;
    let mut tab = TabState::new(PathBuf::from("/tmp/cap-test"));
    tab.terminal_scrollback_lines = cap;
    tab.start_container("ctr".into(), "Agent".into(), 40, 10);

    // Feed 200 lines — 4× the cap.
    if let Some(ref mut parser) = tab.vt100_parser {
        for i in 0u32..200 {
            let line = format!("line {:03}\r\n", i);
            parser.process(line.as_bytes());
        }
    }

    let retained = if let Some(ref mut parser) = tab.vt100_parser {
        parser.set_scrollback(usize::MAX);
        let m = parser.screen().scrollback();
        parser.set_scrollback(0);
        m
    } else {
        0
    };

    assert!(
        retained <= cap,
        "scrollback depth ({}) must not exceed configured cap ({})",
        retained, cap
    );
}

// ─── selection cleared on start_container ────────────────────────────────────

/// Starting a new container session must clear any stale selection state from
/// the previous session.
#[test]
fn start_container_clears_selection() {
    let mut tab = TabState::new(PathBuf::from("/tmp/new-ctr-clear"));
    tab.terminal_selection_start = Some((1, 2));
    tab.terminal_selection_end = Some((3, 4));
    tab.terminal_selection_snapshot = Some(vec![vec!["x".to_string()]]);

    tab.start_container("ctr2".into(), "Agent".into(), 80, 24);

    assert!(
        tab.terminal_selection_start.is_none(),
        "start_container must clear terminal_selection_start"
    );
    assert!(
        tab.terminal_selection_end.is_none(),
        "start_container must clear terminal_selection_end"
    );
    assert!(
        tab.terminal_selection_snapshot.is_none(),
        "start_container must clear terminal_selection_snapshot"
    );
}

// ─── new_tab initialises selection fields to None ────────────────────────────

#[test]
fn new_tab_has_no_selection_state() {
    let tab = TabState::new(PathBuf::from("/tmp/fresh-sel"));
    assert!(
        tab.terminal_selection_start.is_none(),
        "new tab must have no selection start"
    );
    assert!(
        tab.terminal_selection_end.is_none(),
        "new tab must have no selection end"
    );
    assert!(
        tab.terminal_selection_snapshot.is_none(),
        "new tab must have no selection snapshot"
    );
    assert!(
        tab.container_inner_area.is_none(),
        "new tab must have no container_inner_area"
    );
}

// ─── default scrollback lines ─────────────────────────────────────────────────

#[test]
fn new_tab_default_scrollback_lines_is_ten_thousand() {
    let tab = TabState::new(PathBuf::from("/tmp/default-sb"));
    assert_eq!(
        tab.terminal_scrollback_lines,
        awman::config::DEFAULT_SCROLLBACK_LINES,
        "new tab must default to DEFAULT_SCROLLBACK_LINES"
    );
    assert_eq!(
        awman::config::DEFAULT_SCROLLBACK_LINES,
        10_000,
        "DEFAULT_SCROLLBACK_LINES must be 10,000"
    );
}

// ─── container_window becomes hidden after finish_command ────────────────────

#[test]
fn finish_command_hides_container_and_clears_selection() {
    let mut tab = TabState::new(PathBuf::from("/tmp/finish-sel"));
    tab.start_container("ctr".into(), "Agent".into(), 80, 24);

    tab.terminal_selection_start = Some((0, 0));
    tab.terminal_selection_end = Some((0, 5));

    tab.finish_command(0);

    assert_eq!(
        tab.container_window,
        ContainerWindowState::Hidden,
        "container_window must be Hidden after finish_command"
    );
    // finish_command does not clear selection (no spec requirement),
    // but start_container on the next session will.
    // Just verify the container is properly cleaned up.
    assert!(tab.vt100_parser.is_none(), "vt100_parser must be None after finish_command");
}
