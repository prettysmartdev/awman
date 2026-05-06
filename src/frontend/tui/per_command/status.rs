//! `StatusCommandFrontend` impl for the TUI.

use crate::command::commands::status::StatusCommandFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl StatusCommandFrontend for TuiCommandFrontend {
    fn should_continue_watching(&mut self) -> bool {
        true
    }

    fn write_clear_marker(&mut self) {
        if let Ok(mut log) = self.status_log.lock() {
            log.clear();
        }
    }
}
