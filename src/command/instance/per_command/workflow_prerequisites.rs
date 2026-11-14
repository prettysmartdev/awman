//! `WorkflowPrerequisiteFrontend` impl for the TUI.
//!
//! The report and every question are scrollable review dialogs: the body
//! (the report's checks, or a question's explanation) scrolls under a footer
//! that always shows the choices, so nothing is pushed off screen however
//! long the text or small the terminal. The report pins the command layer's
//! "what happens next" above its choices. A pasted token goes through
//! `DialogRequest::SecretInput`, which renders `•` and is wrapped in a
//! `SecretString` the moment it is answered. Nothing here decides what is
//! asked, what the choices mean or what runs.
//!
//! Questions stay visible during yolo countdowns; the event loop dismisses
//! them when the command countdown expires.

use crate::command::commands::agent_trust::TrustDecision;
use crate::command::commands::workflow_prerequisites::{
    GapChoice, IdentityChoice, IdentityPersist, PrerequisiteReportNext, SshChoice, TokenConsent,
    TokenSourceChoice, ToolChoice, WorkflowPrerequisiteFrontend,
};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse, ReviewBody, ReviewRequest};
use crate::data::prompt::{Prompt, TextPrompt};
use crate::data::workflow_prerequisites::WorkflowPrerequisiteReport;
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::host_secret_command::{
    HostSecretCommandError, HostSecretCommandReview, HostSecretOutput,
};

impl InstanceCommandFrontend {
    /// Ask one of the command's questions; Esc and Ctrl-C answer its
    /// `default_on_dismiss`.
    fn ask_prerequisite_prompt<D: Clone + PartialEq>(
        &self,
        prompt: &Prompt<D>,
    ) -> Result<D, CommandError> {
        let state = ReviewRequest::question(prompt, ReviewBody::Prose(prompt.body.clone()));
        let answer = match self.ask_dialog(DialogRequest::Review(state))? {
            DialogResponse::Char(key) => prompt.answer_for_key(key),
            _ => None,
        };
        answer
            .or_else(|| prompt.default_on_dismiss.clone())
            .ok_or(CommandError::Aborted)
    }
}

impl WorkflowPrerequisiteFrontend for InstanceCommandFrontend {
    /// One entry per check, with what continuing leads to pinned above the
    /// choices. A check-only report offers only Close.
    fn show_workflow_prerequisites(
        &mut self,
        report: &WorkflowPrerequisiteReport,
        next: PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        let prompt = crate::command::prompts::prerequisite_report(report, next);
        let mut state = ReviewRequest::question(&prompt, ReviewBody::Prerequisites(report.clone()))
            .with_note(prompt.body.clone());
        if matches!(next, PrerequisiteReportNext::Questions { .. }) {
            state.dismiss_label = Some("Cancel".into());
        }
        match self.ask_dialog(DialogRequest::Review(state))? {
            DialogResponse::Interrupted => Err(CommandError::Aborted),
            DialogResponse::Dismissed
                if matches!(next, PrerequisiteReportNext::Questions { .. }) =>
            {
                Err(CommandError::Aborted)
            }
            DialogResponse::Char(key) if prompt.answer_for_key(key) == Some(false) => {
                Err(CommandError::Aborted)
            }
            _ => Ok(()),
        }
    }

    fn can_ask_prerequisites(&self) -> bool {
        !self.is_headless_cli()
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

    /// Leave the alternate screen and raw mode, run `f`, then restore both
    /// (the event loop does the terminal work and answers each request). The
    /// screen is restored even when `f` fails.
    fn with_terminal_released(
        &mut self,
        title: &str,
        f: &mut dyn FnMut() -> Result<HostSecretOutput, HostSecretCommandError>,
    ) -> Result<HostSecretOutput, HostSecretCommandError> {
        // An engine-hosted command has no TTY of its own: the host process
        // runs in a daemon PTY with the instance env, relayed to the
        // frontend as a hub terminal (WI 0142 Part A, edge cases).
        #[cfg(unix)]
        if let Some(terminals) = self.host_terminals.clone() {
            return self.with_host_pty(title, &terminals, f);
        }
        if self
            .ask_dialog(DialogRequest::TerminalRelease {
                terminal_id: None,
                title: title.to_string(),
            })
            .is_err()
        {
            return Err(HostSecretCommandError::Spawn {
                program: title.into(),
                reason: "terminal release failed".into(),
            });
        }
        struct Restore<'a>(&'a InstanceCommandFrontend);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = self.0.ask_dialog(DialogRequest::TerminalRestore);
            }
        }
        let _restore = Restore(self);
        f()
    }
}

#[cfg(unix)]
impl InstanceCommandFrontend {
    /// The hosted `TerminalRelease`: open a daemon PTY at the tab's size,
    /// register it with the hub, tell the frontends which terminal to show,
    /// run `f` with the PTY as this thread's host terminal, then restore.
    fn with_host_pty(
        &mut self,
        title: &str,
        terminals: &crate::command::instance::frontend::HostTerminals,
        f: &mut dyn FnMut() -> Result<HostSecretOutput, HostSecretCommandError>,
    ) -> Result<HostSecretOutput, HostSecretCommandError> {
        use crate::engine::terminal_hub::{bridge::HostPty, TerminalRegistration};
        let failed = |reason: String| HostSecretCommandError::Spawn {
            program: title.into(),
            reason,
        };
        let (cols, rows) = self.pty_size.get();
        let pty = HostPty::open(cols, rows).map_err(|e| failed(e.to_string()))?;
        let handle = pty
            .bridge(
                &terminals.hub,
                TerminalRegistration {
                    owner: terminals.owner.clone(),
                    agent: title.to_string(),
                    model: None,
                    cols,
                    rows,
                },
            )
            .map_err(|e| failed(e.to_string()))?;
        if self
            .ask_dialog(DialogRequest::TerminalRelease {
                terminal_id: Some(handle.id().clone()),
                title: title.to_string(),
            })
            .is_err()
        {
            handle.exit(1);
            return Err(failed("terminal release failed".into()));
        }
        struct Restore<'a>(&'a InstanceCommandFrontend);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = self.0.ask_dialog(DialogRequest::TerminalRestore);
            }
        }
        let result = {
            let _restore = Restore(self);
            pty.scope(f)
        };
        handle.exit(if result.is_ok() { 0 } else { 1 });
        result
    }
}
