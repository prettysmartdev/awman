//! `WorkflowPrerequisiteFrontend` impl for the TUI.
//!
//! The report is a scrollable modal; every question is the command layer's
//! `Prompt` drawn as a keyed dialog (title, body, one line per choice); a
//! pasted token goes through `DialogRequest::SecretInput`, which renders `•`
//! and is wrapped in a `SecretString` the moment it is answered. Nothing here
//! decides what is asked, what the choices mean or what runs.
//!
//! Questions stay visible during yolo countdowns; the event loop dismisses
//! them when the command countdown expires.

use crate::command::commands::agent_trust::TrustDecision;
use crate::command::commands::workflow_prerequisites::{
    GapChoice, IdentityChoice, IdentityPersist, SshChoice, TokenConsent, TokenSourceChoice,
    ToolChoice, WorkflowPrerequisiteFrontend,
};
use crate::command::error::CommandError;
use crate::data::prompt::{Prompt, TextPrompt};
use crate::data::workflow_prerequisites::WorkflowPrerequisiteReport;
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::host_secret_command::{
    HostSecretCommandError, HostSecretCommandReview, HostSecretOutput,
};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl TuiCommandFrontend {
    /// Render the command's choices and return its input transport response.
    fn ask_prerequisite_prompt<D: Clone>(&self, prompt: &Prompt<D>) -> Result<D, CommandError> {
        self.pick_from_keyed_prompt(prompt)
    }
}

impl WorkflowPrerequisiteFrontend for TuiCommandFrontend {
    fn show_workflow_prerequisites(
        &mut self,
        report: &WorkflowPrerequisiteReport,
    ) -> Result<(), CommandError> {
        let prompt = crate::command::prompts::prerequisite_report(report);
        let response = self.ask_dialog(DialogRequest::Review {
            title: prompt.title.clone(),
            body: prompt.body.clone(),
            keys: prompt
                .choices
                .iter()
                .map(|c| (c.key, c.label.clone()))
                .collect(),
            gated_keys: Vec::new(),
        })?;
        match response {
            DialogResponse::Interrupted => Err(CommandError::Aborted),
            DialogResponse::Char(key) if prompt.answer_for_key(key) == Some(false) => {
                Err(CommandError::Aborted)
            }
            _ => Ok(()),
        }
    }

    fn can_ask_prerequisites(&self) -> bool {
        true
    }

    fn ask_prerequisite_identity(
        &mut self,
        prompt: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn ask_prerequisite_identity_text(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: prompt.title.clone(),
            prompt: prompt.body.clone(),
            default_text: prompt.default.clone(),
        })?;
        Ok(match response {
            DialogResponse::Text(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            DialogResponse::Text(_) => prompt.default.clone(),
            _ => None,
        })
    }

    fn ask_prerequisite_identity_persist(
        &mut self,
        prompt: &Prompt<IdentityPersist>,
    ) -> Result<IdentityPersist, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn ask_prerequisite_ssh(
        &mut self,
        prompt: &Prompt<SshChoice>,
    ) -> Result<SshChoice, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn ask_prerequisite_token_consent(
        &mut self,
        prompt: &Prompt<TokenConsent>,
    ) -> Result<TokenConsent, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn ask_prerequisite_token_source(
        &mut self,
        prompt: &Prompt<TokenSourceChoice>,
    ) -> Result<TokenSourceChoice, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn ask_prerequisite_secret(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<SecretString>, CommandError> {
        let response = self.ask_dialog(DialogRequest::SecretInput {
            title: prompt.title.clone(),
            prompt: prompt.body.clone(),
        })?;
        Ok(match response {
            DialogResponse::Secret(secret) if !secret.expose().is_empty() => Some(secret),
            _ => None,
        })
    }

    fn ask_prerequisite_tool(
        &mut self,
        prompt: &Prompt<ToolChoice>,
    ) -> Result<ToolChoice, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn ask_prerequisite_gap(
        &mut self,
        prompt: &Prompt<GapChoice>,
    ) -> Result<GapChoice, CommandError> {
        self.ask_prerequisite_prompt(prompt)
    }

    fn confirm_prerequisite_host_command(
        &mut self,
        _review: &HostSecretCommandReview,
        prompt: &Prompt<TrustDecision>,
    ) -> Result<TrustDecision, CommandError> {
        // Approval is accepted only once the whole command has been on screen.
        let gated_keys = prompt
            .choices
            .iter()
            .filter(|choice| choice.value == TrustDecision::Approve)
            .map(|choice| choice.key)
            .collect();
        let response = self.ask_dialog(DialogRequest::Review {
            title: prompt.title.clone(),
            body: prompt.body.clone(),
            keys: prompt
                .choices
                .iter()
                .map(|choice| (choice.key, choice.label.clone()))
                .collect(),
            gated_keys,
        })?;
        let answer = match response {
            DialogResponse::Char(key) => prompt.answer_for_key(key),
            _ => None,
        };
        answer
            .or(prompt.default_on_dismiss)
            .ok_or(CommandError::Aborted)
    }

    /// Leave the alternate screen and raw mode, run `f`, then restore both
    /// (the event loop does the terminal work and answers each request). The
    /// screen is restored even when `f` fails.
    fn with_terminal_released(
        &mut self,
        title: &str,
        f: &mut dyn FnMut() -> Result<HostSecretOutput, HostSecretCommandError>,
    ) -> Result<HostSecretOutput, HostSecretCommandError> {
        if self
            .ask_dialog(DialogRequest::TerminalRelease {
                title: title.to_string(),
            })
            .is_err()
        {
            return Err(HostSecretCommandError::Spawn {
                program: title.into(),
                reason: "terminal release failed".into(),
            });
        }
        struct Restore<'a>(&'a TuiCommandFrontend);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = self.0.ask_dialog(DialogRequest::TerminalRestore);
            }
        }
        let _restore = Restore(self);
        f()
    }
}
