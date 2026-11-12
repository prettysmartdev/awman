//! Ready's host-auth phase against a recording pinger: no host agents run.
use super::*;
use crate::data::{
    config::env::EnvSnapshot,
    fs::AuthPathResolver,
    message::{UserMessage, UserMessageSink},
    session::SessionOpenOptions,
};
use awman_test_support::{TestCase, TestSandbox};
use std::{collections::HashMap, path::Path, sync::Mutex};

#[derive(Default)]
struct Pinger {
    calls: Mutex<Vec<String>>,
    failures: HashMap<String, LocalAgentPingResult>,
}

#[async_trait::async_trait]
impl ReadyHostPing for Pinger {
    async fn ping(&self, def: &crate::engine::agent::ResolvedAgent) -> LocalAgentPingResult {
        self.calls.lock().unwrap().push(def.name.clone());
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
                "Auth: claude, codex".into(),
                StepStatus::Failed(String::new())
            ),
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
