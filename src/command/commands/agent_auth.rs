//! `AgentAuthFrontend` — first-run keychain consent prompt.

use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;
use crate::data::session::AgentName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAuthDecision {
    Accept,
    Decline,
    DeclineOnce,
}

pub trait AgentAuthFrontend: UserMessageSink + Send + Sync {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError>;
}
