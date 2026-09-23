//! TUI hooks for the remote command family.

use crate::command::commands::remote::RemoteCommandFrontend;
use crate::data::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl RemoteCommandFrontend for TuiCommandFrontend {
    /// The same box `ready` draws, one status-log line per row.
    fn report_ready_summary(&mut self, summary: &crate::data::ready_summary::ReadySummary) {
        for line in crate::frontend::render_helpers::render_ready_summary(summary).lines() {
            self.messages.info(line.to_string());
        }
    }
}
