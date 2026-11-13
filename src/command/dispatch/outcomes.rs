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
    SquadAttach(crate::command::commands::squad::attach::SquadAttachOutcome),
    Remote(crate::command::commands::remote::RemoteOutcome),
    New(crate::command::commands::new::NewOutcome),
    Specs(crate::command::commands::specs::SpecsOutcome),
    Clean(crate::command::commands::clean::CleanOutcome),
    /// Trivial wrapper used by no-op leaf commands during the refactor.
    Link(String),
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
            Self::Relayed(result) => result.exit_code,
            Self::Attach(outcome) => outcome.exit_code,
            Self::Chat(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecWorkflow(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecPrompt(outcome) => outcome.exit_code.unwrap_or(0),
            Self::QueuePick(outcome) => outcome.exit_code.unwrap_or(1),
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
