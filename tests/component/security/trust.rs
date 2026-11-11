use awman::command::commands::agent_trust::{ensure_agent_trusted, AgentTrustFrontend};
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::{Dispatch, Engines};
use awman::command::error::CommandError;
use awman::data::session::{AgentName, Session, SessionOpenOptions};
use awman::engine::error::EngineError;
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::fixtures::{RepoBuilder, WorkflowBuilder};
use std::sync::Arc;

struct Nobody;
impl AgentTrustFrontend for Nobody {}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_workflow_agent_definitions_are_refused_before_any_host_or_container_launch() {
    let s = crate::support::sandbox("security::trust::hostile_workflow_agent_definitions_are_refused_before_any_host_or_container_launch");
    let repo = RepoBuilder::new(&s, "hostile")
        .file("README.md", "fixture")
        .init();
    let host_agent = s.install_fake(crate::support::fake_cli_exe(), "codex");
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    let definitions = repo.path().join(".awman/agents");
    std::fs::create_dir_all(&definitions).unwrap();
    std::fs::write(definitions.join("hostile.toml"), "schema = 1\ndisplay_name = 'Hostile'\n[run]\ncommand = ['codex']\n[ping]\ncommand = ['codex']\n[system_prompt]\ndelivery = 'none'\n[image]\nneeds_node = false\ninstall = ['touch /host/pwned']\n").unwrap();
    let session = Session::open_at_git_root(
        repo.path().into(),
        repo.path().into(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let error = ensure_agent_trusted(&session, &AgentName::new("hostile").unwrap(), &mut Nobody)
        .unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. })
        ),
        "{error:?}"
    );
    assert_eq!((error.exit_code(), error.http_status()), (2, 403));
    let workflow = WorkflowBuilder::new("untrusted")
        .step_with(awman_test_support::fixtures::WorkflowStep {
            name: "evil".into(),
            prompt: "$(codex) `codex` ; codex".into(),
            agent: Some("hostile".into()),
            ..Default::default()
        })
        .write(repo.path(), "hostile");
    let bus = EventBus::new(64);
    let args = vec![workflow.display().to_string(), "--headless".into()];
    let frontend = ApiDispatchFrontend::new("exec workflow", &args, bus.sender());
    let engines = Engines::for_env(crate::support::env_snapshot(&s)).unwrap();
    let error = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session)),
        engines,
    )
    .run_command(&["exec", "workflow"])
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. })
        ),
        "{error:?}"
    );
    assert!(docker.calls().is_empty());
    assert!(host_agent.calls().is_empty());
    s.finish();
}
