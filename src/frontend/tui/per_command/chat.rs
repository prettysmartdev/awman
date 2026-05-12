//! `ChatCommandFrontend` impl for the TUI.

use crate::command::commands::chat::ChatCommandFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl ChatCommandFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }
}
