//! `AgentSetupFrontend` and `HasAgentFrontend` impls for the TUI.

use crate::command::commands::agent_setup::{
    AgentSetupDecision, AgentSetupFrontend, HasAgentFrontend,
};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{AgentSetupState, DialogRequest, DialogResponse};
use crate::data::message::UserMessageSink;
use crate::data::session::AgentName;
use crate::engine::agent_runtime::frontend::AgentFrontend;

impl AgentSetupFrontend for InstanceCommandFrontend {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.agent_setup(default_available));
        }
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

impl HasAgentFrontend for InstanceCommandFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        if self.is_cli() {
            return Box::new(super::InstanceContainerProxy::with_io(
                self.status_log.clone(),
                self.command_bytes.pipe(self.wake.clone()),
                None,
            ));
        }
        Box::new(super::InstanceContainerProxy::new(self.status_log.clone()))
    }

    fn container_frontend_for_pty(&mut self) -> Box<dyn AgentFrontend> {
        if self.is_headless_cli() {
            return HasAgentFrontend::container_frontend(self);
        }
        // Hand the PTY-bridge channels to the engine so the container's PTY
        // master is wired directly to the TUI's vt100 parser. After this the
        // engine drives all stdout/stdin/resize traffic; the InstanceCommandFrontend
        // continues to be used for status messages and dialog prompts.
        match self.container_io.take() {
            Some(io) => Box::new(super::InstanceContainerProxy::with_io(
                self.status_log.clone(),
                io,
                (!self.parallel_group_active).then(|| self.container.clone()),
            )),
            None => Box::new(super::InstanceContainerProxy::new(self.status_log.clone())),
        }
    }
}

/// One `AgentImageFrontend` for the TUI: `ReadyFrontend` and `InitFrontend`
/// both extend it (F-33), and both reported image-setup steps identically
/// before the merge.
impl crate::engine::agent::AgentImageFrontend for InstanceCommandFrontend {
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: crate::data::step_status::StepStatus,
    ) {
        self.messages.info(format!("  {step}: {status:?}"));
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        if self.is_cli() {
            return HasAgentFrontend::container_frontend(self);
        }
        Box::new(super::InstanceContainerProxy::new(self.status_log.clone()))
    }
}

/// One `AgentLaunchFrontend` for the TUI: `chat`, `exec prompt`,
/// `exec workflow` and `specs` all shared these two methods with identical
/// bodies before F-35.
impl crate::command::commands::agent_setup::AgentLaunchFrontend for InstanceCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
        if self.is_cli() {
            if active {
                self.command_bytes.started();
            } else {
                self.command_bytes.ended();
            }
        }
    }

    fn set_stuck_sender(
        &mut self,
        sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
        >,
    ) {
        self.container
            .attach_engine(crate::engine::workflow::frontend::EngineHandles::stuck(
                sender,
            ));
    }
}
