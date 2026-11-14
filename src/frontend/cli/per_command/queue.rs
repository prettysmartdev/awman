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
        // Raw mode turns Ctrl-C into a key press instead of a signal.
        if key.modifiers.contains(event::KeyModifiers::CONTROL)
            && matches!(key.code, event::KeyCode::Char('c' | 'd'))
        {
            return Err("picker cancelled".into());
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
