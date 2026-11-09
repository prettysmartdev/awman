//! TUI hooks for the remote command family.

use crate::command::commands::remote::RemoteCommandFrontend;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::message::UserMessageSink;

impl RemoteCommandFrontend for InstanceCommandFrontend {
    /// The same box `ready` draws, one status-log line per row.
    fn report_ready_summary(&mut self, summary: &crate::data::ready_summary::ReadySummary) {
        let text = self.presentation.ready_summary(summary);
        for line in text.lines() {
            self.messages.info(line.to_string());
        }
    }
}
