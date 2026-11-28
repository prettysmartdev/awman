//! Adopt the generation registered by the presenter before submit.
use crate::command::instance::feeds::SharedSquadAttach;
use crate::command::instance::frontend::InstanceCommandFrontend;

impl InstanceCommandFrontend {
    pub fn with_squad_attach(mut self, attach: Option<SharedSquadAttach>) -> Self {
        if let Some(attach) = attach {
            self.attach_daemon_reachable = Some(attach.daemon_reachable);
            if let Some(generation) = attach.generation {
                self.attach_lease =
                    Some(crate::command::instance::feeds::AttachSessionLease::adopt(
                        attach.session,
                        generation,
                    ));
            }
        }
        self
    }
}
