//! `ChatCommandFrontend` impl for the TUI.

use crate::command::commands::chat::ChatCommandFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl ChatCommandFrontend for TuiCommandFrontend {
    fn report_session_worktree(&mut self, path: &std::path::Path, _branch: Option<&str>) {
        self.workflow.set_worktree_path(Some(path.to_path_buf()));
    }
}
