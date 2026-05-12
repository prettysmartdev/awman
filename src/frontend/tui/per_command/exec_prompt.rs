//! `ExecPromptCommandFrontend` impl for the TUI.

use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl ExecPromptCommandFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }
}
