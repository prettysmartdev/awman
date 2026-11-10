//! Pure-presentation dialog widgets for the TUI.
//!
//! Each dialog captures keyboard input while open, renders centered in the
//! terminal, and returns a typed Layer 2 enum value when the user responds.
//! Cancellable with Esc.

pub use crate::data::config::show::{ConfigShowRejectedEdit, ConfigShowRow};
pub use crate::data::dialog::*;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::command::commands::squad::commands::SquadConfirmDecision;
use crate::command::dispatch::frontend_action::FrontendAction;
use crate::data::prompt::Prompt;
use crate::frontend::tui::text_edit::TextEdit;

/// Title of the `Ctrl-T` New Tab dialog. The key handler's `Ctrl-S` intercept
/// and the renderer's `[Ctrl+S] open squad` hint are both keyed off this one
/// string, so the shortcut and its advertisement can never disagree.
pub(crate) const NEW_TAB_DIALOG_TITLE: &str = "New Tab";

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
    /// Masked input (WI 0149). Not a `TextEdit`: the buffer is zeroized on
    /// drop and has no cursor movement, so nothing about it is worth keeping.
    SecretInput {
        title: String,
        prompt: String,
        buffer: SecretBuffer,
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
    WorkflowYoloCountdown(WorkflowYoloCountdownState),
    WorkflowStepConfirm(WorkflowStepConfirmState),
    AgentSetup(AgentSetupState),
    MountScope(MountScopeState),
    AgentAuth(AgentAuthState),
    /// The `Ctrl-Q` work-queue panel and its item body view (WI 0143).
    QueuePanel(crate::frontend::tui::queue::QueuePanelState),
    /// Release or cancel the lease of a work-item tab being closed mid-run.
    LeaseCloseConfirm(crate::frontend::tui::queue::LeaseCloseState),
    QuitConfirm,
    CloseTabConfirm,
    WorkflowCancelConfirm,
    ConfigShow(ConfigShowState),
    /// Task detail for the squad tab (WI 0102). Kept live by
    /// `App::tick_all_tabs` from the active squad tab's snapshot. Run history
    /// lives in its own modal (`SquadTaskHistory`), reached with `h`.
    SquadTaskDetail(SquadDetailState),
    /// Run history for one squad task, in a modal of its own so a long task
    /// description can never push the history off the bottom of the detail
    /// modal. Opened with `h` from the card grid or from the detail modal.
    SquadTaskHistory(SquadHistoryState),
    /// Confirmation before removing a squad task (WI 0102). `y` dispatches
    /// `squad remove <name>`; `n`/`Esc` dismisses.
    SquadRemoveConfirm {
        name: String,
    },
    /// Confirmation before triggering, cancelling or pausing a squad task
    /// from the card grid or the detail modal.
    ///
    /// Everything the modal says is `prompt`'s: Layer 2 built it
    /// (`SquadCommand::confirm_prompt`) and Layer 2 knows which command
    /// `action` dispatches to. This dialog holds the two facts the frontend
    /// does own — which task, and which answer a keypress is.
    SquadActionConfirm {
        action: FrontendAction,
        name: String,
        prompt: Prompt<SquadConfirmDecision>,
    },
    /// Confirmation before enabling squad in the global config or starting
    /// the daemon. Each flag describes the change the user is accepting.
    SquadStartConfirm {
        enable_config: bool,
        start_daemon: bool,
    },
    Loading {
        title: String,
    },
    Custom {
        title: String,
        body: String,
        keys: Vec<(char, String)>,
    },
    Review(ReviewState),
    /// Unrecoverable startup error (e.g. an invalid `runtime:` value in the
    /// global config). The only action is quitting: Enter, Esc, and Ctrl-C
    /// all exit the TUI.
    FatalError {
        title: String,
        body: String,
    },
    /// One-shot informational modal with no command thread behind it: Enter and
    /// Esc simply dismiss it. Used for the squad key-setup snippet, which must
    /// be readable long enough to copy and must not scroll away in a status bar.
    Notice {
        title: String,
        body: String,
        /// The raw squad bearer key, when this notice is the key-setup
        /// snippet — `[c]` copies it to the clipboard. `None` for notices
        /// unrelated to a key (e.g. "daemon did not start"), which shows no
        /// copy hint.
        copy_key: Option<String>,
        /// The bare shell export line alone, so `[z]` can copy just what
        /// belongs in the rc file, without the banner and notes around it.
        copy_zshrc_snippet: Option<String>,
    },
}

/// The typed characters of a [`Dialog::SecretInput`]. Append-only editing, a
/// redacted `Debug`, and a best-effort overwrite of the buffer when dropped
/// (cancel, submit, or the dialog being replaced).
pub struct SecretBuffer {
    text: crate::engine::auth::credential::SecretString,
}

impl Default for SecretBuffer {
    fn default() -> Self {
        Self {
            text: crate::engine::auth::credential::SecretString::input_buffer(),
        }
    }
}

impl SecretBuffer {
    pub fn insert_char(&mut self, ch: char) {
        self.text.push_char(ch);
    }

    pub fn backspace(&mut self) {
        self.text.pop_char();
    }

    /// Number of characters typed, for drawing one `•` each.
    pub fn len(&self) -> usize {
        self.text.expose().chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.expose().is_empty()
    }

    /// Move the typed value out as a `SecretString`, leaving the buffer empty.
    pub fn take(&mut self) -> crate::engine::auth::credential::SecretString {
        std::mem::replace(
            &mut self.text,
            crate::engine::auth::credential::SecretString::input_buffer(),
        )
    }
}

impl std::fmt::Debug for SecretBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBuffer(<redacted>)")
    }
}

/// State for the squad task-detail modal (WI 0102). `name` is the identity
/// used by `tick_all_tabs` to refresh `task` from the tab snapshot each tick.
/// Run history is not part of this modal — see `SquadHistoryState`.
#[derive(Debug, Clone)]
pub struct SquadDetailState {
    pub name: String,
    pub task: crate::data::fs::task_store::Task,
}

/// State for the squad run-history modal. `name` is the task whose runs are
/// shown, and the identity `tick_all_tabs` refreshes `runs` against;
/// `selected` is the highlighted row, moved with the arrow keys (the table
/// scrolls to keep it in view). `from_detail` records where the modal was
/// opened from: Esc reopens the detail modal only when the user came from it,
/// and simply closes when the modal was opened straight from the card grid.
/// `run_detail` is the per-run modal Enter opens over the table; while it is
/// up, the arrow keys scroll it and Esc returns to the table.
#[derive(Debug, Clone)]
pub struct SquadHistoryState {
    pub name: String,
    pub runs: Vec<crate::data::fs::task_store::Run>,
    pub selected: usize,
    pub from_detail: bool,
    pub run_detail: Option<SquadRunDetailState>,
}

/// State for the per-run detail modal stacked over the run history: the run
/// itself (refreshed by id while the history stays live) and how far its
/// reason/error text is scrolled. `max_scroll` is written by the renderer —
/// the only place the wrapped line count is known — so the scroll keys stop
/// at the last line instead of running on into blank space.
#[derive(Debug, Clone)]
pub struct SquadRunDetailState {
    pub run: crate::data::fs::task_store::Run,
    pub scroll: usize,
    pub max_scroll: std::cell::Cell<usize>,
}

pub struct ConfigShowState {
    pub rows: Vec<ConfigShowRow>,
    pub selected: usize,
    pub editing: bool,
    pub edit_column: usize,
    pub editor: TextEdit,
    /// In-progress Ctrl+N add-entry flow, if any. While `Some`,
    /// `editing` is also true so text input routes to `editor`.
    pub new_entry: Option<NewMapEntryPhase>,
    /// Why the last save attempt was rejected. Rendered in the dialog until
    /// the user cancels the edit or starts a new one.
    pub error: Option<String>,
}

impl ConfigShowState {
    /// Members follow their header in the command's row projection. Use the
    /// nearest header so separate maps or lists keep their own editors.
    pub(crate) fn selected_header(&self) -> Option<(usize, &ConfigShowRow)> {
        self.rows
            .get(..=self.selected)?
            .iter()
            .enumerate()
            .rev()
            .find(|(_, row)| row.shape.is_header())
    }
}

/// Phase of a Ctrl+N add-entry flow in the config dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewMapEntryPhase {
    /// Typing the new map key.
    Key,
    /// Typing the comma-separated model list for the confirmed key.
    Value { key: String },
    /// Typing a new list entry. Single-phase:
    /// only the entry text is needed; its index is assigned
    /// automatically by appending.
    ListEntry,
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
    // The dialog frame costs 4 rows (two borders + a row of padding each
    // side), and the content is `body_h` rows plus a blank separator plus the
    // key-hint row. Anything less than `body_h + 6` clips the hint off the
    // bottom — which is exactly the row a user needs to know that `y`/`n`/Esc
    // are the answers. `.max(8)` keeps a one-line body's dialog from looking
    // cramped.
    let height = (body_h + 6).min(area.height.saturating_sub(2)).max(8);
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
    let inner = render_dialog_frame("Quit awman?", Color::Yellow, dialog_area, frame);
    let text = "  Press Ctrl-C again to quit awman\n\n  [Esc] cancel";
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
        "  Press Ctrl-C again to quit awman\n  Press Ctrl-T to close this tab\n\n  [Esc] cancel";
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
    let text = "  Cancel workflow execution?\n\n  The running container will be killed and the\n  current step returned to Pending for resumption.\n\n  [y] cancel execution   [n / Esc] keep running";
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

/// State for [`Dialog::Review`]. `scroll` is the first visible wrapped body
/// row; the renderer publishes `max_scroll` (it alone knows the wrapped
/// height). Until the first render `max_scroll` is `usize::MAX`, so a gated
/// key cannot be accepted before the user has seen anything.
#[derive(Debug)]
pub struct ReviewState {
    pub request: ReviewRequest,
    pub scroll: usize,
    pub max_scroll: std::cell::Cell<usize>,
    pub satisfied_expanded: bool,
}

impl ReviewState {
    pub fn new(
        title: String,
        body: String,
        keys: Vec<(char, String)>,
        gated_keys: Vec<char>,
    ) -> Self {
        ReviewRequest::new(title, body, keys, gated_keys).into()
    }
    pub fn question<D: PartialEq>(prompt: &Prompt<D>, body: ReviewBody) -> Self {
        ReviewRequest::question(prompt, body).into()
    }
    pub fn with_body(mut self, body: ReviewBody) -> Self {
        self.request.body = body;
        self
    }
    pub fn with_note(mut self, note: String) -> Self {
        self.request.note = note;
        self
    }
    /// Whether the end of the body has been on screen.
    pub fn fully_seen(&self) -> bool {
        self.scroll >= self.max_scroll.get()
    }

    /// Whether pressing `c` answers the dialog now.
    pub fn accepts(&self, c: char) -> bool {
        self.keys.iter().any(|(ch, _)| *ch == c)
            && (!self.gated_keys.contains(&c) || self.fully_seen())
    }
}

impl From<ReviewRequest> for ReviewState {
    fn from(request: ReviewRequest) -> Self {
        Self {
            request,
            scroll: 0,
            max_scroll: std::cell::Cell::new(usize::MAX),
            satisfied_expanded: false,
        }
    }
}
impl std::ops::Deref for ReviewState {
    type Target = ReviewRequest;
    fn deref(&self) -> &Self::Target {
        &self.request
    }
}
impl std::ops::DerefMut for ReviewState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.request
    }
}
