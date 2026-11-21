//! WI 0157: the API dispatch path refuses a retired built-in `gemini` with the
//! migration note and HTTP 404 before any image build or container launch.
//! The CLI, builtin and daemon paths are covered by `parity::*::retired_gemini_*`.
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::{Dispatch, Engines};
use awman::command::error::CommandError;
use awman::data::session::{Session, SessionOpenOptions};
use awman::engine::error::EngineError;
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::fixtures::{RepoBuilder, WorkflowBuilder};
use std::sync::Arc;

const GEMINI_NOTE: &str =
    "'gemini' is no longer a built-in agent, and awman has not changed your selection.";

#[tokio::test(flavor = "multi_thread")]
async fn api_prompt_and_workflow_naming_gemini_return_404_with_the_note_before_launch() {
    let s = crate::support::sandbox(
        "command::retired_agents::api_prompt_and_workflow_naming_gemini_return_404_with_the_note_before_launch",
    );
    let repo = RepoBuilder::new(&s, "api-gemini")
        .file("README.md", "fixture")
        .init();
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    let host_gemini = s.install_fake(crate::support::fake_cli_exe(), "gemini");
    let workflow = WorkflowBuilder::new("retired")
        .step_with(awman_test_support::fixtures::WorkflowStep {
            name: "one".into(),
            prompt: "look".into(),
            agent: Some("gemini".into()),
            ..Default::default()
        })
        .write(repo.path(), "retired");
    let cases: [(&[&str], Vec<String>); 2] = [
        (
            &["exec", "prompt"],
            vec!["--agent".into(), "gemini".into(), "hello".into()],
        ),
        (
            &["exec", "workflow"],
            vec![workflow.display().to_string(), "--headless".into()],
        ),
    ];
    for (path, args) in cases {
        let session = Session::open_at_git_root(
            repo.path().into(),
            repo.path().into(),
            SessionOpenOptions {
                env: Some(crate::support::env_snapshot(&s)),
                ..Default::default()
            },
        )
        .unwrap();
        let bus = EventBus::new(64);
        let frontend = ApiDispatchFrontend::new(&path.join(" "), &args, bus.sender());
        let engines = Engines::for_env(crate::support::env_snapshot(&s)).unwrap();
        let error = Dispatch::new(
            frontend,
            Arc::new(tokio::sync::RwLock::new(session)),
            engines,
        )
        .run_command(path)
        .await
        .unwrap_err();
        assert!(
            matches!(
                error,
                CommandError::Engine(EngineError::UnknownAgent { .. })
            ),
            "{path:?}: {error:?}"
        );
        assert_eq!(error.http_status(), 404, "{path:?}");
        assert_eq!(error.exit_code(), 4, "{path:?}");
        assert!(error.to_string().contains(GEMINI_NOTE), "{path:?}: {error}");
    }
    assert!(
        !docker.calls().iter().any(|call| matches!(
            call.argv.first().map(String::as_str),
            Some("run" | "build" | "pull")
        )),
        "{:#?}",
        docker.calls()
    );
    assert!(host_gemini.calls().is_empty());
    assert!(!repo.path().join(".awman/Dockerfile.gemini").exists());
    s.finish();
}
