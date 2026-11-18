//! Ready's host-auth phase against a recording pinger: no host agents run.
use super::*;
use crate::data::{
    config::env::EnvSnapshot,
    fs::AuthPathResolver,
    message::{UserMessage, UserMessageSink},
    session::SessionOpenOptions,
};
use awman_test_support::{TestCase, TestSandbox};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

#[derive(Default)]
struct Pinger {
    calls: Mutex<Vec<String>>,
    failures: HashMap<String, LocalAgentPingResult>,
    barrier: Option<tokio::sync::Barrier>,
    pending: Vec<String>,
    active: Arc<AtomicUsize>,
}

struct ActivePing(Arc<AtomicUsize>);
impl Drop for ActivePing {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ReadyHostPing for Pinger {
    async fn ping(&self, def: &crate::engine::agent::ResolvedAgent) -> LocalAgentPingResult {
        self.calls.lock().unwrap().push(def.name.clone());
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = ActivePing(self.active.clone());
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        if self.pending.contains(&def.name) {
            std::future::pending::<()>().await;
        }
        self.failures
            .get(&def.name)
            .cloned()
            .unwrap_or_else(|| LocalAgentPingResult::Ok {
                greeting: "Hello".into(),
                response: "Hi".into(),
            })
    }
}

#[derive(Default)]
struct Frontend {
    messages: Vec<UserMessage>,
    active_pings: Option<Arc<AtomicUsize>>,
    results: Vec<LocalAgentPingResult>,
}
impl UserMessageSink for Frontend {
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message);
    }
    fn replay_queued(&mut self) {}
}
impl crate::engine::agent::AgentImageFrontend for Frontend {
    fn report_step_status(&mut self, _: &SetupStep, _: StepStatus) {}
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        panic!("host-auth checks must not launch a container")
    }
}
impl ReadyFrontend for Frontend {
    fn ask_create_dockerfile(&mut self, _: &Path) -> Result<bool, EngineError> {
        panic!("host-auth checks must not prompt for a Dockerfile")
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        panic!("host-auth checks must not start an audit")
    }
    fn report_phase(&mut self, _: &ReadyPhase) {}
    fn report_summary(&mut self, _: &ReadySummary) {}
    fn report_ping(&mut self, result: &LocalAgentPingResult) {
        if let Some(active) = &self.active_pings {
            assert_eq!(
                active.load(Ordering::SeqCst),
                0,
                "reported before the batch finished"
            );
        }
        self.results.push(result.clone());
    }
}

fn engine(sandbox: &TestSandbox, selected: &str, pinger: Arc<Pinger>) -> ReadyEngine {
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
    let mut engine = ReadyEngine::new(
        None,
        sandbox.awman_home(),
        session,
        Arc::new(GitEngine::with_env(env)),
        overlay,
        runtime,
        agent_engine,
        ReadyEngineOptions {
            agent: AgentName::new(selected).unwrap(),
            refresh: false,
            build: false,
            no_cache: false,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    );
    engine.host_agent = pinger;
    engine.phase = ReadyPhase::CheckingLocalAgent;
    engine
}

fn dockerfiles(sandbox: &TestSandbox, names: &[&str]) {
    let dir = sandbox.work().join(".awman");
    std::fs::create_dir_all(&dir).unwrap();
    for name in names {
        std::fs::write(dir.join(format!("Dockerfile.{name}")), "FROM scratch\n").unwrap();
    }
}

#[tokio::test]
async fn greetings_cover_repo_agents_and_selected_agent_in_one_success_row() {
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::greetings_cover_repo_agents_and_selected_agent_in_one_success_row",
    ));
    // The selected agent is included even without its own Dockerfile.
    dockerfiles(&sandbox, &["claude", "opencode"]);
    let pinger = Arc::new(Pinger::default());
    let mut engine = engine(&sandbox, "codex", pinger.clone());
    assert_eq!(
        engine.step(&mut Frontend::default()).await.unwrap(),
        ReadyPhase::RunningAudit
    );
    assert_eq!(
        *pinger.calls.lock().unwrap(),
        ["claude", "codex", "opencode"]
    );
    let summary = engine.summary();
    assert_eq!(summary.local_agent, StepStatus::Done);
    assert_eq!(
        summary.auth_rows(),
        vec![("Auth: claude, codex, opencode".into(), StepStatus::Done)]
    );
}

#[tokio::test]
async fn failed_greetings_do_not_stop_other_agents_and_share_one_failure_row() {
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::failed_greetings_do_not_stop_other_agents_and_share_one_failure_row",
    ));
    dockerfiles(&sandbox, &["claude", "codex", "opencode"]);
    let pinger = Arc::new(Pinger {
        failures: HashMap::from([
            ("claude".into(), LocalAgentPingResult::TimedOut),
            ("codex".into(), LocalAgentPingResult::NotInstalled),
        ]),
        ..Default::default()
    });
    let mut engine = engine(&sandbox, "claude", pinger.clone());
    let mut frontend = Frontend::default();
    engine.step(&mut frontend).await.unwrap();
    // The default is checked once and failures do not prevent later pings.
    assert_eq!(
        *pinger.calls.lock().unwrap(),
        ["claude", "codex", "opencode"]
    );
    let summary = engine.summary();
    assert!(matches!(summary.local_agent, StepStatus::Failed(_)));
    assert_eq!(
        summary.auth_rows(),
        vec![
            ("Auth: opencode".into(), StepStatus::Done),
            (
                "Auth: claude".into(),
                StepStatus::Warn("claude: host ping timed out after 10 seconds".into())
            ),
            ("Auth: codex".into(), StepStatus::Failed(String::new())),
        ]
    );
    assert!(frontend
        .messages
        .iter()
        .any(|m| m.text.contains("claude: host ping timed out")));
    assert!(frontend
        .messages
        .iter()
        .any(|m| m.text.contains("codex: not installed")));
}

#[tokio::test(start_paused = true)]
async fn host_checks_start_together_timeout_at_ten_seconds_and_report_after_the_batch() {
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::host_checks_start_together_timeout_at_ten_seconds_and_report_after_the_batch",
    ));
    dockerfiles(&sandbox, &["claude", "codex", "opencode"]);
    let pinger = Arc::new(Pinger {
        barrier: Some(tokio::sync::Barrier::new(3)),
        pending: vec!["claude".into(), "codex".into()],
        ..Default::default()
    });
    let mut frontend = Frontend {
        active_pings: Some(pinger.active.clone()),
        ..Default::default()
    };
    let mut engine = engine(&sandbox, "claude", pinger.clone());
    let start = tokio::time::Instant::now();
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(start.elapsed(), std::time::Duration::from_secs(10));
    assert_eq!(
        *pinger.calls.lock().unwrap(),
        ["claude", "codex", "opencode"]
    );
    assert_eq!(frontend.results.len(), 3);
    assert_eq!(
        frontend.results[..2],
        [
            LocalAgentPingResult::TimedOut,
            LocalAgentPingResult::TimedOut
        ]
    );
    assert!(matches!(
        frontend.results[2],
        LocalAgentPingResult::Ok { .. }
    ));
    let summary = engine.summary();
    assert!(matches!(summary.local_agent, StepStatus::Warn(_)));
    let warning = &summary.auth_rows()[1];
    assert_eq!(warning.0, "Auth: claude, codex");
    assert!(matches!(&warning.1, StepStatus::Warn(reason) if reason.contains("10 seconds")));
}

#[tokio::test]
async fn skipped_custom_ping_does_not_skip_other_agents_or_bypass_trust() {
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::skipped_custom_ping_does_not_skip_other_agents_or_bypass_trust",
    ));
    dockerfiles(&sandbox, &["localonly", "claude", "untrusted"]);
    let definition = r#"schema = 1
display_name = "Local Only"
[run]
command = ["localonly"]
[image]
install = ["echo install"]
"#;
    let global = sandbox.awman_home().join("agents");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(global.join("localonly.toml"), definition).unwrap();
    let repo = sandbox.work().join(".awman/agents");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("untrusted.toml"),
        format!("{definition}\n[ping]\ncommand = [\"untrusted\"]\n"),
    )
    .unwrap();
    let pinger = Arc::new(Pinger::default());
    let mut engine = engine(&sandbox, "localonly", pinger.clone());
    let mut frontend = Frontend::default();
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(*pinger.calls.lock().unwrap(), ["claude"]);
    let summary = engine.summary();
    assert_eq!(
        summary.agent_auth[1],
        ("localonly".into(), StepStatus::Skipped)
    );
    assert!(matches!(
        &summary.agent_auth[2],
        (name, StepStatus::Failed(reason))
            if name == "untrusted" && reason.contains("has not been approved")
    ));
    assert!(frontend
        .messages
        .iter()
        .any(|m| m.text.contains("host check skipped")));
    assert_eq!(
        summary.auth_rows(),
        vec![
            ("Auth: claude".into(), StepStatus::Done),
            ("Auth: untrusted".into(), StepStatus::Failed(String::new())),
        ]
    );
}

/// Session setup's `CreatingDockerfile` with `runtime.dockerfile` set to
/// `configured`: what one step does to the repository.
async fn create_project_dockerfile(
    sandbox: &TestSandbox,
    configured: &serde_json::Value,
) -> Result<ReadyPhase, EngineError> {
    let dir = sandbox.work().join(".awman");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        serde_json::json!({"runtime": {"dockerfile": configured}}).to_string(),
    )
    .unwrap();
    let mut engine = engine(sandbox, "claude", Arc::new(Pinger::default()));
    engine.phase = ReadyPhase::CreatingDockerfile;
    engine.step(&mut Frontend::default()).await
}

#[tokio::test]
async fn setup_creates_a_configured_dockerfile_with_missing_parents_and_never_overwrites() {
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::contracts::setup_creates_a_configured_dockerfile_with_missing_parents_and_never_overwrites",
    ));
    let target = sandbox.work().join("missing/Project.Dockerfile");
    create_project_dockerfile(&sandbox, &"missing/Project.Dockerfile".into())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        crate::data::templates::project_dockerfile_dev()
    );
    // A file that appeared after Preflight's existence check is kept.
    std::fs::write(&target, "FROM mine\n").unwrap();
    create_project_dockerfile(&sandbox, &"missing/Project.Dockerfile".into())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "FROM mine\n");
}

#[tokio::test]
async fn setup_never_creates_a_dockerfile_outside_the_git_root() {
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::contracts::setup_never_creates_a_dockerfile_outside_the_git_root",
    ));
    let outside_dir = sandbox.root().join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside = outside_dir.join("Dockerfile");
    let err = create_project_dockerfile(&sandbox, &outside.display().to_string().into())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("outside the git root"), "{err}");
    assert!(!outside.exists());

    #[cfg(unix)]
    {
        // Through a committed directory link, and to a dangling final link.
        std::os::unix::fs::symlink(&outside_dir, sandbox.work().join("docker")).unwrap();
        std::os::unix::fs::symlink(outside_dir.join("gone"), sandbox.work().join("dangling"))
            .unwrap();
        for (configured, why) in [
            (
                "docker/Project.Dockerfile",
                "symbolic link or not a directory",
            ),
            ("dangling", "is a symbolic link"),
        ] {
            let err = create_project_dockerfile(&sandbox, &configured.into())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{configured}: {err}");
        }
        assert_eq!(std::fs::read_dir(&outside_dir).unwrap().count(), 0);
    }
}

/// WI 0163: sessions keep a stale image when its rebuild fails, but
/// `ready --build` is the explicit escape hatch and still fails hard.
#[tokio::test]
async fn ready_build_failure_is_still_a_failed_stage_not_an_image_stale_warning() {
    use crate::engine::agent::image_freshness::test_runtime::BuildRuntime;
    let sandbox = TestSandbox::for_test(TestCase::component(
        "engine::ready::ready_build_failure_is_still_a_failed_stage_not_an_image_stale_warning",
    ));
    std::fs::write(
        sandbox.work().join("Dockerfile.dev"),
        "FROM scratch\nRUN false\n",
    )
    .unwrap();
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
    let runtime = Arc::new(BuildRuntime::default());
    let tag = crate::data::image_tags::project_image_tag(&sandbox.work());
    let previous = runtime.install(&tag, Some("previous"));
    runtime.fail(&tag, true);
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        AuthPathResolver::at_home(sandbox.home()),
    ));
    let mut engine = ReadyEngine::new(
        None,
        sandbox.awman_home(),
        session,
        Arc::new(GitEngine::with_env(env)),
        overlay.clone(),
        runtime.clone(),
        Arc::new(AgentEngine::new(overlay, runtime.clone())),
        ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: true,
            no_cache: true,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    );
    engine.phase = ReadyPhase::BuildingBaseImage;
    let mut frontend = Frontend::default();
    let store = crate::engine::agent::image_freshness::runtime_build_store(
        runtime.as_ref(),
        engine.session.env(),
    );
    let reference = crate::engine::image_prep::AgentImagePreparer::normalize_build_reference(&tag);
    let locks =
        crate::data::image_build_lock::ImageBuildLockStore::under_data_home(&sandbox.awman_home());
    let held = locks.try_lock(&store, &reference).unwrap().unwrap();
    let phase = {
        let step = engine.step(&mut frontend);
        tokio::pin!(step);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut step)
                .await
                .is_err(),
            "ready must wait for a session holding the same project build lease"
        );
        assert!(runtime.built().is_empty());
        drop(held);
        step.await.unwrap()
    };
    assert_eq!(phase, ReadyPhase::BuildingAgentImage);
    assert_eq!(
        runtime.built(),
        std::slice::from_ref(&tag),
        "--build rebuilds a labeled image"
    );
    assert!(
        matches!(&engine.summary().base_image, StepStatus::Failed(m) if m.contains("RUN false")),
        "{:?}",
        engine.summary().base_image
    );
    assert!(!engine.summary().is_ready());
    assert!(!frontend
        .messages
        .iter()
        .any(|m| m.text.contains("[image] warning")));
    assert_eq!(runtime.id(&tag), Some(previous));
}
