//! Pure-presentation helpers for the CLI frontend.
//!
//! Color/hyperlink decisions, exit-code maps for typed outcome variants,
//! and any other terminal-styling logic that does NOT depend on Layer 2
//! semantics belongs here.

use std::io::IsTerminal;

/// `true` when stderr is connected to a TTY (used to decide whether to
/// emit colored escape codes for warnings/errors).
pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// `true` when stdin is connected to a TTY. Drives the CLI frontend's
/// safe-default fallback for interactive prompts when stdin is piped.
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// `true` when stdout is connected to a TTY. Drives hyperlink (OSC 8)
/// emission and table-width-aware rendering.
pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// `true` when color output should be emitted: `NO_COLOR` env var unset
/// AND stderr is a TTY (the standard convention).
pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    stderr_is_tty()
}

/// Best-effort terminal width (columns) — returns `None` when stdout is
/// not a TTY or when the terminal size cannot be determined.
pub fn terminal_width() -> Option<u16> {
    if !stdout_is_tty() {
        return None;
    }
    crossterm::terminal::size().ok().map(|(w, _h)| w)
}

/// Wrap `text` in an OSC 8 hyperlink escape sequence pointing at `url` when
/// stdout is a TTY and color/escape output is enabled. Returns the plain
/// `text` otherwise.
pub fn hyperlink(text: &str, url: &str) -> String {
    if !stdout_is_tty() || !color_enabled() {
        return text.to_string();
    }
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}
