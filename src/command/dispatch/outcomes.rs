//! [`CommandOutcome`] — what `Dispatch::run_command` returns, and the exit code
//! every frontend derives from it.

/// Catch-all outcome enum returned by `Dispatch::run_command`. Layer 3
/// inspects the variant to choose an appropriate rendering.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum CommandOutcome {
    Init(crate::command::commands::init::InitOutcome),
    Ready(crate::command::commands::ready::ReadyOutcome),
    Chat(crate::command::commands::chat::ChatOutcome),
    Agents(crate::command::commands::agents::AgentsOutcome),
    Config(crate::command::commands::config::ConfigOutcome),
    ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome),
    ExecWorkflow(crate::command::commands::exec_workflow::ExecWorkflowOutcome),
    ApiServer(crate::command::commands::api_server::ApiServerOutcome),
    Squad(crate::command::commands::squad::commands::SquadOutcome),
    SquadAttach(crate::command::commands::squad::attach::SquadAttachOutcome),
    Remote(crate::command::commands::remote::RemoteOutcome),
    New(crate::command::commands::new::NewOutcome),
    Specs(crate::command::commands::specs::SpecsOutcome),
    Clean(crate::command::commands::clean::CleanOutcome),
    /// Trivial wrapper used by no-op leaf commands during the refactor.
    Empty,
}

impl CommandOutcome {
    /// The command's process/API exit code. Successful aggregate outcomes can
    /// still carry a failure: `new skill --pull-all` continues collecting
    /// libraries after one source fails, and must be visible to both CLI and
    /// API clients as a non-zero result.
    ///
    /// Every agent-session command reports the agent's own exit code. `Chat`
    /// was missing from this list until WI 0114 F-22: the TUI read
    /// `ChatOutcome::exit_code` itself while the CLI and API came through here
    /// and saw 0, so the same failing `awman chat` exited 0 from a script and
    /// showed red in the TUI.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Chat(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecWorkflow(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecPrompt(outcome) => outcome.exit_code.unwrap_or(0),
            Self::SquadAttach(outcome) => outcome.exit_code,
            Self::Agents(outcome) => outcome.exit_code(),
            // `remote exec --follow` reports the remote command's result.
            Self::Remote(outcome) => outcome.exit_code(),
            // `ReadyOutcome::ready` is `ReadySummary::is_ready()` (WI 0125):
            // a failed required build/import stage must never exit 0, for
            // the CLI's process exit code and the API's command status alike.
            Self::Ready(outcome) if !outcome.ready => 1,
            _ if self.is_partial_failure() => 1,
            _ => 0,
        }
    }

    pub fn is_partial_failure(&self) -> bool {
        matches!(self, Self::New(crate::command::commands::new::NewOutcome::Skill(skill))
            if skill.libraries.iter().any(|library| library.error.is_some()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_outcome_exit_code_covers_execution_and_partial_skill_failure() {
        assert_eq!(
            CommandOutcome::ExecWorkflow(
                crate::command::commands::exec_workflow::ExecWorkflowOutcome {
                    prerequisites: None,
                    workflow: "workflow.toml".into(),
                    exit_code: Some(17),
                    worktree_used: false,
                }
            )
            .exit_code(),
            17
        );
        assert_eq!(
            CommandOutcome::ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome {
                agent: None,
                exit_code: Some(3)
            })
            .exit_code(),
            3
        );
        let partial = CommandOutcome::New(crate::command::commands::new::NewOutcome::Skill(
            crate::command::commands::new::NewSkillOutcome {
                interview: false,
                global: false,
                path: None,
                pull: true,
                libraries: vec![crate::command::commands::new::PullLibraryOutcome {
                    slug: "broken".into(),
                    dir: "broken".into(),
                    updated: false,
                    skills_found: Vec::new(),
                    error: Some("unreachable".into()),
                }],
            },
        ));
        assert!(partial.is_partial_failure());
        assert_eq!(partial.exit_code(), 1);
        assert_eq!(CommandOutcome::Empty.exit_code(), 0);
    }

    /// WI 0125 (M-7): a `Ready` outcome's exit code is driven by
    /// `ReadyOutcome::ready` (set from `ReadySummary::is_ready()` in
    /// `From<ReadySummary>`), not by whether the engine returned `Ok` —
    /// `run_to_completion` can return `Ok(summary)` for a summary that
    /// recorded a failed build/import as a row rather than an `Err`. Before
    /// this change every `Ready` outcome exited 0 regardless of
    /// `StepStatus::Failed` anywhere in it.
    #[test]
    fn command_outcome_exit_code_reflects_ready_image_failures() {
        fn ready_outcome(ready: bool) -> CommandOutcome {
            use crate::command::commands::ready::ReadyOutcome;
            use crate::data::step_status::StepStatus;
            CommandOutcome::Ready(ReadyOutcome {
                runtime: "docker".into(),
                dockerfile: StepStatus::Done,
                base_image: StepStatus::Done,
                agent_image: StepStatus::Done,
                image_source: None,
                agent_definition: None,
                local_agent: StepStatus::Done,
                audit: StepStatus::Skipped,
                image_rebuild: StepStatus::Skipped,
                agent_credentials: Vec::new(),
                non_default_agent_images: Vec::new(),
                image_results: Vec::new(),
                base_image_results: Vec::new(),
                ready,
                json_requested: false,
                refresh_requested: false,
            })
        }

        assert_eq!(ready_outcome(true).exit_code(), 0);
        assert_eq!(
            ready_outcome(false).exit_code(),
            1,
            "a failed required image stage must exit non-zero on every runtime, \
             not only builtin"
        );
    }
}
