//! `AgentTrustFrontend` impl for the CLI.
//!
//! An interactive run shows the review summary and reads `y`/`n`. A run that
//! cannot ask (`--non-interactive`, `--json`, or stdin that is not a TTY)
//! refuses with the "has not been approved" error rather than answering for
//! the user; `--yes` plays no part.

use crate::command::commands::agent_trust::{
    untrusted_error, AgentTrustFrontend, AgentTrustReview, TrustDecision,
};
use crate::command::error::CommandError;
use crate::data::prompt::Prompt;
use crate::frontend::cli::command_frontend::CliFrontend;

impl AgentTrustFrontend for CliFrontend {
    fn confirm_agent_definition(
        &mut self,
        review: &AgentTrustReview,
        prompt: &Prompt<TrustDecision>,
    ) -> Result<TrustDecision, CommandError> {
        if self.non_interactive || self.is_json_mode() {
            return Err(untrusted_error(review));
        }
        self.pick_from_prompt(prompt)
    }
}
