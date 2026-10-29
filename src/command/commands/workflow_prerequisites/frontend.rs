//! `WorkflowPrerequisiteFrontend` — the questions the workflow prerequisite
//! flow asks, and the answers a frontend maps a keypress back to.
//!
//! Every prompt is built in `src/command/prompts.rs`; a frontend renders the
//! `Prompt<D>` / `TextPrompt` it is handed and returns the chosen value. The
//! defaults refuse, so a frontend with nobody at it (the API, the squad
//! daemon, a `--non-interactive` CLI) never grants a credential: the flow
//! only calls an `ask_*` method when [`can_ask_prerequisites`] is `true`.
//!
//! [`can_ask_prerequisites`]: WorkflowPrerequisiteFrontend::can_ask_prerequisites

use std::path::PathBuf;

use crate::command::commands::agent_trust::TrustDecision;
use crate::command::error::CommandError;
use crate::data::config::workflows::UnresolvedMode;
use crate::data::message::UserMessageSink;
use crate::data::prompt::{Prompt, TextPrompt};
use crate::data::workflow_prerequisites::WorkflowPrerequisiteReport;
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::host_secret_command::{
    HostSecretCommandError, HostSecretCommandReview, HostSecretOutput,
};

/// Answer to the git identity question for `commit_changes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityChoice {
    /// Inject the host's global identity into `commit_changes` containers.
    UseHost { name: String, email: String },
    /// Type a name and an email.
    Enter,
    /// Write the host identity into this repo's `.git/config` (persistent).
    SetLocalFromHost { name: String, email: String },
    /// Proceed without; `commit_changes` will fail.
    Continue,
}

/// Confirmation for writing an identity into `.git/config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityPersist {
    /// Do not write the file; inject the identity for this run only.
    RunOnly,
    /// Run the shown `git config --local` command on the host.
    WriteLocal,
}

/// Answer to the SSH key question for `push_branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshChoice {
    /// Mount this one key read-only; `remember` stores the decision for the repo.
    Key { path: PathBuf, remember: bool },
    /// Mount the whole `~/.ssh` read-only (today's `ssh()` overlay).
    WholeDir,
    /// Do not use SSH keys; `push_branch` will fail.
    Decline,
}

/// Answer to "use the token found in `GH_TOKEN`?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenConsent {
    ThisRun,
    Remember,
    Decline,
}

/// Where a GitHub token should come from when none is usable yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSourceChoice {
    Paste,
    GhAuthToken,
    GhAuthLogin,
    Acquire,
    Decline,
}

/// Answer for a tool missing from the phase image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Continue,
    Abort,
}

/// Answer for a gap no grant can fix (missing remote, detached HEAD, a pull
/// request with no push before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapChoice {
    Continue,
    Abort,
}

/// What follows the prerequisite report, so a frontend can say so beside it
/// and offer a Continue/Abort choice only when one is really being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrerequisiteReportNext {
    /// `--check-prerequisites`: the report is the whole result; nothing runs.
    CheckOnly,
    /// A person is there: each item that needs attention is asked about
    /// next, one at a time, then `unresolved` decides what an item left
    /// unresolved does to the run.
    Questions { unresolved: UnresolvedMode },
    /// Nobody to ask: the headless answers have already been applied.
    Decided,
}

fn unavailable(prompt: &str) -> CommandError {
    CommandError::InteractiveInputUnavailable {
        prompt: prompt.to_string(),
    }
}

/// The workflow prerequisite questions. A supertrait of
/// `ExecWorkflowCommandFrontend` and `SquadCommandFrontend`.
///
/// Every frontend renders the report; only a frontend with a person at it
/// overrides [`can_ask_prerequisites`](Self::can_ask_prerequisites) and the
/// `ask_*` methods. The frontend never decides what is asked, what the
/// choices are, or what runs: the command layer builds the prompts and hands
/// [`with_terminal_released`](Self::with_terminal_released) the closure to run.
pub trait WorkflowPrerequisiteFrontend: UserMessageSink {
    /// Show the prerequisite report (CLI table, TUI modal, API event, squad
    /// log). Called once: before any question when a person is there,
    /// otherwise after the headless answers were applied. A dismissal that
    /// means "stop" (Ctrl-C on the TUI summary) returns
    /// `Err(CommandError::Aborted)`.
    ///
    /// `next` says what happens after it (see
    /// [`prompts::prerequisite_report`](crate::command::prompts::prerequisite_report)).
    ///
    /// The default writes the report's one text rendering
    /// ([`WorkflowPrerequisiteReport::render_lines`]) as info messages.
    fn show_workflow_prerequisites(
        &mut self,
        report: &WorkflowPrerequisiteReport,
        _next: PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        for line in report.render_lines() {
            self.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: line,
            });
        }
        Ok(())
    }

    /// The report of the non-interactive re-check run just before teardown.
    /// The flow has already written one warning when it found gaps; the API
    /// emits it as a `before_teardown` event. Default: nothing more to show.
    fn report_workflow_prerequisites_recheck(&mut self, _report: &WorkflowPrerequisiteReport) {}

    /// `true` only for a frontend with a person at it: the CLI on a TTY
    /// without `--non-interactive`/`--json`, and the TUI.
    fn can_ask_prerequisites(&self) -> bool {
        false
    }

    fn ask_prerequisite_identity(
        &mut self,
        prompt: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        Err(unavailable(&prompt.title))
    }

    /// The name or email after "Enter a name and email". `None` on dismiss.
    fn ask_prerequisite_identity_text(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<String>, CommandError> {
        Err(unavailable(&prompt.title))
    }

    fn ask_prerequisite_identity_persist(
        &mut self,
        prompt: &Prompt<IdentityPersist>,
    ) -> Result<IdentityPersist, CommandError> {
        Err(unavailable(&prompt.title))
    }

    fn ask_prerequisite_ssh(
        &mut self,
        prompt: &Prompt<SshChoice>,
    ) -> Result<SshChoice, CommandError> {
        Err(unavailable(&prompt.title))
    }

    fn ask_prerequisite_token_consent(
        &mut self,
        prompt: &Prompt<TokenConsent>,
    ) -> Result<TokenConsent, CommandError> {
        Err(unavailable(&prompt.title))
    }

    fn ask_prerequisite_token_source(
        &mut self,
        prompt: &Prompt<TokenSourceChoice>,
    ) -> Result<TokenSourceChoice, CommandError> {
        Err(unavailable(&prompt.title))
    }

    /// A masked text prompt (`prompt.masked == true`) for a pasted token.
    /// Returns `None` on an empty answer or a dismissal. The frontend must not
    /// echo, log, keep in history or otherwise retain the text, and must wrap
    /// it in a `SecretString` immediately.
    fn ask_prerequisite_secret(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<SecretString>, CommandError> {
        Err(unavailable(&prompt.title))
    }

    fn ask_prerequisite_tool(
        &mut self,
        prompt: &Prompt<ToolChoice>,
    ) -> Result<ToolChoice, CommandError> {
        Err(unavailable(&prompt.title))
    }

    fn ask_prerequisite_gap(
        &mut self,
        prompt: &Prompt<GapChoice>,
    ) -> Result<GapChoice, CommandError> {
        Err(unavailable(&prompt.title))
    }

    /// Approve a repository-defined token command for its exact content hash.
    /// The default declines, like `AgentTrustFrontend`: API, squad and a
    /// non-interactive CLI never approve one, and `--yes` plays no part.
    fn confirm_prerequisite_host_command(
        &mut self,
        _review: &HostSecretCommandReview,
        _prompt: &Prompt<TrustDecision>,
    ) -> Result<TrustDecision, CommandError> {
        Ok(TrustDecision::Decline)
    }

    /// Run `f` with the terminal handed to the host command it spawns. The TUI
    /// leaves the alternate screen first and restores it afterwards; the CLI
    /// just runs `f`. The frontend never decides what runs.
    fn with_terminal_released(
        &mut self,
        _title: &str,
        f: &mut dyn FnMut() -> Result<HostSecretOutput, HostSecretCommandError>,
    ) -> Result<HostSecretOutput, HostSecretCommandError> {
        f()
    }
}
