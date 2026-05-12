//! `AgentSetupFrontend` impl for the CLI.
//!
//! The safe non-interactive default is
//! `Setup` (proceed with download/build). The CLI prompts on stdin only
//! when stdin is a TTY; otherwise it returns the safe default.

use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::message::{MessageLevel, UserMessageSink};

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

impl AgentSetupFrontend for CliFrontend {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        if !stdin_is_tty() {
            return Ok(AgentSetupDecision::Setup);
        }
        let action = if image_only {
            format!("Build image for {}", requested.as_str())
        } else {
            format!("Set up agent {}", requested.as_str())
        };
        eprintln!(
            "amux: {action}? [y]es / [n]o{}",
            if default_available && default.as_str() != requested.as_str() {
                format!(" / [f]allback to {}", default.as_str())
            } else {
                String::new()
            }
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(AgentSetupDecision::Abort);
        }
        Ok(match buf.trim() {
            "y" | "Y" | "" => AgentSetupDecision::Setup,
            "f" | "F" if default_available && default.as_str() != requested.as_str() => {
                AgentSetupDecision::FallbackToDefault
            }
            _ => AgentSetupDecision::Abort,
        })
    }

    fn record_fallback(&mut self, _requested: &AgentName, fallback: &AgentName) {
        // Per-step fallback caching is a TUI-only concern. The CLI never
        // re-prompts within a single invocation.
        let level = MessageLevel::Info;
        self.messages
            .write_message(crate::engine::message::UserMessage {
                level,
                text: format!("falling back to agent {}", fallback.as_str()),
            });
    }
}
