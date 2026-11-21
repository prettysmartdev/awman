//! Process startup and admission exercise the real daemon composition, with a
//! runtime fake that cannot allocate an agent or touch a VM/provider.
use crate::support;
use awman::command::{
    commands::api_server::event_bus::EventBus,
    daemon_runtime::{DaemonBootstrap, DaemonRuntime},
    dispatch::{Dispatch, Engines},
    error::CommandError,
};
use awman::data::{
    config::{env::EnvSnapshot, ConfigFile, ConfigOverrides, EffectiveConfig, FileKind},
    daemon_state::StopMode,
    message::UserMessageSink,
    session::{AgentHandle, Session, SessionOpenOptions},
};
use awman::engine::{
    agent_runtime::{
        execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
        ReadyAgentOptions, ResolvedAgentOptions,
    },
    container::BuildRequest,
    daemon::core::{TerminalHub, TerminalHubConfig},
    error::EngineError,
};
use awman::frontend::api::{command_frontend::ApiDispatchFrontend, ApiFrontendFactory};
use awman_test_support::TestSandbox;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct Runtime(&'static str);
impl AgentRuntimeEngine for Runtime {
    fn runtime_name(&self) -> &'static str {
        self.0
    }
    fn display_name(&self) -> &'static str {
        "process config fake"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: true,
            image_acquisition: ImageAcquisition::Build,
            per_launch_vm_settings: true,
            reports_resource_stats: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("refusal must precede agent allocation")
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
        panic!("no agent")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("no agent")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no agent")
    }
    fn ready_agent(
        &self,
        _: &str,
        _: ReadyAgentOptions,
        _: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        panic!("no image allocation")
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        panic!("no image probe")
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        panic!("no image probe")
    }
    fn build_image(&self, _: &BuildRequest, _: &mut dyn FnMut(&str)) -> Result<(), EngineError> {
        panic!("no image allocation")
    }
}

fn bootstrap(env: EnvSnapshot, set: &[String]) -> DaemonBootstrap<ApiFrontendFactory> {
    DaemonBootstrap {
        env,
        set: ConfigOverrides::parse_set_args(set).unwrap(),
        squad_run_frontends: awman::frontend::squad::unattended::UnattendedFrontends::shared(),
        push_frontend_factory: ApiFrontendFactory,
        terminal_hub: Arc::new(TerminalHub::new(TerminalHubConfig::default())),
        engine_presentation: Arc::new(awman::command::instance::frontend::PlainPresentation),
    }
}
fn write_config(s: &TestSandbox, kind: &str) {
    let path = awman::data::config::global_config_path(&support::env_snapshot(s)).unwrap();
    let value = json!({"runtime":{"kind":kind},"daemon":{"features":{"engine":{"enabled":true},"squad":{"envPersistence":"none"}}}});
    let config = ConfigFile::from_value(value, FileKind::Global).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
}
fn fake_engines(config: &EffectiveConfig) -> Result<Engines, EngineError> {
    let name = config.runtime_selection().unwrap().as_str();
    Engines::with_process_runtime(config, Arc::new(Runtime(name)), None, None)
}
async fn startup_override(s: &TestSandbox, from: &str, to: &'static str) {
    write_config(s, from);
    let state = s.root().join("bound-builtin");
    let set = vec![
        format!("runtime.kind={to}"),
        "agents.auth.refresh.enabled=false".into(),
        format!("runtime.builtin.stateDir={}", state.display()),
    ];
    let (runtime, claim) = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        DaemonRuntime::bootstrap_with_engine_factory(
            bootstrap(support::env_snapshot(s), &set),
            |config| {
                assert_eq!(config.runtime_selection().unwrap().as_str(), to);
                assert!(!config.auth_refresh().enabled);
                // Later file changes cannot change the graph or displayed startup
                // config. Bootstrap must reuse the resolved config it hands us.
                std::fs::write(
                    awman::data::config::global_config_path(config.env()).unwrap(),
                    b"{changed after resolution",
                )
                .unwrap();
                eprintln!("startup factory: {to}");
                fake_engines(config)
            },
        ),
    )
    .await
    .expect("daemon bootstrap deadline")
    .unwrap();
    eprintln!("bootstrap completed: {to}");
    claim.finish();
    assert_eq!(runtime.core().runtime().runtime_name(), to);
    assert_eq!(runtime.engines().runtime.runtime_name(), to);
    assert_eq!(
        runtime.engines().process_binding().runtime_kind.as_str(),
        to
    );
    assert_eq!(runtime.config().runtime_selection().unwrap().as_str(), to);
    assert!(runtime.engines().credential_monitor.is_none());
    assert!(runtime.core().credential_monitor().is_none());
    assert!(!runtime.config().auth_refresh().enabled);
    if to == "builtin" {
        assert_eq!(
            runtime.engines().process_binding().builtin_state_dir,
            Some(state)
        );
    }
    eprintln!("shutting down: {to}");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.shutdown(StopMode::Now),
    )
    .await
    .expect("daemon shutdown deadline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_docker_to_builtin_binds_actual_backend_and_disables_refresh() {
    let s = support::sandbox("daemon::process_config::startup_docker_to_builtin_binds_actual_backend_and_disables_refresh");
    startup_override(&s, "docker", "builtin").await;
    s.finish();
}
#[tokio::test]
async fn startup_builtin_to_docker_binds_actual_backend_and_disables_refresh() {
    let s = support::sandbox("daemon::process_config::startup_builtin_to_docker_binds_actual_backend_and_disables_refresh");
    startup_override(&s, "builtin", "docker").await;
    s.finish();
}

#[tokio::test]
async fn invalid_config_unavailable_or_mismatched_backend_allocates_no_daemon() {
    let s = support::sandbox("daemon::process_config::invalid_config_unavailable_or_mismatched_backend_allocates_no_daemon");
    write_config(&s, "docker");
    let env = support::env_snapshot(&s);
    let mut b = bootstrap(env.clone(), &[]);
    b.set = ConfigOverrides::new(vec![("runtime.kind".into(), json!("typo"))]);
    let called = AtomicBool::new(false);
    let invalid = DaemonRuntime::bootstrap_with_engine_factory(b, |_| {
        called.store(true, Ordering::SeqCst);
        panic!("invalid effective config must fail before engine construction")
    })
    .await;
    assert!(invalid.is_err());
    assert!(!called.load(Ordering::SeqCst));
    let unavailable =
        DaemonRuntime::bootstrap_with_engine_factory(bootstrap(env.clone(), &[]), |_| {
            Err(EngineError::BuiltinRuntimeUnavailable {
                reason: "injected unavailable backend".into(),
            })
        })
        .await;
    assert!(matches!(
        unavailable,
        Err(CommandError::Engine(
            EngineError::BuiltinRuntimeUnavailable { .. }
        ))
    ));
    let mismatch =
        DaemonRuntime::bootstrap_with_engine_factory(bootstrap(env.clone(), &[]), |config| {
            Engines::with_process_runtime(config, Arc::new(Runtime("builtin")), None, None)
        })
        .await;
    assert!(mismatch
        .err()
        .unwrap()
        .to_string()
        .contains("constructed builtin"));
    let paths = awman::data::fs::DataPaths::from_env(&env).unwrap();
    assert!(!paths.daemon().pid_file().exists());
    assert!(!paths.db_path().exists());
    s.finish();
}

fn built_command(
    session: &Session,
    engines: &Engines,
    set: &[String],
) -> Result<awman::command::dispatch::BuiltCommand, CommandError> {
    let args: Vec<String> = set
        .iter()
        .flat_map(|s| ["--set".into(), s.clone()])
        .collect();
    use awman::command::dispatch::catalogue::FrontendKind;
    let build = |kind| {
        let bus = EventBus::new(16);
        let frontend = ApiDispatchFrontend::for_frontend("config show", &args, bus.sender(), kind);
        Dispatch::new(
            frontend,
            Arc::new(tokio::sync::RwLock::new(session.clone())),
            engines.clone(),
        )
        .build_command(&["config", "show"])
    };
    let api = build(FrontendKind::WORKER_LOCAL);
    let tui = build(FrontendKind::Tui);
    assert_eq!(
        api.is_ok(),
        tui.is_ok(),
        "TUI and API binding decisions must agree"
    );
    assert_eq!(
        api.as_ref().err().map(|e| e.code()),
        tui.as_ref().err().map(|e| e.code())
    );
    api
}

#[test]
fn dispatch_accepts_equal_effective_values_and_session_options_but_refuses_process_changes() {
    let s = support::sandbox("daemon::process_config::dispatch_accepts_equal_effective_values_and_session_options_but_refuses_process_changes");
    write_config(&s, "docker");
    let bound_dir = s.root().join("builtin-bound");
    let set = vec![
        "runtime.kind=builtin".into(),
        format!("runtime.builtin.stateDir={}", bound_dir.display()),
        "agents.auth.refresh.enabled=false".into(),
    ];
    let config = Engines::resolve_process_config(
        support::env_snapshot(&s),
        ConfigOverrides::parse_set_args(&set).unwrap(),
    )
    .unwrap();
    let engines = fake_engines(&config).unwrap();
    // This later session sees the global file's Docker/refresh defaults, not
    // the process startup argv. It must inherit the actual process baseline.
    let session = Session::open_at_directory_root(
        s.work().to_path_buf(),
        SessionOpenOptions {
            env: Some(support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(built_command(&session, &engines, &[]).is_ok());
    let mut same = set.clone();
    same.extend([
        "agents.auth.refresh.threshold=1200s".into(),
        "agents.auth.refresh.tick=1m".into(),
        "runtime.builtin.vcpus=4".into(),
        "runtime.baseImage=session-model".into(),
    ]);
    built_command(&session, &engines, &same)
        .unwrap_or_else(|e| panic!("matching overrides refused: {e}"));
    for (set, key) in [
        ("runtime.kind=docker".to_string(), "runtime.kind"),
        (
            format!(
                "runtime.builtin.stateDir={}",
                s.root().join("other").display()
            ),
            "runtime.builtin.stateDir",
        ),
        (
            "agents.auth.refresh.enabled=true".into(),
            "agents.auth.refresh.enabled",
        ),
        (
            "agents.auth.refresh.threshold=1m".into(),
            "agents.auth.refresh.threshold",
        ),
        (
            "agents.auth.refresh.tick=1s".into(),
            "agents.auth.refresh.tick",
        ),
    ] {
        let err = built_command(&session, &engines, &[set]).err().unwrap();
        assert!(
            matches!(&err, CommandError::ProcessScopedSetting { key: actual, .. } if actual == key),
            "{err}"
        );
        assert_eq!(err.code(), "process_scoped_setting");
        assert_eq!(err.http_status(), 409);
        assert!(err.to_string().contains("Restart the daemon or TUI"));
    }
    assert!(built_command(&session, &engines, &["runtime.kind=invalid".into()]).is_err());
    s.finish();
}

#[test]
fn repository_state_directory_conflict_keeps_its_typed_error_and_can_be_overridden_to_match() {
    let s = support::sandbox("daemon::process_config::repository_state_directory_conflict_keeps_its_typed_error_and_can_be_overridden_to_match");
    write_config(&s, "builtin");
    let repo = awman_test_support::fixtures::RepoBuilder::new(&s, "repository")
        .file(
            ".awman/config.json",
            serde_json::to_string(&json!({
                "runtime":{"builtin":{"stateDir":s.root().join("repo-state")}}
            }))
            .unwrap(),
        )
        .init();
    let env = support::env_snapshot(&s);
    let config = Engines::resolve_process_config(env.clone(), Default::default()).unwrap();
    let engines = fake_engines(&config).unwrap();
    let session = Session::open_at_git_root(
        repo.path().to_path_buf(),
        repo.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let err = built_command(&session, &engines, &[]).err().unwrap();
    assert!(matches!(
        err,
        CommandError::Engine(EngineError::BuiltinStateDirMismatch { .. })
    ));
    let matching = format!(
        "runtime.builtin.stateDir={}",
        config.builtin_state_dir().unwrap().display()
    );
    assert!(built_command(&session, &engines, &[matching]).is_ok());
    s.finish();
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_backend_fails_explicitly_for_config_command_and_tui_startup() {
    let s = support::sandbox("daemon::process_config::unsupported_backend_fails_explicitly_for_config_command_and_tui_startup");
    write_config(&s, "apple");
    let config =
        Engines::resolve_process_config(support::env_snapshot(&s), Default::default()).unwrap();
    for command in [vec![], vec!["config", "show"], vec!["exec", "prompt"]] {
        let result = Engines::detect_effective(
            awman::command::dispatch::catalogue::CommandCatalogue::get(),
            &config,
            &command,
        );
        assert!(matches!(
            result,
            Err(EngineError::BackendUnsupportedOnPlatform { .. })
        ));
    }
    s.finish();
}

#[tokio::test]
async fn session_overrides_keep_precedence_and_do_not_mutate_the_managed_session() {
    use awman::command::{commands::config::ConfigOutcome, dispatch::CommandOutcome};
    use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
    let s = support::sandbox("daemon::process_config::session_overrides_keep_precedence_and_do_not_mutate_the_managed_session");
    let repo = RepoBuilder::new(&s, "project").init();
    ConfigTree::new()
        .global(json!({"runtime":{"baseImage":"global"},"agents":{"auth":{"refresh":{"enabled":false}}}}))
        .repo(json!({"runtime":{"baseImage":"repository"}}))
        .write(&s, Some(repo.path()));
    let mut values = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    values.insert("AWMAN_RUNTIME_BASE_IMAGE".into(), "environment".into());
    let env = EnvSnapshot::from_full_env(values).with_invocation(repo.path().to_path_buf(), vec![]);
    let config = Engines::resolve_process_config(
        env.clone(),
        ConfigOverrides::parse_set_args(&["runtime.baseImage=startup".into()]).unwrap(),
    )
    .unwrap();
    let engines = fake_engines(&config).unwrap();
    let session = Session::open_at_git_root(
        repo.path().to_path_buf(),
        repo.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(env),
            overrides: ConfigOverrides::parse_set_args(&["runtime.baseImage=startup".into()])
                .unwrap(),
            ..Default::default()
        },
    )
    .unwrap();
    let managed = Arc::new(tokio::sync::RwLock::new(session));
    for (set, expected) in [
        (Some("runtime.baseImage=command"), "command"),
        (None, "startup"),
    ] {
        let mut args = vec!["runtime.baseImage".to_string()];
        if let Some(set) = set {
            args.splice(0..0, ["--set".into(), set.into()]);
        }
        let bus = EventBus::new(16);
        let frontend = ApiDispatchFrontend::new("config get", &args, bus.sender());
        let result = Dispatch::new(frontend, managed.clone(), engines.clone())
            .run_command(&["config", "get"])
            .await
            .unwrap();
        let CommandOutcome::Config(ConfigOutcome::Get(result)) = result else {
            panic!("expected typed config result")
        };
        assert_eq!(result.effective_value.as_deref(), Some(expected));
    }
    assert_eq!(
        managed.read().await.effective_config().value()["runtime"]["baseImage"],
        "startup"
    );
    s.finish();
}

/// Session admission uses the process's resolved default, even when the live
/// configuration contains no explicit builtin stateDir. This graph cannot
/// allocate an image, agent or VM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_default_state_directory_is_retained_during_session_admission() {
    let s = support::sandbox("daemon::process_config::builtin_default_state_directory_is_retained_during_session_admission");
    let repo = awman_test_support::fixtures::RepoBuilder::new(&s, "repository").init();
    let env = support::env_snapshot(&s);
    let path = awman::data::config::global_config_path(&env).unwrap();
    let config = ConfigFile::from_value(
        json!({"runtime":{"kind":"builtin"},
        "agents":{"auth":{"refresh":{"enabled":false}}},
        "daemon":{"features":{"engine":{"enabled":true},"squad":{"envPersistence":"none"},"worker":{
            "enabled":false,"push":{"enabled":true,"workDirs":{"allow":[repo.path()]}}}}}}),
        FileKind::Global,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let (runtime, claim) =
        DaemonRuntime::bootstrap_with_engine_factory(bootstrap(env, &[]), fake_engines)
            .await
            .unwrap();
    claim.finish();
    assert!(runtime
        .config()
        .config()
        .runtime
        .as_ref()
        .unwrap()
        .builtin
        .as_ref()
        .and_then(|b| b.state_dir.as_ref())
        .is_none());
    assert!(runtime
        .engines()
        .process_binding()
        .builtin_state_dir
        .is_some());
    let api = &runtime.worker().api;
    // Only exercise admission with this engine-only fake graph. It has no
    // container runtime and cannot start the worker feature or execute setup.
    let admitted = api
        .admission()
        .admit_session(
            awman::command::commands::api_server::admission::SessionSubmission {
                request: awman::command::session_create::SessionCreateRequest {
                    workdir: Some(repo.path().display().to_string()),
                    ..Default::default()
                },
                labels: Default::default(),
            },
            None,
            false,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("unchanged process binding must admit: {e:?}"));
    assert_eq!(
        admitted.plan.mount_authority.unwrap().root(),
        repo.path().canonicalize().unwrap()
    );
    api.lifecycle().close(&admitted.session_id).await.unwrap();
    runtime.shutdown(StopMode::Now).await;
}

#[test]
fn a_client_environment_conflict_survives_inheritance_and_is_refused() {
    let s = support::sandbox(
        "daemon::process_config::a_client_environment_conflict_survives_inheritance_and_is_refused",
    );
    let env = support::env_snapshot(&s);
    let config = Engines::resolve_process_config(env.clone(), ConfigOverrides::default()).unwrap();
    let engines = fake_engines(&config).unwrap();
    let mut values = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    values.insert("AWMAN_RUNTIME_KIND".into(), "builtin".into());
    let client = EnvSnapshot::from_instance_env(values.into_iter().collect(), s.work(), vec![]);
    let mut session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(client),
            ..Default::default()
        },
    )
    .unwrap();
    engines.inherit_process_settings(&mut session).unwrap();
    assert!(
        matches!(engines.validate_process_config(&session.effective_config()), Err(CommandError::ProcessScopedSetting { key, .. }) if key == "runtime.kind")
    );
    s.finish();
}
