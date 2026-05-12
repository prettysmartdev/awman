//! `AgentAuthFrontend` impl for the CLI.
//!
//! The safe non-interactive default is `DeclineOnce`
//! (do NOT auto-persist consent). The CLI prompts on stdin only when stdin
//! is a TTY; otherwise it falls back to the safe default.

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::error::CommandError;
use crate::data::session::AgentName;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

impl AgentAuthFrontend for CliFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        if !stdin_is_tty() {
            return Ok(AgentAuthDecision::DeclineOnce);
        }
        let vars = if env_var_names.is_empty() {
            "no environment variables".to_string()
        } else {
            env_var_names.join(", ")
        };
        eprintln!(
            "amux: Inject host credentials ({vars}) into the {} container? [y]es / [n]o / [o]nce",
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
