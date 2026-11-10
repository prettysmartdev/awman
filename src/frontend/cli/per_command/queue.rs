//! CLI queue listing and manual picker.
use std::io::Write;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::{cursor, execute, terminal};

use crate::command::daemon_api::worker_pull::QueueRow;

fn age(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

struct RawModeGuard;
impl RawModeGuard {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// What a key press did to the picker.
#[derive(Debug, PartialEq, Eq)]
enum PickerOutcome {
    Continue,
    /// Index into the *visible* (filtered) rows.
    Picked(usize),
    Cancelled,
}

/// Picker state: filter text, whether `/` opened the filter prompt, and the
/// selected row of the visible list.
#[derive(Debug, Default)]
struct PickerState {
    filter: String,
    filtering: bool,
    selected: usize,
}

impl PickerState {
    fn handle(&mut self, code: KeyCode, rows: &[QueueRow]) -> PickerOutcome {
        let visible = filtered(rows, &self.filter).len();
        match code {
            KeyCode::Esc if self.filtering => {
                self.filtering = false;
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Esc => return PickerOutcome::Cancelled,
            KeyCode::Enter if self.filtering => self.filtering = false,
            KeyCode::Enter if visible > 0 => {
                return PickerOutcome::Picked(self.selected.min(visible - 1));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down if visible > 0 => self.selected = (self.selected + 1).min(visible - 1),
            KeyCode::Char('/') if !self.filtering => self.filtering = true,
            KeyCode::Char(ch) if self.filtering => {
                self.filter.push(ch);
                self.selected = 0;
            }
            KeyCode::Backspace if self.filtering => {
                self.filter.pop();
                self.selected = 0;
            }
            _ => {}
        }
        PickerOutcome::Continue
    }
}

/// Arrow-key picker. `/` opens the filter prompt; Enter closes the prompt or
/// picks; Esc closes the prompt, then cancels.
pub(crate) fn picker(rows: &[QueueRow]) -> Result<QueueRow, String> {
    if rows.is_empty() {
        return Err("there are no eligible work items to pick".into());
    }
    let _raw = RawModeGuard::enter().map_err(|e| format!("could not enter picker mode: {e}"))?;
    let mut out = std::io::stdout();
    let mut state = PickerState::default();
    loop {
        execute!(
            out,
            terminal::Clear(terminal::ClearType::All),
            cursor::MoveTo(0, 0)
        )
        .map_err(|e| e.to_string())?;
        // Raw mode does not translate a bare line feed into CR LF.
        write!(
            out,
            "{}",
            picker_snapshot(rows, &state).replace('\n', "\r\n")
        )
        .map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        if !event::poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match state.handle(key.code, rows) {
            PickerOutcome::Continue => {}
            PickerOutcome::Cancelled => return Err("picker cancelled".into()),
            PickerOutcome::Picked(index) => {
                return Ok(filtered(rows, &state.filter)[index].clone());
            }
        }
    }
}

fn filtered<'a>(rows: &'a [QueueRow], filter: &str) -> Vec<&'a QueueRow> {
    rows.iter()
        .filter(|row| {
            filter.is_empty()
                || format!("{} {} {}", row.id, row.title, row.server)
                    .to_lowercase()
                    .contains(&filter.to_lowercase())
        })
        .collect()
}

#[cfg(test)]
fn can_pick_interactively(has_id: bool, json: bool, stdin_is_tty: bool) -> bool {
    has_id || (!json && stdin_is_tty)
}

fn picker_snapshot(rows: &[QueueRow], state: &PickerState) -> String {
    let visible = filtered(rows, &state.filter);
    let mut lines = vec![
        "Pick a work item  \u{2191}/\u{2193} move \u{b7} / filter \u{b7} Enter pick \u{b7} Esc cancel"
            .to_string(),
        format!(
            "Filter: {}{}",
            state.filter,
            if state.filtering { "_" } else { "" }
        ),
    ];
    if visible.is_empty() {
        lines.push("  No matching work items.".to_string());
    }
    for (i, row) in visible.iter().enumerate() {
        lines.push(format!(
            "{} {}  {}  [{}]  priority {}  {}{}",
            if i == state.selected { ">" } else { " " },
            row.id,
            row.title,
            row.server,
            row.priority,
            age(row.age_ms),
            if row.for_you { "  \u{b7} for you" } else { "" }
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use awman_wsp::WorkKind;
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn row(id: &str, title: &str, for_you: bool) -> QueueRow {
        QueueRow {
            body: None,
            source_url: None,
            id: id.into(),
            server: "team".into(),
            title: title.into(),
            summary: None,
            kind: WorkKind::Workflow,
            labels: BTreeMap::from([("area".into(), "cli".into())]),
            priority: 3,
            enqueued_at: Utc::now(),
            age_ms: 65_000,
            for_you,
        }
    }

    fn two_rows() -> Vec<QueueRow> {
        vec![
            row("01J", "Update CLI", true),
            row("02J", "Fix docs", false),
        ]
    }

    const HEADER: &str =
        "Pick a work item  \u{2191}/\u{2193} move \u{b7} / filter \u{b7} Enter pick \u{b7} Esc cancel";

    #[test]
    fn snapshot_of_an_empty_picker() {
        let screen = picker_snapshot(&[], &PickerState::default());
        assert_eq!(
            screen,
            format!("{HEADER}\nFilter: \n  No matching work items.")
        );
    }

    #[test]
    fn snapshot_of_a_populated_picker_marks_selection_and_for_you() {
        let screen = picker_snapshot(&two_rows(), &PickerState::default());
        assert_eq!(
            screen,
            format!(
                "{HEADER}\nFilter: \n> 01J  Update CLI  [team]  priority 3  1m  \u{b7} for you\n  02J  Fix docs  [team]  priority 3  1m"
            )
        );
    }

    #[test]
    fn snapshot_of_a_filtered_picker_shows_only_matches_and_the_prompt() {
        let rows = two_rows();
        let mut state = PickerState::default();
        for code in [
            KeyCode::Char('/'),
            KeyCode::Char('d'),
            KeyCode::Char('o'),
            KeyCode::Char('c'),
        ] {
            assert_eq!(state.handle(code, &rows), PickerOutcome::Continue);
        }
        let screen = picker_snapshot(&rows, &state);
        assert_eq!(
            screen,
            format!("{HEADER}\nFilter: doc_\n> 02J  Fix docs  [team]  priority 3  1m")
        );
        // A filter with no match says so instead of drawing nothing.
        state.handle(KeyCode::Char('z'), &rows);
        assert!(picker_snapshot(&rows, &state).contains("No matching work items."));
    }

    #[test]
    fn the_for_you_marker_is_only_on_rows_that_are_for_you() {
        let screen = picker_snapshot(&two_rows(), &PickerState::default());
        assert_eq!(screen.matches("for you").count(), 1);
        assert!(screen.lines().nth(2).unwrap().ends_with("for you"));
    }

    #[test]
    fn arrows_move_and_enter_picks_the_selected_row() {
        let rows = two_rows();
        let mut state = PickerState::default();
        assert_eq!(state.handle(KeyCode::Down, &rows), PickerOutcome::Continue);
        assert_eq!(state.handle(KeyCode::Down, &rows), PickerOutcome::Continue);
        assert_eq!(state.selected, 1, "selection clamps at the last row");
        assert_eq!(state.handle(KeyCode::Up, &rows), PickerOutcome::Continue);
        assert_eq!(state.handle(KeyCode::Down, &rows), PickerOutcome::Continue);
        assert_eq!(
            state.handle(KeyCode::Enter, &rows),
            PickerOutcome::Picked(1)
        );
    }

    #[test]
    fn slash_opens_the_filter_and_letters_are_ignored_before_it() {
        let rows = two_rows();
        let mut state = PickerState::default();
        state.handle(KeyCode::Char('d'), &rows);
        assert_eq!(state.filter, "", "typing without / does not filter");
        state.handle(KeyCode::Char('/'), &rows);
        assert!(state.filtering);
        state.handle(KeyCode::Char('d'), &rows);
        assert_eq!(state.filter, "d");
        state.handle(KeyCode::Backspace, &rows);
        assert_eq!(state.filter, "");
    }

    #[test]
    fn enter_in_the_filter_prompt_keeps_the_filter_then_picks_the_match() {
        let rows = two_rows();
        let mut state = PickerState::default();
        for code in [
            KeyCode::Char('/'),
            KeyCode::Char('f'),
            KeyCode::Char('i'),
            KeyCode::Char('x'),
        ] {
            state.handle(code, &rows);
        }
        assert_eq!(state.handle(KeyCode::Enter, &rows), PickerOutcome::Continue);
        assert!(!state.filtering);
        assert_eq!(state.filter, "fix");
        let visible = filtered(&rows, &state.filter);
        assert_eq!(
            state.handle(KeyCode::Enter, &rows),
            PickerOutcome::Picked(0)
        );
        assert_eq!(visible[0].id, "02J");
    }

    #[test]
    fn esc_closes_the_filter_first_and_then_cancels() {
        let rows = two_rows();
        let mut state = PickerState::default();
        state.handle(KeyCode::Char('/'), &rows);
        state.handle(KeyCode::Char('x'), &rows);
        assert_eq!(state.handle(KeyCode::Esc, &rows), PickerOutcome::Continue);
        assert!(!state.filtering && state.filter.is_empty());
        assert_eq!(state.handle(KeyCode::Esc, &rows), PickerOutcome::Cancelled);
    }

    #[test]
    fn enter_with_no_visible_rows_does_nothing() {
        let rows = two_rows();
        let mut state = PickerState::default();
        for code in [KeyCode::Char('/'), KeyCode::Char('z'), KeyCode::Enter] {
            state.handle(code, &rows);
        }
        assert_eq!(state.handle(KeyCode::Enter, &rows), PickerOutcome::Continue);
    }

    #[test]
    fn queue_json_shape_is_items_with_rows() {
        let value = serde_json::json!({"items": [row("01J", "Update CLI", true)]});
        assert!(value["items"].is_array());
        assert_eq!(value["items"][0]["for_you"], true);
        assert_eq!(value["items"][0]["server"], "team");
    }

    #[test]
    fn pick_requires_an_id_without_a_tty_and_json_never_prompts() {
        assert!(!can_pick_interactively(false, false, false));
        assert!(!can_pick_interactively(false, true, true));
        assert!(can_pick_interactively(true, true, false));
    }

    #[test]
    fn pick_without_an_id_uses_the_picker_only_for_a_tty() {
        assert!(can_pick_interactively(false, false, true));
        assert!(!can_pick_interactively(false, false, false));
    }
}
