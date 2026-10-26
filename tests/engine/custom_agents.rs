//! Cross-layer custom definition → ready → builder and squad regressions.
use crate::helpers::{custom_agents::*, IsolatedEnv};
use awman::data::{image_tags, step_status::StepStatus};
use awman::engine::{
    agent::{custom::render_dockerfile, AgentRegistry},
    error::EngineError,
    ready::ReadyPhase,
    squad::launcher::SquadAgentLauncher,
};
use std::sync::Arc;

async fn prepare_images(
    session: Arc<awman::data::session::Session>,
    runtime: Arc<RecordingRuntime>,
) {
    let mut ready = ready(session, runtime, "pi");
    let mut frontend = ReadyRecorder::default();
    let summary = ready.run_to_completion(&mut frontend).await.unwrap();
    assert_eq!(summary.agent_image, StepStatus::Done);
    assert_eq!(summary.agent_definition, Some(StepStatus::Done));
    assert_eq!(summary.local_agent, StepStatus::Skipped);
    assert_eq!(ready.phase(), &ReadyPhase::Complete);
    assert!(frontend
        .messages
        .iter()
        .any(|message| message.text.contains("pi.toml")));
}

async fn ready_rebuilds(escape_hatch: bool) {
    let fixture = IsolatedEnv::new();
    let root = fixture.git_root.path();
    std::fs::write(root.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    let toml = if escape_hatch {
        INSTALL.replace(
            "install = [\"printf pi-v1 > /usr/local/bin/pi\", \"chmod +x /usr/local/bin/pi\"]",
            "dockerfile = \"pi.Dockerfile\"",
        )
    } else {
        INSTALL.into()
    };
    write_definition(root, "pi", &toml);
    let source = root.join(".awman/agents/pi.Dockerfile");
    if escape_hatch {
        std::fs::write(&source, "FROM {{AWMAN_BASE_IMAGE}}\nRUN echo pi-v1\n").unwrap();
    }
    let session = Arc::new(fixture.open_session());
    approve(&session, "pi");
    let runtime = Arc::new(RecordingRuntime::default());
    prepare_images(session.clone(), runtime.clone()).await;
    let name = awman::data::session::AgentName::new("pi").unwrap();
    let def = AgentRegistry::resolve_for_session(&session, &name).unwrap();
    let expected = render_dockerfile(&def, &image_tags::project_image_tag(root))
        .unwrap()
        .content;
    {
        let builds = runtime.builder.builds.lock().unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(
            builds[0].0.reference,
            image_tags::agent_image_tag(root, "pi")
        );
        assert_eq!(builds[0].0.dockerfile, root.join(".awman/Dockerfile.pi"));
        assert_eq!(builds[0].0.context, root);
        assert!(!builds[0].0.no_cache);
        assert_eq!(builds[0].1, expected);
        assert!(expected.contains("pi-v1"));
        assert!(!expected.contains("{{AWMAN_BASE_IMAGE}}"));
    }
    // Repeating ready without an edit is idempotent, even with a fresh engine.
    prepare_images(session.clone(), runtime.clone()).await;
    assert_eq!(runtime.builder.builds.lock().unwrap().len(), 1);
    if escape_hatch {
        std::fs::write(&source, "FROM {{AWMAN_BASE_IMAGE}}\nRUN echo pi-v2\n").unwrap();
        write_definition(root, "pi", &toml.replace("Pi", "Pi edited"));
    } else {
        write_definition(root, "pi", &toml.replace("pi-v1", "pi-v2"));
    }
    // Changed content cannot render or build until its new hash is approved.
    let mut unapproved = ready(session.clone(), runtime.clone(), "pi");
    let error = unapproved
        .step(&mut ReadyRecorder::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, EngineError::AgentDefinitionUntrusted { .. }),
        "{error:?}"
    );
    assert_eq!(runtime.builder.builds.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(root.join(".awman/Dockerfile.pi")).unwrap(),
        expected
    );
    // Re-approve changed content, just as an interactive ready would do.
    approve(&session, "pi");
    prepare_images(session, runtime.clone()).await;
    let builds = runtime.builder.builds.lock().unwrap();
    assert_eq!(
        builds.len(),
        2,
        "changed recipe forces rebuild of an existing image"
    );
    assert_ne!(builds[0].1, builds[1].1);
    assert_ne!(builds[0].1.lines().next(), builds[1].1.lines().next());
    assert!(builds[1].1.contains("pi-v2"));
    assert_eq!(
        std::fs::read_to_string(&builds[1].0.dockerfile).unwrap(),
        builds[1].1
    );
    assert!(runtime.options.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ready_custom_install_renders_and_forces_rebuild_after_edit() {
    ready_rebuilds(false).await;
}
#[tokio::test]
async fn ready_custom_escape_hatch_renders_and_forces_rebuild_after_edit() {
    ready_rebuilds(true).await;
}

#[tokio::test]
async fn squad_directory_renders_custom_definition_and_refuses_untrusted_without_prompt() {
    let fixture = IsolatedEnv::new();
    let root = fixture.git_root.path();
    write_definition(root, "pi", INSTALL);
    let error = SquadAgentLauncher::ensure_directory_workspace_project_with_env(
        &fixture.env(),
        root,
        &["pi".into()],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, EngineError::AgentDefinitionUntrusted { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("awman ready"));
    assert!(!root.join("Dockerfile.dev").exists());
    assert!(!root.join(".awman/Dockerfile.pi").exists());
    approve(&fixture.open_session(), "pi");
    SquadAgentLauncher::ensure_directory_workspace_project_with_env(
        &fixture.env(),
        root,
        &["pi".into()],
    )
    .await
    .unwrap();
    let rendered = std::fs::read_to_string(root.join(".awman/Dockerfile.pi")).unwrap();
    assert!(rendered.contains(&image_tags::project_image_tag(root)));
    assert!(rendered.contains("pi-v1"));
    assert!(root.join("Dockerfile.dev").is_file());
}

#[tokio::test]
async fn ready_custom_crush_sbx_is_failed_step_before_any_kit_or_build() {
    let fixture = IsolatedEnv::new();
    write_definition(
        fixture.git_root.path(),
        "crush",
        include_str!("../../skills/create-custom-agent/examples/crush.toml"),
    );
    let session = Arc::new(fixture.open_session());
    approve(&session, "crush");
    let runtime = Arc::new(RecordingRuntime::sandbox());
    let mut engine = ready(session, runtime.clone(), "crush");
    let mut frontend = ReadyRecorder::default();
    let error = engine.step(&mut frontend).await.unwrap_err();
    assert!(
        matches!(error, EngineError::CustomAgentRuntimeUnsupported { .. }),
        "{error:?}"
    );
    assert!(matches!(
        engine.summary().agent_image,
        StepStatus::Failed(_)
    ));
    assert!(frontend
        .statuses
        .iter()
        .any(|(_, status)| matches!(status, StepStatus::Failed(_))));
    assert_eq!(engine.phase(), &ReadyPhase::Complete);
    assert!(runtime.builder.builds.lock().unwrap().is_empty());
    assert!(runtime.options.lock().unwrap().is_empty());
    assert!(!fixture
        .git_root
        .path()
        .join(".awman/Dockerfile.crush")
        .exists());
    assert!(!fixture.home_dir.path().join("kits").exists());
}
