//! Tests for git-sidebar rendering: open/closed width allocation, the
//! green-corner indicator, and the status-bar +/- summary.

use super::*;

// ─── Git sidebar ──────────────────────────────────────────────────────────

fn render_app(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// True if the buffer contains a green rounded top-left corner ('╭'). The
/// only green rounded border in an idle app is the git sidebar (idle tabs
/// are DarkGray), so this uniquely detects a rendered sidebar.
fn has_green_sidebar_corner(buf: &ratatui::buffer::Buffer) -> Option<u16> {
    let area = *buf.area();
    for x in 0..area.width {
        for y in 0..area.height {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == "\u{256d}" && cell.fg == ratatui::style::Color::Green {
                return Some(x);
            }
        }
    }
    None
}

fn set_summary(app: &App, additions: u32, deletions: u32) {
    use crate::frontend::tui::git_sidebar::GitDiffSummary;
    *app.active_tab().git_diff_summary.lock().unwrap() = Some(GitDiffSummary {
        files: Vec::new(),
        total_additions: additions,
        total_deletions: deletions,
        branch: None,
    });
}

#[test]
fn ctrl_g_toggles_sidebar_twice_returns_to_closed() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    assert_eq!(
        app.active_tab().git_sidebar_state,
        GitSidebarState::Closed,
        "sidebar starts closed"
    );
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().git_sidebar_state, GitSidebarState::Open);
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().git_sidebar_state,
        GitSidebarState::Closed,
        "toggling twice returns to Closed"
    );
}

#[test]
fn render_frame_closed_has_no_sidebar_and_uses_full_width() {
    let mut app = make_app();
    let buf = render_app(&mut app, 80, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "closed sidebar must not render a green border"
    );
    // The vertical layout still spans the full width: the tab bar's rounded
    // top-left corner sits at column 0.
    assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "\u{256d}");
}

#[test]
fn render_frame_open_allocates_at_most_a_quarter_to_the_sidebar() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    let width = 80u16;
    let buf = render_app(&mut app, width, 24);
    let sidebar_x = has_green_sidebar_corner(&buf)
        .expect("open sidebar must render a green rounded border");
    let sidebar_width = width - sidebar_x;
    assert!(
        sidebar_width <= width / 4,
        "sidebar width {sidebar_width} must be ≤ 25% of {width}"
    );
    assert_eq!(sidebar_width, 20, "80/4 == 20 columns");
}

#[test]
fn render_frame_narrow_terminal_collapses_sidebar() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    set_summary(&app, 7, 2);
    // 60/4 == 15 < MIN_SIDEBAR_WIDTH (20) → sidebar collapses to nothing.
    let buf = render_app(&mut app, 60, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "sidebar must collapse when a quarter of the width is < 20 columns"
    );
    let text: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("+7") && text.contains("-2"),
        "collapsed sidebar must still show the status-bar summary: {text:?}"
    );
}

#[test]
fn status_bar_shows_plus_minus_when_sidebar_closed_and_summary_present() {
    let mut app = make_app();
    set_summary(&app, 12, 3);
    let buf = render_app(&mut app, 80, 24);
    let text: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("+12"),
        "status bar shows additions: had lines"
    );
    assert!(text.contains("-3"), "status bar shows deletions");
}

#[test]
fn status_bar_omits_summary_when_none() {
    // No summary set → no `+`/`-` diff readout injected into the status bar.
    let mut app = make_app();
    let buf = render_app(&mut app, 80, 24);
    // The idle status hint contains "ctrl-g git" but never a "+N -N" pair.
    let last_rows: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !last_rows.contains("+0 -0"),
        "no diff summary must be shown when the summary is None"
    );
}

