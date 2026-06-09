//! `ChatCommandFrontend` impl for the TUI.

use crate::command::commands::chat::ChatCommandFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl ChatCommandFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }

    fn set_stuck_sender(
        &mut self,
        sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::container::instance::StuckEvent>,
        >,
    ) {
        if let Ok(mut guard) = self.stuck_sender_shared.lock() {
            *guard = Some(sender);
        }
    }
}
