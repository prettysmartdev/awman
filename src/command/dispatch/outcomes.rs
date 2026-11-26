//! [`CommandOutcome`] — what `Dispatch::run_command` returns, and the exit code
//! every frontend derives from it.

/// Catch-all outcome enum returned by `Dispatch::run_command`. Layer 3
/// inspects the variant to choose an appropriate rendering.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum CommandOutcome {
    /// A completed command whose typed result lives in another engine.
    /// Presenters retain its exit code even when they render from feed deltas.
    Relayed(crate::data::frontend_protocol::CommandResultWire),
    Attach(crate::command::commands::attach::AttachOutcome),
    Init(crate::command::commands::init::InitOutcome),
    Ready(crate::command::commands::ready::ReadyOutcome),
    Chat(crate::command::commands::chat::ChatOutcome),
    Agents(crate::command::commands::agents::AgentsOutcome),
    Config(crate::command::commands::config::ConfigOutcome),
    WorkerLink(crate::command::commands::worker::WorkerLinkOutcome),
    Queue(crate::data::wire::List<crate::command::daemon_api::worker_pull::QueueRow>),
    QueuePick(crate::data::fs::api_db::CommandResult),
    ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome),
    ExecWorkflow(crate::command::commands::exec_workflow::ExecWorkflowOutcome),
    DaemonApi(crate::command::commands::daemon::api::DaemonApiOutcome),
    Daemon(crate::command::commands::daemon::lifecycle::DaemonOutcome),
    Squad(crate::command::commands::squad::commands::SquadOutcome),
    Remote(crate::command::commands::remote::RemoteOutcome),
    New(crate::command::commands::new::NewOutcome),
    Edit(crate::command::commands::edit::EditOutcome),
    Clean(crate::command::commands::clean::CleanOutcome),
    /// Trivial wrapper used by no-op leaf commands during the refactor.
    Link(String),
    Empty,
}

impl CommandOutcome {
    /// The same final assurance travels through CLI, relay and command storage.
    pub fn automation(&self) -> Option<&crate::data::automation_records::CommandAutomationResult> {
        match self {
            Self::Chat(outcome) => outcome.automation.as_ref(),
            Self::ExecPrompt(outcome) => outcome.automation.as_ref(),
            Self::Relayed(result) => result.automation.as_ref(),
            Self::QueuePick(result) => result.automation.as_ref(),
            _ => None,
        }
    }
    /// An interactive automation run's last confirmed answer (WI 0070).
    pub fn automation_final_message(&self) -> Option<&str> {
        match self {
            Self::Chat(outcome) => outcome.automation_final_message.as_deref(),
            Self::ExecPrompt(outcome) => outcome.automation_final_message.as_deref(),
            _ => None,
        }
    }
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
            Self::Relayed(result) => result.exit_code,
            Self::Attach(outcome) => outcome.exit_code,
            Self::Chat(outcome) => outcome.exit_code.unwrap_or(1),
            Self::ExecWorkflow(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecPrompt(outcome) => outcome.exit_code.unwrap_or(1),
            Self::QueuePick(outcome) => outcome.exit_code.unwrap_or(1),
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

/// A failed runtime wait is never an absent, successful exit. Cancellation is
/// synthetic only when no provider failure is already known.
pub(crate) fn agent_exit_code(
    exit: Result<crate::engine::agent_runtime::AgentExitInfo, crate::engine::error::EngineError>,
    cancelled: bool,
) -> Result<Option<i32>, crate::command::error::CommandError> {
    match exit {
        Err(error) => Err(crate::command::error::CommandError::RuntimeWaitFailed(
            error.to_string(),
        )),
        Ok(info) if info.exit_code != 0 => Ok(Some(info.exit_code)),
        Ok(_) if cancelled => Ok(Some(crate::data::command_status::CANCELLED_EXIT_CODE)),
        Ok(info) => Ok(Some(info.exit_code)),
    }
}

#[cfg(test)]
mod wait_failure_tests {
    use super::*;
    use crate::engine::agent_runtime::execution::{CancelHandle, ExecutionBackend};
    use crate::engine::agent_runtime::{AgentExecution, AgentExitInfo};
    use crate::engine::error::EngineError;
    use std::sync::Arc;

    struct FailedWait;
    impl ExecutionBackend for FailedWait {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            Err(EngineError::Container(
                "synthetic runtime wait failure".into(),
            ))
        }
        fn cancel(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn cancel_handle(&self) -> CancelHandle {
            CancelHandle::new(|| Ok(()))
        }
    }
    fn info(code: i32) -> AgentExitInfo {
        let now = chrono::DateTime::from_timestamp(0, 0).unwrap();
        AgentExitInfo {
            exit_code: code,
            signal: None,
            started_at: now,
            ended_at: now,
        }
    }

    #[test]
    fn cancellation_does_not_hide_runtime_wait_failure() {
        let result = agent_exit_code(
            Err(EngineError::Container("wait failed during cancel".into())),
            true,
        );
        assert!(matches!(
            result,
            Err(crate::command::error::CommandError::RuntimeWaitFailed(message))
                if message.contains("wait failed during cancel")
        ));
    }

    #[test]
    fn missing_chat_or_exec_exit_code_is_a_failure_projection() {
        let chat = CommandOutcome::Chat(crate::command::commands::chat::ChatOutcome {
            automation: None,
            automation_final_message: None,
            agent: None,
            exit_code: None,
        });
        let exec =
            CommandOutcome::ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome {
                automation: None,
                automation_final_message: None,
                agent: None,
                exit_code: None,
            });
        assert_eq!(chat.exit_code(), 1);
        assert_eq!(exec.exit_code(), 1);
    }

    #[tokio::test]
    async fn failed_runtime_wait_is_a_failed_command_result() {
        let now = info(0).started_at;
        let (sender, _) = tokio::sync::broadcast::channel(16);
        let mut execution = AgentExecution::new(
            crate::data::session::AgentHandle {
                id: "fake".into(),
                name: "fake".into(),
                image_tag: "fake:latest".into(),
                started_at: now,
            },
            Box::new(FailedWait),
            Arc::new(sender),
            None,
        );
        let error = agent_exit_code(execution.wait().await, false).unwrap_err();
        assert_eq!(
            (error.exit_code(), error.http_status(), error.code()),
            (1, 500, crate::data::agent_failure::RUNTIME_WAIT_FAILED)
        );
        assert!(error.to_string().contains("synthetic runtime wait failure"));
    }

    #[test]
    fn known_provider_failure_survives_cancellation_and_outcome_projection() {
        for cancelled in [true, false] {
            for code in [1, 17, 42, 130, 255] {
                let exit_code = agent_exit_code(Ok(info(code)), cancelled).unwrap();
                for outcome in [
                    CommandOutcome::Chat(crate::command::commands::chat::ChatOutcome {
                        automation: None,
                        automation_final_message: None,
                        agent: None,
                        exit_code,
                    }),
                    CommandOutcome::ExecPrompt(
                        crate::command::commands::exec_prompt::ExecPromptOutcome {
                            automation: None,
                            automation_final_message: None,
                            agent: None,
                            exit_code,
                        },
                    ),
                ] {
                    assert_eq!(outcome.exit_code(), code);
                }
            }
        }
        assert_eq!(agent_exit_code(Ok(info(0)), true).unwrap(), Some(130));
        assert!(matches!(
            agent_exit_code(Err(EngineError::Other("cancelled wait".into())), true),
            Err(crate::command::error::CommandError::RuntimeWaitFailed(_))
        ));
        assert_eq!(agent_exit_code(Ok(info(0)), false).unwrap(), Some(0));
    }
}
