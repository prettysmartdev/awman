//! Presentation helpers for agent-reported status (OSC 7501), shared by the
//! CLI and the TUI.
//!
//! Layer 3, and only wording and glyphs: which state a report puts an agent in
//! and what the engine does about it is decided in `engine::workflow`. The
//! `msg` an agent reports is untrusted text; everything here treats it as
//! plain characters, never as markup or a path, and truncates it to fit.

use crate::data::program_status::{BlockedKind, EffectiveStatus, ProgramState};
use crate::engine::workflow::actions::CountdownKind;

/// A reported `msg`, made safe to draw: control characters (which the parser
/// already rejects) and bidi/invisible formatting characters removed,
/// whitespace runs collapsed to one space, and truncated to `max_chars`
/// characters with a trailing `…`.
pub fn display_msg(msg: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in msg.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_invisible_format(c) {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    truncate(&out, max_chars)
}

fn is_invisible_format(c: char) -> bool {
    matches!(c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200F}' | '\u{FEFF}')
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let head: String = s.chars().take(keep).collect();
    format!("{head}\u{2026}")
}

/// The tab-bar glyph for a status: `◐` working (with `NN%` when progress is
/// set), `?` blocked on a question, `🔒` blocked on permission/auth, `✓`
/// done, `✗` error. `idle` (turn ended, awaiting input) draws nothing.
pub fn tab_glyph(status: &EffectiveStatus) -> String {
    match status.state {
        ProgramState::Working => match status.progress {
            Some(p) => format!("\u{25D0} {p}%"),
            None => "\u{25D0}".to_string(),
        },
        ProgramState::Blocked => match status.kind {
            Some(BlockedKind::Question) => "?".to_string(),
            _ => "\u{1F512}".to_string(),
        },
        ProgramState::Done => "\u{2713}".to_string(),
        ProgramState::Error => "\u{2717}".to_string(),
        ProgramState::Idle => String::new(),
    }
}

/// Short state text for the workflow overview: `blocked: permission`,
/// `error`, `done`, `working`.
pub fn short_label(status: &EffectiveStatus) -> String {
    match (status.state, status.kind) {
        (ProgramState::Blocked, Some(kind)) => format!("blocked: {}", kind.as_str()),
        (state, _) => state.as_str().to_string(),
    }
}

/// The one-line sentence the CLI prints on a state change, e.g.
/// `Agent is waiting for permission: allow rm -rf build?`.
pub fn status_line(status: &EffectiveStatus) -> String {
    let head = match (status.state, status.kind) {
        (ProgramState::Blocked, Some(BlockedKind::Permission)) => {
            "Agent is waiting for permission".to_string()
        }
        (ProgramState::Blocked, Some(BlockedKind::Question)) => {
            "Agent is waiting for an answer".to_string()
        }
        (ProgramState::Blocked, Some(BlockedKind::Auth)) => {
            "Agent is waiting for you to log in".to_string()
        }
        (ProgramState::Blocked, None) => "Agent is waiting for input".to_string(),
        (ProgramState::Error, _) => "Agent reported an error".to_string(),
        (ProgramState::Done, _) => "Agent reported done".to_string(),
        (ProgramState::Idle, _) => "Agent is idle".to_string(),
        (ProgramState::Working, _) => "Agent is working".to_string(),
    };
    match status
        .msg
        .as_deref()
        .map(|m| display_msg(m, 160))
        .filter(|m| !m.is_empty())
    {
        Some(msg) => format!("{head}: {msg}"),
        None => head,
    }
}

/// The countdown wording for each [`CountdownKind`], e.g.
/// `Agent reported done: advancing in 8s`.
pub fn countdown_label(kind: CountdownKind, remaining_secs: u64) -> String {
    match kind {
        CountdownKind::StuckStep => format!("No output: advancing in {remaining_secs}s"),
        CountdownKind::FailureRetry => format!("Step failed: retrying in {remaining_secs}s"),
        CountdownKind::AgentDone => {
            format!("Agent reported done: advancing in {remaining_secs}s")
        }
        CountdownKind::AgentBlocked => {
            format!("Agent is waiting for input: failing step in {remaining_secs}s")
        }
        CountdownKind::AgentError => {
            format!("Agent reported an error: failing step in {remaining_secs}s")
        }
    }
}

/// Whether a countdown of this kind ends in a failed step, so a dialog must
/// never label its "act now" key as "advance".
pub fn countdown_fails_step(kind: CountdownKind) -> bool {
    matches!(
        kind,
        CountdownKind::AgentBlocked | CountdownKind::AgentError
    )
}

/// Emits at most one status line per state change: a report that only changes
/// progress or message, or repeats the same state and kind, prints nothing.
#[derive(Debug, Default)]
pub struct StatusLineGate {
    last: Option<(ProgramState, Option<BlockedKind>)>,
}

impl StatusLineGate {
    /// The line to print for this report, or `None` when the state did not
    /// change. A cleared status (`None`) resets the gate and prints nothing.
    pub fn line_for(&mut self, status: Option<&EffectiveStatus>) -> Option<String> {
        let Some(status) = status else {
            self.last = None;
            return None;
        };
        let key = (status.state, status.kind);
        if self.last == Some(key) {
            return None;
        }
        self.last = Some(key);
        Some(status_line(status))
    }
}
