//! The TUI's command frontend.
//!
//! The per-command frontend implementation that runs on a tab's command
//! thread is Layer 2 since WI 0142 (`command::instance::frontend::
//! InstanceCommandFrontend`, re-exported here as `TuiCommandFrontend`). What
//! stays in the TUI is [`TuiPresentation`]: the wording, glyph, box-drawing,
//! and host-output filtering decisions the producer asks its
//! presenter for, implemented with the same Layer 3 helpers as before.

pub use crate::command::instance::frontend::{
    DialogChannels, HostByteFilter, InstanceCommandFrontend as TuiCommandFrontend,
    InstancePresentation,
};

use crate::data::program_status::EffectiveStatus;
use crate::data::ready_summary::ReadySummary;
use crate::data::terminal::HostOutputPolicy;
use crate::frontend::terminal_output_filter::HostOutputFilter;

/// The TUI's [`InstancePresentation`]: exactly the helpers the command
/// frontend called directly before it moved to Layer 2.
#[derive(Debug, Clone, Copy, Default)]
pub struct TuiPresentation;

struct TuiHostFilter(HostOutputFilter);

impl HostByteFilter for TuiHostFilter {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.0.feed(chunk, out);
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        self.0.finish(out);
    }
}

impl InstancePresentation for TuiPresentation {
    fn ready_summary(&self, summary: &ReadySummary) -> String {
        crate::frontend::render_helpers::render_ready_summary(summary)
    }

    fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        crate::frontend::render_helpers::format_table(headers, rows)
    }

    fn status_short_label(&self, status: &EffectiveStatus) -> String {
        crate::frontend::program_status_text::short_label(status)
    }

    fn status_line(&self, status: &EffectiveStatus) -> String {
        crate::frontend::program_status_text::status_line(status)
    }

    fn host_output_filter(&self, policy: HostOutputPolicy) -> Box<dyn HostByteFilter> {
        Box::new(TuiHostFilter(HostOutputFilter::new(policy)))
    }
}
