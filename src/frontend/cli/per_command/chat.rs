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

impl ChatCommandFrontend for CliFrontend {}

/// One `AgentLaunchFrontend` for the CLI: `chat`, `exec prompt`,
/// `exec workflow` and `specs` all gated host stdio the same way before F-35.
impl crate::command::commands::agent_setup::AgentLaunchFrontend for CliFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.messages.set_pty_active(active);
        if !active && self.is_agent_attach() {
            self.unbind_container_stdio();
        }
    }

    /// `chat` and `exec prompt`: print one status line per state change the
    /// agent reports (OSC 7501), through the same stderr sink as every other
    /// CLI message (held back while a PTY owns the terminal). Reports still
    /// pass through to the user's own terminal unchanged.
    fn set_stuck_sender(
        &mut self,
        sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
        >,
    ) {
        if self.is_json_mode() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let mut rx = sender.subscribe();
        let sink = self.messages.handle();
        tokio::spawn(async move {
            use crate::data::message::{MessageLevel, UserMessage};
            use crate::data::program_status::ProgramState;
            use crate::engine::agent_runtime::StuckEvent;
            let mut gate = crate::frontend::program_status_text::StatusLineGate::default();
            loop {
                match rx.recv().await {
                    Ok(StuckEvent::Reported(status)) => {
                        if let Some(text) = gate.line_for(status.as_ref()) {
                            let level = match status.map(|s| s.state) {
                                Some(ProgramState::Error) => MessageLevel::Error,
                                Some(ProgramState::Blocked) => MessageLevel::Warning,
                                _ => MessageLevel::Info,
                            };
                            sink.write_message(UserMessage { level, text });
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// One `AgentImageFrontend` for the CLI: `ReadyFrontend` and `InitFrontend`
/// both extend it (F-33). The `--json` guard comes from the `ready` impl;
/// `init` has no `--json` flag, so `is_json_mode()` is always false there
/// and the merge is behaviour-preserving.
impl crate::engine::agent::AgentImageFrontend for CliFrontend {
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: crate::data::step_status::StepStatus,
    ) {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use crate::data::step_status::StepStatus;
        // When --json is active, suppress human-readable output on stderr.
        if self.is_json_mode() {
            return;
        }
        let level = match status {
            StepStatus::Failed(_) => MessageLevel::Error,
            _ => MessageLevel::Info,
        };
        self.messages.write_message(UserMessage {
            level,
            text: format!("{step}: {}", super::helpers::step_status_label(&status)),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }
}
