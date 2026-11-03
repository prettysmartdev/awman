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

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with(branch: Option<&str>, file_count: usize) -> GitDiffSummary {
        GitDiffSummary {
            branch: branch.map(str::to_string),
            files: (0..file_count)
                .map(|i| crate::engine::git::GitFileEntry {
                    path: format!("f{i}.rs"),
                    change: crate::engine::git::GitFileChangeType::Modified,
                    added: 1,
                    removed: 0,
                    binary: false,
                })
                .collect(),
            added: file_count as u32,
            removed: 0,
        }
    }

    #[test]
    fn sidebar_title_variants() {
        assert_eq!(sidebar_title(&None), "git status");
        assert_eq!(
            sidebar_title(&Some(summary_with(Some("main"), 3))),
            "main: 3 changed"
        );
        assert_eq!(
            sidebar_title(&Some(summary_with(Some("main"), 0))),
            "main: clean"
        );
        assert_eq!(
            sidebar_title(&Some(summary_with(None, 1))),
            "HEAD: 1 changed"
        );
    }

    #[test]
    fn sidebar_width_respects_state_and_minimum() {
        assert_eq!(sidebar_width(80, GitSidebarState::Open), 20);
        assert_eq!(sidebar_width(79, GitSidebarState::Open), 0);
        assert_eq!(sidebar_width(200, GitSidebarState::Open), 50);
        assert_eq!(sidebar_width(200, GitSidebarState::Closed), 0);
    }
}
