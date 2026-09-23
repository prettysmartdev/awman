//! `AgentSetupFrontend` and `HasAgentFrontend` impls for the TUI.

use crate::command::commands::agent_setup::{
    AgentSetupDecision, AgentSetupFrontend, HasAgentFrontend,
};
use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;
use crate::data::session::AgentName;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{AgentSetupState, DialogRequest, DialogResponse};

impl AgentSetupFrontend for TuiCommandFrontend {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        let has_fallback = default_available && default.as_str() != requested.as_str();
        let response = self.ask_dialog(DialogRequest::AgentSetup(AgentSetupState {
            agent_name: requested.as_str().to_string(),
            image_only,
            has_fallback,
            fallback_name: if has_fallback {
                Some(default.as_str().to_string())
            } else {
                None
            },
        }))?;
        Ok(match response {
            DialogResponse::Char('y') | DialogResponse::Yes => AgentSetupDecision::Setup,
            DialogResponse::Char('f') if default_available => AgentSetupDecision::FallbackToDefault,
            _ => AgentSetupDecision::Abort,
        })
    }

    fn record_fallback(&mut self, _requested: &AgentName, fallback: &AgentName) {
        self.messages
            .info(format!("Falling back to agent {}", fallback.as_str()));
    }
}

impl HasAgentFrontend for TuiCommandFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }

    fn container_frontend_for_pty(&mut self) -> Box<dyn AgentFrontend> {
        // Hand the PTY-bridge channels to the engine so the container's PTY
        // master is wired directly to the TUI's vt100 parser. After this the
        // engine drives all stdout/stdin/resize traffic; the TuiCommandFrontend
        // continues to be used for status messages and dialog prompts.
        match self.container_io.take() {
            Some(io) => Box::new(super::TuiContainerProxy::with_io(
                self.status_log.clone(),
                io,
                self.container_name_shared.clone(),
            )),
            None => Box::new(super::TuiContainerProxy::new(self.status_log.clone())),
        }
    }
}

/// One `AgentImageFrontend` for the TUI: `ReadyFrontend` and `InitFrontend`
/// both extend it (F-33), and both reported image-setup steps identically
/// before the merge.
impl crate::engine::agent::AgentImageFrontend for TuiCommandFrontend {
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: crate::data::step_status::StepStatus,
    ) {
        self.messages.info(format!("  {step}: {status:?}"));
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }
}

/// One `AgentLaunchFrontend` for the TUI: `chat`, `exec prompt`,
/// `exec workflow` and `specs` all shared these two methods with identical
/// bodies before F-35.
impl crate::command::commands::agent_setup::AgentLaunchFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }

    fn set_stuck_sender(
        &mut self,
        sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
        >,
    ) {
        if let Ok(mut guard) = self.stuck_sender_shared.lock() {
            *guard = Some(sender);
        }
    }
}
