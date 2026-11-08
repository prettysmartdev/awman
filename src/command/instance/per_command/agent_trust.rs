//! `AgentTrustFrontend` impl for the TUI: the review summary in a scrollable
//! review dialog. The approve key is accepted only once the whole review has
//! been on screen; Esc is the prompt's `default_on_dismiss` (decline).

use crate::command::commands::agent_trust::{AgentTrustFrontend, AgentTrustReview, TrustDecision};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse, ReviewRequest};
use crate::data::prompt::Prompt;

impl AgentTrustFrontend for InstanceCommandFrontend {
    fn confirm_agent_definition(
        &mut self,
        review: &AgentTrustReview,
        prompt: &Prompt<TrustDecision>,
    ) -> Result<TrustDecision, CommandError> {
        if self.is_headless_cli() {
            return Err(crate::command::commands::agent_trust::untrusted_error(
                review,
            ));
        }
        let gated_keys = prompt
            .choices
            .iter()
            .filter(|choice| choice.value == TrustDecision::Approve)
            .map(|choice| choice.key)
            .collect();
        let response = self.ask_dialog(DialogRequest::Review(ReviewRequest::new(
            prompt.title.clone(),
            prompt.body.clone(),
            prompt
                .choices
                .iter()
                .map(|choice| (choice.key, choice.label.clone()))
                .collect(),
            gated_keys,
        )))?;
        let answer = match response {
            DialogResponse::Char(key) => prompt.answer_for_key(key),
            _ => None,
        };
        answer
            .or(prompt.default_on_dismiss)
            .ok_or(CommandError::Aborted)
    }
}
