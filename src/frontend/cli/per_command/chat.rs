//! `ChatCommandFrontend` impl for the CLI.
//!
//! `ChatCommandFrontend` requires a `container_frontend()` accessor on
//! top of `UserMessageSink + MountScopeFrontend + AgentSetupFrontend +
//! AgentAuthFrontend`. The supertraits are already implemented on
//! `CliFrontend`; we only need to provide the accessor here.

use crate::command::commands::agent_setup::HasAgentFrontend;
use crate::command::commands::chat::ChatCommandFrontend;
use crate::engine::agent_runtime::frontend::AgentFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

impl HasAgentFrontend for CliFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn container_frontend_for_pty(&mut self) -> Box<dyn AgentFrontend> {
        if self.non_interactive {
            return self.container_frontend();
        }
        let io = self.take_interactive_io();
        Box::new(
            super::container_frontend_marker::CliInteractiveContainerProxy {
                container_io: Some(io),
            },
        )
    }
}

impl ChatCommandFrontend for CliFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.messages.set_pty_active(active);
    }
}
