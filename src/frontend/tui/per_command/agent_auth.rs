//! `AgentAuthFrontend` impl for the TUI.

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{AgentAuthState, DialogRequest, DialogResponse};

impl AgentAuthFrontend for TuiCommandFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        let response = self.ask_dialog(DialogRequest::AgentAuth(AgentAuthState {
            agent_name: agent.as_str().to_string(),
            env_vars: env_var_names.iter().map(|s| s.to_string()).collect(),
        }))?;
        Ok(match response {
            DialogResponse::Char('y') | DialogResponse::Yes => AgentAuthDecision::Accept,
            DialogResponse::Char('n') | DialogResponse::No => AgentAuthDecision::Decline,
            _ => AgentAuthDecision::DeclineOnce,
        })
    }
}
