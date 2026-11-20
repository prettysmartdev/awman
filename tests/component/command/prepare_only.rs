//! `ready --prepare-only`: image preparation without any host-agent ping,
//! credential check or audit, proven through the production command against a
//! recording runtime and a fake host `claude` that leaves a marker if run.
use crate::support;
use awman::command::{
    commands::{
        api_server::event_bus::EventBus,
        ready::{ReadyCommand, ReadyCommandFlags, ReadyOutcome},
        Command,
    },
    dispatch::{catalogue::CommandCatalogue, Engines},
    error::CommandError,
};
use awman::data::{
    fs::AuthPathResolver,
    preparation::{AuthenticationCheck, PreparationPolicy},
    session::{AgentHandle, Session, SessionOpenOptions},
    step_status::StepStatus,
    WorkflowStateStore,
};
use awman::engine::{
    agent::AgentEngine,
    agent_runtime::{
        execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
        ResolvedAgentOptions,
    },
    auth::AuthEngine,
    error::EngineError,
    git::GitEngine,
    overlay::OverlayEngine,
};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::{
    fixtures::{ConfigTree, RepoBuilder},
    TestCase, TestSandbox,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// A building runtime whose images are present or whose build fails.
struct Runtime {
    caps: Capabilities,
    images_present: bool,
    builds: Mutex<Vec<String>>,
    /// When set, only builds whose request mentions this text fail.
    fail_only: Mutex<Option<String>>,
}

impl AgentRuntimeEngine for Runtime {
    fn ready_agent(
        &self,
        _: &str,
        _: awman::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn awman::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        unreachable!("a container runtime")
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(self.images_present)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(Some("/home/awman".into()))
    }
    fn build_image(
        &self,
        request: &awman::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        let text = format!("{request:?}");
        self.builds.lock().unwrap().push(text.clone());
        match &*self.fail_only.lock().unwrap() {
            Some(only) if !text.contains(only.as_str()) => Ok(()),
            _ => Err(EngineError::Other("simulated build failure".into())),
        }
    }
    fn runtime_name(&self) -> &'static str {
        "test"
    }
    fn display_name(&self) -> &'static str {
        "recording runtime"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("ready must never launch an agent container in these cases")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("runtime must not be reached")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("runtime must not be reached")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("runtime must not be reached")
    }
}

struct Fixture {
    sandbox: TestSandbox,
    session: Session,
    engines: Engines,
    runtime: Arc<Runtime>,
    marker: std::path::PathBuf,
}

/// A fake host `claude` that records being run. `ready` pings by executing it.
fn fixture(name: &'static str, images_present: bool) -> Fixture {
    let sandbox = TestSandbox::for_test(TestCase::component(name));
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "fixture\n")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(".awman/Dockerfile.claude", "FROM scratch\n")
        .file(".awman/Dockerfile.codex", "FROM scratch\n")
        .init();
    ConfigTree::new()
        .global(json!({"agents": {"auth":{"refresh":{"enabled":false}}}}))
        .write(&sandbox, Some(repo.path()));
    let marker = sandbox.root().join("host-agent-ran");
    let fake = sandbox.install_fake(support::fake_cli_exe(), "claude");
    fake.on(&[]).touch(&marker).stdout("hello\n");
    let mut entries = sandbox.env().clone();
    // No ambient executable search, including for the deliberately missing codex.
    entries.set("PATH", sandbox.bin());
    let env = awman::data::config::env::EnvSnapshot::from_instance_env(
        entries
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect(),
        repo.path().to_path_buf(),
        vec![],
    );
    let session = Session::open_at_git_root(
        repo.path().to_path_buf(),
        repo.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let env = session.env().clone();
    let runtime = Arc::new(Runtime {
        caps: Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: true,
            image_acquisition: ImageAcquisition::Build,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        },
        images_present,
        builds: Default::default(),
        fail_only: Default::default(),
    });
    let auth = AuthPathResolver::at_home(sandbox.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::new(&session).unwrap()),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
        workflow_state_store: Arc::new(WorkflowStateStore::new(&session)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: None,
    };
    Fixture {
        sandbox,
        session,
        engines,
        runtime,
        marker,
    }
}

fn flags(prepare_only: bool, build: bool) -> ReadyCommandFlags {
    ReadyCommandFlags {
        refresh: false,
        build,
        no_cache: false,
        non_interactive: true,
        allow_docker: false,
        json: true,
        prepare_only,
        agent: Some("claude".into()),
    }
}

async fn run(f: &Fixture, flags: ReadyCommandFlags) -> Result<ReadyOutcome, CommandError> {
    let bus = EventBus::new(1024);
    let frontend = ApiDispatchFrontend::new("ready", &[], bus.sender());
    ReadyCommand::new(flags, f.engines.clone(), f.session.clone())
        .run_with_frontend(Box::new(frontend))
        .await
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_only_runs_no_host_agent_and_says_authentication_was_not_checked() {
    let f = fixture(
        "command::prepare_only::prepare_only_runs_no_host_agent_and_says_authentication_was_not_checked",
        true,
    );
    let outcome = run(&f, flags(true, false)).await.unwrap();
    assert!(!f.marker.exists(), "a host agent was executed");
    assert!(outcome.ready && outcome.images_ready);
    assert_eq!(outcome.preparation_policy, PreparationPolicy::ImagesOnly);
    assert_eq!(outcome.authentication, AuthenticationCheck::NotChecked);
    assert_eq!(outcome.local_agent, StepStatus::Skipped);
    assert_eq!(outcome.audit, StepStatus::Skipped);
    assert!(outcome.agent_auth.is_empty() && outcome.agent_credentials.is_empty());
    let json = outcome.to_legacy_json();
    assert_eq!(json["preparation_policy"], "images_only");
    assert_eq!(json["images_ready"], true);
    assert_eq!(json["authentication"], "not_checked");
    assert_eq!(json["ready"], true);
}

#[cfg(unix)]
#[tokio::test]
async fn default_ready_still_pings_the_host_agent() {
    // The control: the same fixture without `--prepare-only` executes the
    // fake host agent, so the absence above is meaningful.
    let f = fixture(
        "command::prepare_only::default_ready_still_pings_the_host_agent",
        true,
    );
    let outcome = run(&f, flags(false, false)).await.unwrap();
    assert!(f.marker.exists(), "ordinary ready must still ping");
    assert_eq!(outcome.preparation_policy, PreparationPolicy::Full);
    // The fixture's other repo agent (codex) is not installed, so the
    // verdict is a failure, never `not_checked`.
    assert_eq!(outcome.authentication, AuthenticationCheck::Failed);
    assert_eq!(outcome.to_legacy_json()["preparation_policy"], "full");
    assert_eq!(outcome.to_legacy_json()["authentication"], "failed");
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_only_build_failure_still_fails() {
    let f = fixture(
        "command::prepare_only::prepare_only_build_failure_still_fails",
        false,
    );
    let outcome = run(&f, flags(true, true)).await.unwrap();
    assert!(!f.runtime.builds.lock().unwrap().is_empty(), "no build ran");
    assert!(!outcome.ready && !outcome.images_ready);
    assert_eq!(outcome.authentication, AuthenticationCheck::NotChecked);
    assert_eq!(outcome.to_legacy_json()["ready"], false);
    assert!(!f.marker.exists());
    // Process exit status follows `ready`.
    let _ = &f.sandbox;
}

#[tokio::test]
async fn prepare_only_with_an_audit_request_is_rejected_before_any_work() {
    let f = fixture(
        "command::prepare_only::prepare_only_with_an_audit_request_is_rejected_before_any_work",
        true,
    );
    // Direct construction bypasses the catalogue conflict; the engine still
    // refuses an audit request under preparation-only.
    let mut combined = flags(true, false);
    combined.refresh = true;
    let error = run(&f, combined).await.unwrap_err();
    assert!(error.to_string().contains("--prepare-only"), "{error}");
    assert!(f.runtime.builds.lock().unwrap().is_empty());
    assert!(!f.marker.exists());
}

#[test]
fn catalogue_accepts_prepare_only_with_its_composing_flags_and_rejects_refresh() {
    let catalogue = CommandCatalogue::get();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let parsed = catalogue
        .parse_raw_args(
            &["ready"],
            &args(&[
                "--prepare-only",
                "--agent",
                "codex",
                "--build",
                "--no-cache",
                "--headless",
                "--json",
            ]),
        )
        .unwrap();
    assert!(parsed.has_flag("prepare-only"));
    let error = catalogue
        .parse_raw_args(&["ready"], &args(&["--prepare-only", "--refresh"]))
        .unwrap_err();
    assert_eq!(error.exit_code(), 2, "{error:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_only_build_fails_when_a_requested_discovered_image_fails() {
    let f = fixture(
        "command::prepare_only::prepare_only_build_fails_when_a_requested_discovered_image_fails",
        true,
    );
    // The selected image builds; only the discovered codex image fails.
    *f.runtime.fail_only.lock().unwrap() = Some("claude".into());
    let outcome = run(&f, flags(true, true)).await.unwrap();
    assert!(!outcome.ready && !outcome.images_ready, "{outcome:?}");
    assert_eq!(outcome.to_legacy_json()["ready"], false);
    assert_eq!(outcome.to_legacy_json()["images_ready"], false);
    assert!(!f.marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_only_build_fails_for_an_untrusted_discovered_definition() {
    let f = fixture(
        "command::prepare_only::prepare_only_build_fails_for_an_untrusted_discovered_definition",
        true,
    );
    *f.runtime.fail_only.lock().unwrap() = Some(String::new());
    let root = f.session.git_root();
    std::fs::write(root.join(".awman/Dockerfile.untrusted"), "FROM scratch\n").unwrap();
    std::fs::create_dir_all(root.join(".awman/agents")).unwrap();
    std::fs::write(
        root.join(".awman/agents/untrusted.toml"),
        "schema = 1\ndisplay_name = \"U\"\n[run]\ncommand = [\"u\"]\n[image]\ninstall = [\"echo i\"]\n[ping]\ncommand = [\"u\"]\n",
    )
    .unwrap();
    let outcome = run(&f, flags(true, true)).await.unwrap();
    assert!(!outcome.ready && !outcome.images_ready, "{outcome:?}");
    assert!(!f.marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_only_without_build_keeps_discovered_images_optional() {
    let f = fixture(
        "command::prepare_only::prepare_only_without_build_keeps_discovered_images_optional",
        true,
    );
    std::fs::write(f.session.git_root().join(".awman/Dockerfile.other"), "x\n").unwrap();
    let outcome = run(&f, flags(true, false)).await.unwrap();
    assert!(outcome.images_ready, "{outcome:?}");
}

#[tokio::test]
async fn full_ready_reports_checkout_refusal_without_claiming_authentication() {
    let mut f = fixture(
        "command::prepare_only::full_ready_reports_checkout_refusal_without_claiming_authentication",
        true,
    );
    let bin = f.session.git_root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    awman_test_support::exe::install_as(
        &f.sandbox.bin().join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        }),
        &bin,
        "claude",
    );
    let mut env = f.sandbox.env().clone();
    env.set("PATH", &bin);
    f.session = Session::open_at_git_root(
        f.session.working_dir().to_path_buf(),
        f.session.git_root().to_path_buf(),
        SessionOpenOptions {
            // Deliberately leave invocation cwd elsewhere: ready must carry
            // the session root independently of the process snapshot cwd.
            env: Some(awman::data::config::env::EnvSnapshot::from_instance_env(
                env.iter()
                    .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
                    .collect(),
                f.sandbox.work(),
                vec![],
            )),
            ..Default::default()
        },
    )
    .unwrap();
    let outcome = run(&f, flags(false, false)).await.unwrap();
    assert!(
        matches!(&outcome.local_agent, StepStatus::Failed(message) if message.contains("refused: resolves inside the checkout")),
        "{outcome:?}"
    );
    assert_eq!(outcome.authentication, AuthenticationCheck::Failed);
    assert!(!f.marker.exists());
    f.sandbox.finish();
}

#[tokio::test]
async fn prepare_only_never_registers_refresh_even_with_an_available_monitor() {
    use awman::engine::{
        auth::credential::CredentialBinding,
        credential_refresh::{CredentialRefreshMonitor, MonitorConfig},
    };
    let mut f = fixture(
        "command::prepare_only::prepare_only_never_registers_refresh_even_with_an_available_monitor",
        true,
    );
    ConfigTree::new()
        .global(json!({"agents":{"auth":{"refresh":{"enabled":true}}}}))
        .write(&f.sandbox, Some(f.session.git_root()));
    f.session = Session::open_at_git_root(
        f.session.working_dir().to_path_buf(),
        f.session.git_root().to_path_buf(),
        SessionOpenOptions {
            env: Some(f.session.env().clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let binding = CredentialBinding::to_file(
        AuthPathResolver::at_home(f.sandbox.home()),
        f.sandbox.tmp().join("no-credential-read-expected"),
    );
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig::default(),
        binding,
        f.session.env(),
    );
    f.engines.credential_monitor = Some(monitor.clone());
    let outcome = run(&f, flags(true, false)).await.unwrap();
    assert_eq!(outcome.authentication, AuthenticationCheck::NotChecked);
    assert!(!f.marker.exists());
    assert_eq!(monitor.live_lease_count(), 0);
    assert!(
        monitor.status().is_empty(),
        "no refresh authority may be enrolled"
    );
    drop(f);
    drop(monitor);
}

/// Exercise the actual workflow factory at both authorized synchronous
/// triggers. Agent execution is an owned fake Docker CLI, never a host agent.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn workflow_pre_step_and_post_exit_recovery_keep_the_sanctioned_ping() {
    use awman::command::dispatch::Dispatch;
    use awman::engine::{
        auth::credential::CredentialBinding,
        credential_refresh::{CredentialRefreshMonitor, MonitorConfig},
    };
    use std::time::Duration;
    let s = support::sandbox(
        "command::prepare_only::workflow_pre_step_and_post_exit_recovery_keep_the_sanctioned_ping",
    );
    let repo = RepoBuilder::new(&s, "workflow-ping")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(".awman/Dockerfile.claude", "FROM scratch\n")
        .file(
            "workflow.toml",
            "[[steps]]\nname = 'owned-step'\nprompt = 'private task must stay in the container'\n",
        )
        .init();
    ConfigTree::new()
        .global(json!({"runtime":{"kind":"docker"},"agents":{"auth":{"mode":"keychain","refresh":{"enabled":true}}}}))
        .write(&s, Some(repo.path()));
    let source = s.home().join(".claude/.credentials.json");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(
        &source,
        r#"{"claudeAiOauth":{"accessToken":"owned-workflow-secret","expiresAt":1}}"#,
    )
    .unwrap();
    let ping = s.install_fake(support::fake_cli_exe(), "claude");
    ping.on(&[]).stdout("hello\n");
    let docker = s.install_fake(support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker.on(&[]);
    docker.on(&["image", "inspect"]);
    docker
        .on(&["image", "inspect", "--format", "{{.Id}}"])
        .stdout("sha256:owned\n");
    docker
        .on(&[
            "image",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
        ])
        .stdout("HOME=/home/awman\n");
    docker.on(&["run"]).stderr("401 Unauthorized\n").exit(1);
    let mut entries = s.env().clone();
    // General CLI isolation recognizes owned fakes beneath its private temp
    // root. Include bin as well as tmp without opting into a real daemon.
    entries.set("TMPDIR", s.root());
    let env = awman::data::config::env::EnvSnapshot::from_instance_env(
        entries
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect(),
        repo.path().to_path_buf(),
        vec![],
    );
    assert_eq!(
        awman::engine::host_cli::program_in(&env, "docker"),
        docker.exe()
    );
    let session = Session::open_at_git_root(
        repo.path().to_path_buf(),
        repo.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut engines = Engines::for_env(env.clone()).unwrap();
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig {
            tick_interval: Duration::from_secs(3600),
            ..Default::default()
        },
        CredentialBinding::to_file(AuthPathResolver::at_home(s.home()), &source),
        &env,
    );
    engines.credential_monitor = Some(monitor.clone());
    // Keep the independent periodic trigger out of this synchronous-trigger
    // test. The monitor's real lease/tick journey is covered in engine tests.
    engines.agent_engine = Arc::new(AgentEngine::new(
        engines.overlay_engine.clone(),
        engines.runtime.clone(),
    ));
    let args = vec![
        "--headless".into(),
        "--auto".into(),
        "--yolo=false".into(),
        "--agent".into(),
        "claude".into(),
        repo.path().join("workflow.toml").display().to_string(),
    ];
    let bus = EventBus::new(1024);
    let mut events = bus.subscribe_log();
    let cancel = tokio_util::sync::CancellationToken::new();
    let frontend = ApiDispatchFrontend::new("exec workflow", &args, bus.sender());
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session)),
        engines,
    )
    .with_cancellation(cancel.clone());
    // After the auth retry also fails, the API starts its ordinary failure
    // countdown. Observe that boundary to prove the auth retry guard stopped
    // another immediate recovery, then cancel instead of waiting for an
    // unrelated automatic workflow retry.
    let observe = async {
        awman_test_support::wait::wait_for_async(
            "ordinary failure countdown after the single auth recovery",
            Duration::from_secs(8),
            || {
                std::future::ready({
                    std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
                    event.payload,
                    awman::data::execution_event::EventPayload::StatusMessage { phase, message }
                        if phase == "yolo_countdown" && message.contains("retrying after failure")
                )).then_some(())
                })
            },
        )
        .await;
        cancel.cancel();
    };
    // Keep the production command's large debug future off the test stack
    // before combining it with the independent event observer.
    let run = Box::pin(dispatch.run_command(&["exec", "workflow"]));
    let (result, ()) = tokio::join!(tokio::time::timeout(Duration::from_secs(12), run), observe,);
    let result = result.expect("workflow cancellation must finish inside the component deadline");
    let runs: Vec<_> = docker
        .calls()
        .into_iter()
        .filter(|c| c.argv.first().is_some_and(|a| a == "run"))
        .collect();
    assert_eq!(
        runs.len(),
        2,
        "one initial run and one auth retry; result={result:?}"
    );
    assert_eq!(
        ping.calls().len(),
        3,
        "pre-step, post-exit recovery, retry pre-step; result={result:?}"
    );
    assert_eq!(monitor.live_lease_count(), 0);
    for call in ping.calls() {
        assert_eq!(&call.argv[..3], ["--model", "haiku", "--print"]);
        assert!(awman::engine::ready::GREETINGS.contains(&call.argv[3].as_str()));
        assert_eq!(call.argv.len(), 4, "no task content in ping argv");
        assert!(!call.cwd.starts_with(repo.path()));
        assert_eq!(call.env["HOME"], s.home().to_string_lossy());
    }
    drop(monitor);
    s.finish();
}
