//! `WorkflowPrerequisiteFrontend` impl for the CLI.
//!
//! The report prints as the command layer's own text table. Questions are
//! asked only on a terminal (`can_ask_prerequisites`), through the same
//! `Prompt` rendering every other CLI question uses. A pasted token is read
//! with echo off and is never printed or logged. The terminal is not given
//! up for a host command: the CLI already owns it, so the closure just runs.

use std::io::IsTerminal as _;

use crate::command::commands::agent_trust::TrustDecision;
use crate::command::commands::workflow_prerequisites::{
    GapChoice, IdentityChoice, IdentityPersist, SshChoice, TokenConsent, TokenSourceChoice,
    ToolChoice, WorkflowPrerequisiteFrontend,
};
use crate::command::error::CommandError;
use crate::data::prompt::{Prompt, TextPrompt};
use crate::data::workflow_prerequisites::WorkflowPrerequisiteReport;
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::host_secret_command::HostSecretCommandReview;
use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::per_command::helpers;

/// Print a report's text rendering to stderr (the summary line is its last line).
pub(crate) fn print_report(report: &WorkflowPrerequisiteReport) {
    for line in report.render_lines() {
        eprintln!("{line}");
    }
}

impl WorkflowPrerequisiteFrontend for CliFrontend {
    /// `--json` prints nothing here: the outcome carries the report.
    fn show_workflow_prerequisites(
        &mut self,
        report: &WorkflowPrerequisiteReport,
    ) -> Result<(), CommandError> {
        if !self.is_json_mode() {
            print_report(report);
            if self.can_ask_prerequisites() {
                let prompt = crate::command::prompts::prerequisite_report(report);
                if !helpers::read_prerequisite_report(&prompt)? {
                    return Err(CommandError::Aborted);
                }
            }
        }
        Ok(())
    }

    fn can_ask_prerequisites(&self) -> bool {
        !self.non_interactive && !self.is_json_mode() && std::io::stdin().is_terminal()
    }

    fn ask_prerequisite_identity(
        &mut self,
        prompt: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn ask_prerequisite_identity_text(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<String>, CommandError> {
        if !self.can_ask_prerequisites() {
            return Ok(None);
        }
        eprintln!("awman: {}", prompt.title);
        let question = match &prompt.default {
            Some(default) => format!("{} [{default}]", prompt.body),
            None => prompt.body.clone(),
        };
        Ok(helpers::read_line(&question).and_then(|typed| {
            if typed.is_empty() {
                prompt.default.clone()
            } else {
                Some(typed)
            }
        }))
    }

    fn ask_prerequisite_identity_persist(
        &mut self,
        prompt: &Prompt<IdentityPersist>,
    ) -> Result<IdentityPersist, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn ask_prerequisite_ssh(
        &mut self,
        prompt: &Prompt<SshChoice>,
    ) -> Result<SshChoice, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn ask_prerequisite_token_consent(
        &mut self,
        prompt: &Prompt<TokenConsent>,
    ) -> Result<TokenConsent, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn ask_prerequisite_token_source(
        &mut self,
        prompt: &Prompt<TokenSourceChoice>,
    ) -> Result<TokenSourceChoice, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn ask_prerequisite_secret(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<SecretString>, CommandError> {
        if !self.can_ask_prerequisites() {
            return Err(CommandError::InteractiveInputUnavailable {
                prompt: prompt.title.clone(),
            });
        }
        eprintln!("awman: {}", prompt.title);
        Ok(helpers::read_secret_line(&prompt.body).unwrap_or(None))
    }

    fn ask_prerequisite_tool(
        &mut self,
        prompt: &Prompt<ToolChoice>,
    ) -> Result<ToolChoice, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn ask_prerequisite_gap(
        &mut self,
        prompt: &Prompt<GapChoice>,
    ) -> Result<GapChoice, CommandError> {
        self.pick_from_prompt(prompt)
    }

    /// Never approved without a person: `--yes`, `--json` and
    /// `--non-interactive` all decline.
    fn confirm_prerequisite_host_command(
        &mut self,
        _review: &HostSecretCommandReview,
        prompt: &Prompt<TrustDecision>,
    ) -> Result<TrustDecision, CommandError> {
        if !self.can_ask_prerequisites() {
            return Ok(TrustDecision::Decline);
        }
        self.pick_from_prompt(prompt)
    }
}

#[cfg(test)]
mod wi0149_tests {
    use super::*;
    use crate::data::workflow_prerequisites::*;
    #[test]
    fn wi0149_cli_prerequisite_defaults_are_the_command_prompts_defaults() {
        let matches = crate::command::dispatch::catalogue::CommandCatalogue::get()
            .build_clap_command()
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml", "--non-interactive"])
            .unwrap();
        let mut cli = CliFrontend::new(matches);
        let check = PrerequisiteCheck {
            prerequisite: WorkflowPrerequisite::GitIdentity,
            status: PrerequisiteStatus::Missing,
            explanation: "fixture".into(),
            needed_by: vec![],
            remediations: vec![],
        };
        assert!(!cli.can_ask_prerequisites());
        let p = crate::command::prompts::prerequisite_identity(&check);
        assert_eq!(
            cli.ask_prerequisite_identity(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
        let p = crate::command::prompts::prerequisite_identity_persist("git config --local");
        assert_eq!(
            cli.ask_prerequisite_identity_persist(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
        let p = crate::command::prompts::prerequisite_ssh(&check, "github.com:org/repo");
        assert_eq!(
            cli.ask_prerequisite_ssh(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
        let p = crate::command::prompts::prerequisite_token_consent(&check, "GH_TOKEN");
        assert_eq!(
            cli.ask_prerequisite_token_consent(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
        let p = crate::command::prompts::prerequisite_token_source(&check);
        assert_eq!(
            cli.ask_prerequisite_token_source(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
        let p = crate::command::prompts::prerequisite_missing_tool(&check);
        assert_eq!(
            cli.ask_prerequisite_tool(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
        let p = crate::command::prompts::prerequisite_gap(&check);
        assert_eq!(
            cli.ask_prerequisite_gap(&p).unwrap(),
            p.default_on_dismiss.unwrap()
        );
    }
}
