//! `AgentAuthFrontend` impl for the TUI.

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{AgentAuthState, DialogRequest, DialogResponse};
use crate::data::session::AgentName;

impl AgentAuthFrontend for InstanceCommandFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.agent_auth_consent());
        }
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
