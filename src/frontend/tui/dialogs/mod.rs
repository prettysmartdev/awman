//! Pure-presentation dialog widgets for the TUI.
//!
//! Each dialog captures keyboard input while open, renders centered in the
//! terminal, and returns a typed Layer 2 enum value when the user responds.
//! Cancellable with Esc.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::frontend::tui::text_edit::TextEdit;

/// A dialog request sent from the command thread to the event loop.
#[derive(Debug)]
pub enum DialogRequest {
    YesNo {
        title: String,
        body: String,
    },
    YesNoCancel {
        title: String,
        body: String,
    },
    TextInput {
        title: String,
        prompt: String,
        default_text: Option<String>,
    },
    MultilineInput {
        title: String,
        prompt: String,
    },
    ListPicker {
        title: String,
        items: Vec<String>,
    },
    KindSelect {
        title: String,
        options: Vec<(String, String)>,
    },
    WorkflowControlBoard(WorkflowControlBoardState),
    WorkflowStepError(WorkflowStepErrorState),
    WorkflowYoloCountdown(WorkflowYoloCountdownState),
    WorkflowStepConfirm(WorkflowStepConfirmState),
    AgentSetup(AgentSetupState),
    MountScope(MountScopeState),
    AgentAuth(AgentAuthState),
    QuitConfirm,
    CloseTabConfirm,
    /// Confirmation prompt opened when the user presses Ctrl+C while a
    /// workflow is running. `y` aborts the workflow (kills the container,
    /// returns the current step to Pending), `n`/`Esc` keeps it running.
    WorkflowCancelConfirm,
    ConfigShow {
        rows: Vec<ConfigShowRow>,
    },
    Loading {
        title: String,
    },
    Custom {
        title: String,
        body: String,
        keys: Vec<(char, String)>,
    },
}

/// A dialog response returned from the event loop to the command thread.
#[derive(Debug, Clone)]
pub enum DialogResponse {
    Yes,
    No,
    Cancel,
    Text(String),
    Index(usize),
    Char(char),
    Dismissed,
}

/// The active dialog state stored in `App`.
pub enum Dialog {
    YesNo {
        title: String,
        body: String,
    },
    YesNoCancel {
        title: String,
        body: String,
    },
    TextInput {
        title: String,
        prompt: String,
        editor: TextEdit,
    },
    MultilineInput {
        title: String,
        prompt: String,
        editor: TextEdit,
    },
    ListPicker {
        title: String,
        items: Vec<String>,
        selected: usize,
    },
    KindSelect {
        title: String,
        options: Vec<(String, String)>,
    },
    WorkflowControlBoard(WorkflowControlBoardState),
    WorkflowStepError(WorkflowStepErrorState),
    WorkflowYoloCountdown(WorkflowYoloCountdownState),
    WorkflowStepConfirm(WorkflowStepConfirmState),
    AgentSetup(AgentSetupState),
    MountScope(MountScopeState),
    AgentAuth(AgentAuthState),
    QuitConfirm,
    CloseTabConfirm,
    WorkflowCancelConfirm,
    ConfigShow(ConfigShowState),
    Loading {
        title: String,
    },
    Custom {
        title: String,
        body: String,
        keys: Vec<(char, String)>,
    },
}

#[derive(Debug, Clone)]
pub struct WorkflowControlBoardState {
    pub step_name: String,
    pub can_launch_next: bool,
    pub can_continue_current: bool,
    pub can_restart: bool,
    pub can_go_back: bool,
    pub can_finish: bool,
    /// Human-readable reason explaining why "continue in current container"
    /// is not available (e.g. "next step uses a different agent"). Rendered
    /// in DarkGray underneath the disabled `[↓]` line so users understand
    /// why it's greyed out.
    pub continue_unavailable_reason: Option<String>,
    pub cancel_to_previous_unavailable_reason: Option<String>,
    pub finish_workflow_unavailable_reason: Option<String>,
    /// True when a container is currently running (mid-step). The engine
    /// computes this from `can_dismiss` in `AvailableActions`.
    /// Changes rendering: Esc = dismiss (step keeps running), [p] = pause.
    pub can_dismiss: bool,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepErrorState {
    pub step_name: String,
    pub error_lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowYoloCountdownState {
    pub step_name: String,
    pub remaining_secs: u64,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepConfirmState {
    pub completed_step: String,
    pub next_step: String,
}

#[derive(Debug, Clone)]
pub struct AgentSetupState {
    pub agent_name: String,
    pub image_only: bool,
    pub has_fallback: bool,
    pub fallback_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MountScopeState {
    pub git_root: String,
    pub cwd: String,
}

#[derive(Debug, Clone)]
pub struct AgentAuthState {
    pub agent_name: String,
    pub env_vars: Vec<String>,
}

pub struct ConfigShowState {
    pub rows: Vec<ConfigShowRow>,
    pub selected: usize,
    pub editing: bool,
    pub edit_column: usize,
    pub editor: TextEdit,
}

#[derive(Debug)]
pub struct ConfigShowRow {
    pub field: String,
    pub global: String,
    pub repo: String,
    pub effective: String,
    pub read_only: bool,
}

/// Compute a centered rect for a dialog.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

/// Compute a fixed-size centered rect.
pub fn centered_fixed(cols: u16, rows: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(cols) / 2;
    let y = area.y + area.height.saturating_sub(rows) / 2;
    Rect::new(x, y, cols.min(area.width), rows.min(area.height))
}

/// Render a dialog frame with the given title and border color.
/// Returns the padded inner area (1-cell horizontal padding, 1-row vertical
/// padding inside the border) so dialog content doesn't touch the frame.
pub fn render_dialog_frame(title: &str, color: Color, area: Rect, frame: &mut Frame) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .border_type(ratatui::widgets::BorderType::Rounded);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Add padding: 1 col each side, 1 row top/bottom
    Rect {
        x: inner.x.saturating_add(1),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(2),
    }
}

/// Render the YesNo dialog.
///
/// Sizes dynamically: width grows to fit the longest body line (clamped to a
/// usable range) and height grows to fit the body, a blank-line separator,
/// and the hint row. Body wraps so content is never silently clipped.
pub fn render_yes_no(title: &str, body: &str, area: Rect, frame: &mut Frame) {
    let max_w = area.width.saturating_sub(6).max(40);
    let max_body_w = body
        .lines()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(0) as u16;
    // +6 = 2 borders + 2 padding + 2 leading-space margin used in the hint.
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
    let width = max_body_w.saturating_add(6).max(50).max(title_w).min(max_w);
    // Body lines (after wrapping at content width), blank separator, hint.
    let inner_w = width.saturating_sub(4) as usize; // subtract borders+padding
    let wrapped_lines: usize = body
        .lines()
        .map(|line| {
            let w = unicode_width::UnicodeWidthStr::width(line);
            if inner_w == 0 || w == 0 {
                1
            } else {
                w.div_ceil(inner_w)
            }
        })
        .sum();
    let body_h = wrapped_lines as u16;
    let height = (body_h + 5).min(area.height.saturating_sub(2)).max(7);
    let dialog_area = centered_fixed(width, height, area);
    let inner = render_dialog_frame(title, Color::Yellow, dialog_area, frame);
    let text = format!("{body}\n\n  [y] Yes   [n] No   [Esc] Cancel");
    frame.render_widget(
        Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render the quit confirmation dialog (single tab).
pub fn render_quit_confirm(area: Rect, frame: &mut Frame) {
    let width = 56u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = centered_fixed(width, 8, area);
    let inner = render_dialog_frame("Quit amux?", Color::Yellow, dialog_area, frame);
    let text = "  Press Ctrl-C again to quit amux\n\n  [Esc] cancel";
    frame.render_widget(
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::default()),
        inner,
    );
}

/// Render the close-tab confirmation dialog (multiple tabs).
pub fn render_close_tab_confirm(area: Rect, frame: &mut Frame) {
    let width = 60u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = centered_fixed(width, 9, area);
    let inner = render_dialog_frame("Close tab?", Color::Yellow, dialog_area, frame);
    let text =
        "  Press Ctrl-C again to quit amux\n  Press Ctrl-T to close this tab\n\n  [Esc] cancel";
    frame.render_widget(
        Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render the workflow-cancel confirmation dialog (Ctrl+C while a workflow
/// is running).
pub fn render_workflow_cancel_confirm(area: Rect, frame: &mut Frame) {
    let width = 64u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = centered_fixed(width, 11, area);
    let inner = render_dialog_frame(
        "Cancel Workflow Execution",
        Color::Yellow,
        dialog_area,
        frame,
    );
    let text =
        "  Cancel workflow execution?\n\n  The running container will be killed and the\n  current step returned to Pending for resumption.\n\n  [y] cancel execution   [n / Esc] keep running";
    frame.render_widget(
        Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    // ─── Helper ───────────────────────────────────────────────────────────────

    fn render_to_string(
        width: u16,
        height: u16,
        f: impl FnOnce(ratatui::layout::Rect, &mut ratatui::Frame),
    ) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| f(frame.area(), frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or(" ".to_string())
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ─── Geometry: centered_fixed ─────────────────────────────────────────────

    #[test]
    fn centered_fixed_center_within_large_area() {
        let area = Rect::new(0, 0, 100, 50);
        let result = centered_fixed(40, 10, area);
        assert_eq!(result.x, (100 - 40) / 2);
        assert_eq!(result.y, (50 - 10) / 2);
        assert_eq!(result.width, 40);
        assert_eq!(result.height, 10);
    }

    #[test]
    fn centered_fixed_clips_width_when_smaller_than_area() {
        let area = Rect::new(0, 0, 20, 50);
        let result = centered_fixed(40, 10, area);
        assert_eq!(result.width, 20);
    }

    #[test]
    fn centered_fixed_clips_height_when_smaller_than_area() {
        let area = Rect::new(0, 0, 100, 5);
        let result = centered_fixed(40, 10, area);
        assert_eq!(result.height, 5);
    }

    #[test]
    fn centered_fixed_zero_dialog_is_at_center() {
        let area = Rect::new(0, 0, 100, 50);
        let result = centered_fixed(0, 0, area);
        assert_eq!(result.width, 0);
        assert_eq!(result.height, 0);
    }

    // ─── Geometry: centered_rect ──────────────────────────────────────────────

    #[test]
    fn centered_rect_centers_percentage_area() {
        let area = Rect::new(0, 0, 100, 100);
        let result = centered_rect(50, 50, area);
        // With 50% of 100 = 50 cols/rows centered: margins are 25 each side
        assert!(result.x >= 24 && result.x <= 26, "x={}", result.x);
        assert!(result.y >= 24 && result.y <= 26, "y={}", result.y);
    }

    // ─── Rendering tests ──────────────────────────────────────────────────────

    #[test]
    fn render_quit_confirm_contains_quit_text() {
        let output = render_to_string(80, 24, |area, frame| {
            render_quit_confirm(area, frame);
        });
        let lower = output.to_lowercase();
        assert!(
            lower.contains("quit"),
            "expected 'quit' in output:\n{output}"
        );
    }

    #[test]
    fn render_yes_no_shows_y_and_n_keys() {
        let output = render_to_string(80, 24, |area, frame| {
            render_yes_no("Test?", "Test body", area, frame);
        });
        assert!(
            output.contains("[y]"),
            "expected '[y]' in output:\n{output}"
        );
        assert!(
            output.contains("[n]"),
            "expected '[n]' in output:\n{output}"
        );
    }

    #[test]
    fn render_close_tab_confirm_shows_options() {
        let output = render_to_string(80, 24, |area, frame| {
            render_close_tab_confirm(area, frame);
        });
        assert!(
            output.contains("Ctrl-C"),
            "expected 'Ctrl-C' in output:\n{output}"
        );
        assert!(
            output.contains("Ctrl-T"),
            "expected 'Ctrl-T' in output:\n{output}"
        );
        assert!(
            output.contains("Esc"),
            "expected 'Esc' in output:\n{output}"
        );
    }
}
