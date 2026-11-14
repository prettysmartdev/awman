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

#[cfg(unix)]
#[test]
fn context_launch_events_and_errors_do_not_expose_host_paths() {
    use awman::command::commands::launch_policy::LaunchPolicy;
    use awman::data::{
        config::overlays::{ContextOverlaySpec, ContextScope, OverlayPermission},
        context::ContextKey,
        execution_event::EventPayload,
        fs::context_dirs::ContextDirResolver,
        session::RepoContextBinding,
    };
    let s = crate::support::sandbox(
        "security::trust::context_launch_events_and_errors_do_not_expose_host_paths",
    );
    let mut session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let bus = EventBus::new(32);
    let mut events = bus.subscribe();
    let mut frontend = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
    let spec = ContextOverlaySpec {
        scope: ContextScope::Repo,
        permission: OverlayPermission::ReadWrite,
        key: None,
    };
    let agent = AgentName::new("claude").unwrap();
    LaunchPolicy::for_session(&session)
        .resolve_context_overlays(
            std::slice::from_ref(&spec),
            &agent,
            None,
            None,
            &mut frontend,
        )
        .unwrap();
    let mut messages = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventPayload::StatusMessage { message, .. } = event.payload {
            messages.push(message);
        }
    }
    assert!(messages.iter().any(|m| m.contains("local fallback")));
    for message in messages {
        assert!(
            !message.contains(s.awman_home().to_str().unwrap()),
            "{message}"
        );
    }
    let key = ContextKey::parse("secret-key").unwrap();
    let resolver = ContextDirResolver::at_home(s.awman_home());
    let keyed = resolver.keyed_repo_dir(&key);
    std::fs::create_dir_all(keyed.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(s.work(), &keyed).unwrap();
    session.set_repo_context(RepoContextBinding::Keyed(key));
    let error = LaunchPolicy::for_session(&session)
        .resolve_context_overlays(&[spec], &agent, None, None, &mut frontend)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "context overlay directory is unavailable"
    );
    assert!(!error.to_string().contains(s.awman_home().to_str().unwrap()));
}
