//! `AgentAuthFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin only when stdin is a TTY; otherwise it returns the
//! CLI profile's answer from `src/command/headless.rs` — which, alone among
//! the three headless profiles, declines rather than injecting a human user's
//! host credentials nobody consented to.

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::error::CommandError;
use crate::data::session::AgentName;

use crate::frontend::cli::command_frontend::CliFrontend;

impl AgentAuthFrontend for CliFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.agent_auth_consent());
        }
        let vars = if env_var_names.is_empty() {
            "no environment variables".to_string()
        } else {
            env_var_names.join(", ")
        };
        eprintln!(
            "awman: Inject host credentials ({vars}) into the {} container? [y]es / [n]o / [o]nce",
            agent.as_str()
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(AgentAuthDecision::DeclineOnce);
        }
        Ok(match buf.trim() {
            "y" | "Y" => AgentAuthDecision::Accept,
            "n" | "N" => AgentAuthDecision::Decline,
            _ => AgentAuthDecision::DeclineOnce,
        })
    }
}
