//! Presentation helpers for the TUI git sidebar.

use crate::engine::git::GitDiffSummary;

/// Sidebar open/close state, stored on the `Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSidebarState {
    Open,
    Closed,
}

/// Minimum usable sidebar width in columns. When 1/4 of the terminal is
/// narrower than this the sidebar is treated as closed (only the status-bar
/// summary shows).
pub const MIN_SIDEBAR_WIDTH: u16 = 20;

/// The sidebar's column width for the given terminal width and state. Returns
/// `0` when the sidebar is closed or would be narrower than
/// [`MIN_SIDEBAR_WIDTH`]. Shared by the renderer (layout split) and the event
/// loop (PTY resize) so both agree on when the sidebar is effectively present.
pub fn sidebar_width(term_cols: u16, state: GitSidebarState) -> u16 {
    match state {
        GitSidebarState::Open => {
            let w = term_cols / 4;
            if w >= MIN_SIDEBAR_WIDTH {
                w
            } else {
                0
            }
        }
        GitSidebarState::Closed => 0,
    }
}

/// The sidebar's border title — a condensed `git status` line. Always
/// non-empty so the open sidebar is labeled even with no data or no changes:
/// `main: 3 changed`, `main: clean`, `HEAD: 1 changed` (detached), or
/// `git status` when there is no git data at all.
pub fn sidebar_title(summary: &Option<GitDiffSummary>) -> String {
    let Some(summary) = summary else {
        return "git status".to_string();
    };
    let branch = summary.branch.as_deref().unwrap_or("HEAD");
    match summary.files.len() {
        0 => format!("{branch}: clean"),
        n => format!("{branch}: {n} changed"),
    }
}
