use awman::command::commands::{
    chat::ChatOutcome, exec_prompt::ExecPromptOutcome, exec_workflow::ExecWorkflowOutcome,
};
use awman::command::dispatch::CommandOutcome;
use awman::command::error::CommandError;

#[test]
fn failures_keep_their_exit_http_and_machine_code_classification() {
    let cases = [
        (
            CommandError::unknown_command(&["missing"]),
            2,
            400,
            "unknown_subcommand",
        ),
        (
            CommandError::unknown_flag(&["chat"], "invalid"),
            2,
            400,
            "unknown_flag",
        ),
        (
            CommandError::missing_required_argument(&["config", "get"], "key"),
            2,
            400,
            "missing_required_argument",
        ),
        (CommandError::Aborted, 130, 500, "aborted"),
        (
            CommandError::WorkItemNotFound { number: 144 },
            4,
            404,
            "work_item_not_found",
        ),
        (
            CommandError::SessionWorkdirRequired,
            2,
            400,
            "session_workdir_required",
        ),
        (
            CommandError::SessionRepoUrlEmpty,
            2,
            400,
            "session_repo_url_empty",
        ),
        (CommandError::DaemonNotRunning, 1, 500, "daemon_not_running"),
        (CommandError::RemoteTimeout, 1, 500, "remote_timeout"),
        (
            CommandError::Other("storage failed".into()),
            1,
            500,
            "internal",
        ),
    ];
    for (error, exit, http, code) in cases {
        assert_eq!(
            (error.exit_code(), error.http_status(), error.code()),
            (exit, http, code),
            "{error:?}"
        );
        assert!(!error.to_string().is_empty());
    }
}

/// Guards: #144 all execution presenters must retain the agent's exit code.
#[test]
fn execution_outcomes_preserve_nonzero_and_absent_agent_exit_codes() {
    for exit in [None, Some(0), Some(1), Some(17), Some(130), Some(255)] {
        for outcome in [
            CommandOutcome::Chat(ChatOutcome {
                agent: None,
                exit_code: exit,
            }),
            CommandOutcome::ExecPrompt(ExecPromptOutcome {
                agent: None,
                exit_code: exit,
            }),
            CommandOutcome::ExecWorkflow(ExecWorkflowOutcome {
                prerequisites: None,
                workflow: "flow".into(),
                exit_code: exit,
                worktree_used: false,
            }),
        ] {
            assert_eq!(outcome.exit_code(), exit.unwrap_or(0));
            let wire = serde_json::to_value(outcome).unwrap();
            assert_eq!(wire["payload"]["exit_code"], serde_json::json!(exit));
            assert!(wire["kind"].is_string());
        }
    }
}
