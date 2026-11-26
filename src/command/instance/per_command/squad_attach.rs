//! Register the local attach lifecycle before a submitted command starts.
use crate::command::instance::feeds::SharedSquadAttach;
use crate::command::instance::frontend::InstanceCommandFrontend;

impl InstanceCommandFrontend {
    pub fn with_squad_attach(mut self, attach: Option<SharedSquadAttach>) -> Self {
        if let Some(attach) = attach {
            // Every new command replaces the previous local viewer. This runs
            // synchronously at submit, before any daemon request or slot event.
            attach.session.end();
            if crate::command::dispatch::catalogue::CommandCatalogue::get()
                .is_terminal_attach(&self.parsed.path)
            {
                self.attach_lease =
                    Some(crate::command::instance::feeds::AttachSessionLease::begin(
                        attach.session,
                        &self.parsed.path.join(" "),
                        self.command_cancel.clone(),
                    ));
            }
        }
        self
    }
}
