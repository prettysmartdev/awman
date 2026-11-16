//! Init's project files against a headless (API-profile) frontend: what is
//! created, where, what is kept byte-for-byte, and what the result reports.
//! The headless profile skips the audit, so no container runtime is touched.
use super::*;
use crate::data::{
    config::env::EnvSnapshot,
    config::work_items::WorkItemsConfig,
    fs::AuthPathResolver,
    init_files::InitFileKind,
    message::{UserMessage, UserMessageSink},
    session::SessionOpenOptions,
    templates,
};
use crate::engine::init::frontend::DockerfileSetupDecision;
use awman_test_support::{TestCase, TestSandbox};

/// Answers exactly as `HeadlessDefaults::api()` does (Layer 2 pins that
/// table; this engine test cannot name a Layer 2 type) and records what it
/// was asked.
#[derive(Default)]
struct Headless {
    messages: Vec<UserMessage>,
    dockerfile_questions: Vec<String>,
}
impl UserMessageSink for Headless {
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message);
    }
    fn replay_queued(&mut self) {}
}
impl crate::engine::agent::AgentImageFrontend for Headless {
    fn report_step_status(&mut self, _: &SetupStep, _: StepStatus) {}
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        panic!("headless init must not launch a container")
    }
}
impl InitFrontend for Headless {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(None)
    }
    fn ask_dockerfile_setup(
        &mut self,
        _: &Prompt<DockerfileSetupChoice>,
        _: &std::path::Path,
        dockerfile_path: &str,
    ) -> Result<DockerfileSetupDecision, EngineError> {
        self.dockerfile_questions.push(dockerfile_path.to_string());
        Ok(DockerfileSetupDecision::CreateNew)
    }
    fn report_phase(&mut self, _: &InitPhase) {}
    fn report_summary(&mut self, _: &InitSummary) {}
}

fn sandbox(name: &str) -> TestSandbox {
    TestSandbox::for_test(TestCase::component(Box::leak(
        format!("engine::init::contracts::{name}").into_boxed_str(),
    )))
}

async fn run_init(
    sandbox: &TestSandbox,
    agent: &str,
) -> (Result<InitSummary, EngineError>, Headless) {
    run_init_with(sandbox, agent, None).await
}

/// `init`, or `init --aspec` served from `aspec_archive`.
async fn run_init_with(
    sandbox: &TestSandbox,
    agent: &str,
    aspec_archive: Option<Vec<u8>>,
) -> (Result<InitSummary, EngineError>, Headless) {
    let env = EnvSnapshot::from_full_env(
        sandbox
            .env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    let session = Arc::new(
        Session::open_at_directory_root(
            sandbox.work(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker_in(
        env.clone(),
    ));
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        AuthPathResolver::at_home(sandbox.home()),
    ));
    let agent_engine = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
    let mut engine = InitEngine::new(
        session,
        Arc::new(GitEngine::with_env(env)),
        overlay,
        runtime,
        agent_engine,
        InitEngineOptions {
            agent: AgentName::new(agent).unwrap(),
            run_aspec_setup: aspec_archive.is_some(),
            git_root: sandbox.work().to_path_buf(),
            dockerfile_setup_prompt: crate::data::prompt::Prompt {
                title: String::new(),
                body: String::new(),
                choices: Vec::new(),
                default_on_dismiss: None,
            },
        },
    );
    if let Some(archive) = aspec_archive {
        engine = engine
            .with_aspec_downloader(crate::engine::aspec::AspecDownloader::from_archive(archive));
    }
    let mut frontend = Headless::default();
    let result = engine.run_to_completion(&mut frontend).await;
    (result, frontend)
}

fn files(summary: &InitSummary) -> Vec<(String, InitFileKind, InitFileStatus)> {
    summary
        .files
        .iter()
        .map(|f| (f.path.clone(), f.kind, f.status.clone()))
        .collect()
}

fn write_config(sandbox: &TestSandbox, json: &str) {
    let dir = sandbox.work().join(".awman");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), json).unwrap();
}

#[tokio::test]
async fn uninitialised_repo_gets_the_template_dockerfile_config_and_agent_dockerfile() {
    let sandbox =
        sandbox("uninitialised_repo_gets_the_template_dockerfile_config_and_agent_dockerfile");
    let (summary, frontend) = run_init(&sandbox, "claude").await;
    let summary = summary.unwrap();
    let root = sandbox.work();
    assert_eq!(
        std::fs::read(root.join("Dockerfile.dev")).unwrap(),
        templates::project_dockerfile_dev().as_bytes()
    );
    assert!(root.join(".awman/config.json").is_file());
    assert!(root.join(".awman/Dockerfile.claude").is_file());
    assert_eq!(summary.dockerfile, StepStatus::Done);
    assert_eq!(summary.audit, StepStatus::Skipped);
    assert_eq!(summary.image_build, StepStatus::Skipped);
    assert_eq!(frontend.dockerfile_questions, ["Dockerfile.dev"]);
    assert_eq!(
        files(&summary),
        vec![
            (
                "Dockerfile.dev".into(),
                InitFileKind::ProjectDockerfile,
                InitFileStatus::Created
            ),
            (
                ".awman/config.json".into(),
                InitFileKind::RepoConfig,
                InitFileStatus::Created
            ),
            (
                ".awman/Dockerfile.claude".into(),
                InitFileKind::AgentDockerfile,
                InitFileStatus::Created
            ),
        ]
    );
    // The agent Dockerfile is rendered against the project image built from
    // the Dockerfile.dev created first.
    let agent = std::fs::read_to_string(root.join(".awman/Dockerfile.claude")).unwrap();
    assert!(agent.contains(&crate::data::image_tags::project_image_tag(&root)));
}

#[tokio::test]
async fn existing_dockerfile_dev_is_kept_byte_for_byte_even_when_empty() {
    let sandbox = sandbox("existing_dockerfile_dev_is_kept_byte_for_byte_even_when_empty");
    for body in [&b"FROM debian:bookworm\nRUN echo mine\n"[..], &b""[..]] {
        let root = sandbox.work();
        std::fs::write(root.join("Dockerfile.dev"), body).unwrap();
        let (summary, frontend) = run_init(&sandbox, "claude").await;
        let summary = summary.unwrap();
        assert_eq!(std::fs::read(root.join("Dockerfile.dev")).unwrap(), body);
        assert!(frontend.dockerfile_questions.is_empty());
        assert_eq!(summary.dockerfile, StepStatus::Done);
        assert_eq!(
            summary
                .project_dockerfile()
                .map(|f| (&f.path[..], &f.status)),
            Some(("Dockerfile.dev", &InitFileStatus::Kept))
        );
    }
}

#[tokio::test]
async fn configured_relative_dockerfile_is_created_at_that_path_then_kept() {
    let sandbox = sandbox("configured_relative_dockerfile_is_created_at_that_path_then_kept");
    write_config(
        &sandbox,
        r#"{"runtime":{"dockerfile":"docker/Dockerfile.project"}}"#,
    );
    let root = sandbox.work();
    let (summary, frontend) = run_init(&sandbox, "claude").await;
    let summary = summary.unwrap();
    assert_eq!(frontend.dockerfile_questions, ["docker/Dockerfile.project"]);
    assert_eq!(
        std::fs::read(root.join("docker/Dockerfile.project")).unwrap(),
        templates::project_dockerfile_dev().as_bytes()
    );
    assert!(!root.join("Dockerfile.dev").exists());
    assert_eq!(
        files(&summary)[..2],
        [
            (
                "docker/Dockerfile.project".into(),
                InitFileKind::ProjectDockerfile,
                InitFileStatus::Created
            ),
            (
                ".awman/config.json".into(),
                InitFileKind::RepoConfig,
                InitFileStatus::Kept
            ),
        ]
    );

    let (again, frontend) = run_init(&sandbox, "claude").await;
    let again = again.unwrap();
    assert!(frontend.dockerfile_questions.is_empty(), "nothing to ask");
    assert_eq!(
        files(&again),
        vec![
            (
                "docker/Dockerfile.project".into(),
                InitFileKind::ProjectDockerfile,
                InitFileStatus::Kept
            ),
            (
                ".awman/config.json".into(),
                InitFileKind::RepoConfig,
                InitFileStatus::Kept
            ),
            (
                ".awman/Dockerfile.claude".into(),
                InitFileKind::AgentDockerfile,
                InitFileStatus::Kept
            ),
        ]
    );
}

#[tokio::test]
async fn configured_dockerfile_outside_the_git_root_is_skipped_with_a_reason() {
    let sandbox = sandbox("configured_dockerfile_outside_the_git_root_is_skipped_with_a_reason");
    let outside = sandbox.root().join("elsewhere").join("Dockerfile");
    write_config(
        &sandbox,
        &serde_json::json!({"runtime": {"dockerfile": outside}}).to_string(),
    );
    let (summary, _) = run_init(&sandbox, "claude").await;
    let summary = summary.unwrap();
    assert!(!outside.exists(), "init never writes outside the git root");
    assert!(!sandbox.work().join("Dockerfile.dev").exists());
    assert_eq!(summary.dockerfile, StepStatus::Skipped);
    let entry = summary.project_dockerfile().unwrap();
    assert_eq!(entry.path, outside.display().to_string());
    assert!(matches!(
        &entry.status,
        InitFileStatus::Skipped { reason } if reason.contains("outside the git root")
    ));
    // The agent Dockerfile is still provisioned.
    assert!(sandbox.work().join(".awman/Dockerfile.claude").is_file());
}

#[tokio::test]
async fn custom_agent_whose_dockerfile_cannot_render_fails_init() {
    let sandbox = sandbox("custom_agent_whose_dockerfile_cannot_render_fails_init");
    let global = sandbox.awman_home().join("agents");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("localonly.toml"),
        "schema = 1\ndisplay_name = \"Local Only\"\n[run]\ncommand = [\"localonly\"]\n[image]\ninstall = [\"echo install\"]\n",
    )
    .unwrap();
    // A directory where the agent Dockerfile goes cannot be read or replaced.
    std::fs::create_dir_all(sandbox.work().join(".awman/Dockerfile.localonly")).unwrap();
    let (result, _) = run_init(&sandbox, "localonly").await;
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("agent Dockerfile render failed"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn init_aspec_reports_every_template_file_it_writes() {
    let sandbox = sandbox("init_aspec_reports_every_template_file_it_writes");
    let root = sandbox.work();
    std::fs::create_dir_all(root.join("aspec")).unwrap();
    std::fs::write(root.join("aspec/foundation.md"), "mine").unwrap();
    let archive = crate::engine::aspec::tests::archive(&[
        ("aspec-main/aspec/foundation.md", Some("template")),
        (
            "aspec-main/aspec/work-items/0000-template.md",
            Some("# Template\n"),
        ),
    ]);
    let (summary, _) = run_init_with(&sandbox, "claude", Some(archive)).await;
    let summary = summary.unwrap();
    assert_eq!(summary.aspec_folder, StepStatus::Done);
    let aspec: Vec<_> = files(&summary)
        .into_iter()
        .filter(|(_, kind, _)| *kind == InitFileKind::AspecFile)
        .collect();
    assert_eq!(
        aspec,
        vec![
            (
                "aspec/foundation.md".into(),
                InitFileKind::AspecFile,
                InitFileStatus::Updated
            ),
            (
                "aspec/work-items/0000-template.md".into(),
                InitFileKind::AspecFile,
                InitFileStatus::Created
            ),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("aspec/foundation.md")).unwrap(),
        "template"
    );
    // Every written file is something to commit (no git here: the default).
    assert!(summary
        .files
        .iter()
        .filter(|f| f.status.is_written())
        .all(|f| f.uncommitted));
}

#[cfg(unix)]
#[tokio::test]
async fn init_never_writes_through_a_linked_directory() {
    use std::os::unix::fs::symlink;
    let sandbox = sandbox("init_never_writes_through_a_linked_directory");
    let outside = sandbox.root().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // runtime.dockerfile below a committed directory link: skipped.
    write_config(
        &sandbox,
        r#"{"runtime":{"dockerfile":"docker/Project.Dockerfile"}}"#,
    );
    symlink(&outside, sandbox.work().join("docker")).unwrap();
    let (summary, frontend) = run_init(&sandbox, "claude").await;
    let summary = summary.unwrap();
    assert_eq!(summary.dockerfile, StepStatus::Skipped);
    let entry = summary.project_dockerfile().unwrap();
    assert_eq!(entry.path, "docker/Project.Dockerfile");
    assert!(
        matches!(&entry.status, InitFileStatus::Skipped { reason } if reason.contains("symbolic link")),
        "{entry:?}"
    );
    assert!(!entry.uncommitted);
    assert!(frontend
        .messages
        .iter()
        .any(|m| m.text.contains("symbolic link")));
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);

    // A linked `.awman/`: init refuses before writing anything.
    let sandbox = self::sandbox("init_never_writes_through_a_linked_directory_awman");
    symlink(&outside, sandbox.work().join(".awman")).unwrap();
    let (result, _) = run_init(&sandbox, "claude").await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn an_unusable_existing_dockerfile_is_skipped_not_kept() {
    let sandbox = sandbox("an_unusable_existing_dockerfile_is_skipped_not_kept");
    let root = sandbox.work();
    std::os::unix::fs::symlink(root.join("gone"), root.join("Dockerfile.dev")).unwrap();
    let (summary, frontend) = run_init(&sandbox, "claude").await;
    let summary = summary.unwrap();
    assert!(frontend.dockerfile_questions.is_empty());
    assert_eq!(summary.dockerfile, StepStatus::Skipped);
    assert!(matches!(
        &summary.project_dockerfile().unwrap().status,
        InitFileStatus::Skipped { reason } if reason.contains("does not exist")
    ));
    assert!(
        !root.join("gone").exists(),
        "nothing written through the link"
    );

    std::fs::remove_file(root.join("Dockerfile.dev")).unwrap();
    std::fs::create_dir_all(root.join("Dockerfile.dev")).unwrap();
    let (summary, _) = run_init(&sandbox, "claude").await;
    assert!(matches!(
        &summary.unwrap().project_dockerfile().unwrap().status,
        InitFileStatus::Skipped { reason } if reason.contains("directory")
    ));
}
