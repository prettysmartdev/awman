//! One production daemon core/listener for conformance tests and the TUI matrix.
//! Included in the library test harness as well as the integration harness.
#![allow(dead_code)]
use crate::command::{
    commands::api_server::{CommandCanceller, TokenRegistry},
    daemon_api::{worker::WorkerApi, DaemonApi},
    daemon_runtime::DaemonRuntime,
    dispatch::{catalogue::CommandCatalogue, Engines},
    instance::{
        frontend::PlainPresentation,
        host::{
            EngineInstanceHost, HostParts, InstanceOpener, OpenInstanceRequest, OpenedInstance,
        },
        EngineInstance, HostedOptions, InitialTabSpec, InstanceParts,
    },
    integrations::IntegrationRegistry,
};
use crate::data::{
    config::{env::EnvSnapshot, ConfigFile, ConfigLayers, EffectiveConfig, FileKind},
    daemon_state::FeatureName,
    frontend_protocol::*,
    fs::api_db::SqliteSessionStore,
    fs::{ApiPaths, AuthPathResolver},
    session::{Session, SessionOpenOptions, StaticGitRootResolver},
    session_manager::SessionManager,
    WorkflowStateStore,
};
use crate::engine::{
    agent::AgentEngine,
    auth::AuthEngine,
    container::ContainerRuntime,
    daemon::guard::{DaemonClaim, DaemonGuard},
    git::GitEngine,
    overlay::OverlayEngine,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

pub fn engines(root: &Path) -> Engines {
    let runtime = Arc::new(ContainerRuntime::docker());
    let auth = AuthPathResolver::at_home(root);
    let paths = ApiPaths::from_root(root.join("api"));
    paths.ensure_root().unwrap();
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth, paths)),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(root)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    }
}

struct Opener {
    engines: Engines,
    opened: Arc<Mutex<Vec<Arc<EngineInstance>>>>,
}
impl InstanceOpener for Opener {
    fn open(
        &self,
        request: OpenInstanceRequest,
    ) -> Result<OpenedInstance, crate::command::error::CommandError> {
        let cwd = request.env.cwd().to_path_buf();
        let session = Session::open(
            cwd.clone(),
            &StaticGitRootResolver::new(cwd),
            SessionOpenOptions {
                env: Some(request.env.clone()),
                ..Default::default()
            },
        )?;
        let env = request.env.clone();
        let instance = EngineInstance::new_hosted(
            InstanceParts {
                catalogue: request.catalogue,
                engines: if request.kind == FrontendKindWire::Cli {
                    Engines::build(&session)?
                } else {
                    self.engines.clone()
                },
                session_manager: Arc::new(SessionManager::in_memory(env.clone())),
                runtime: tokio::runtime::Handle::current(),
                env: Arc::new(move || env.clone()),
                presentation: Arc::new(PlainPresentation),
            },
            HostedOptions {
                id: request.id,
                kind: match request.kind {
                    FrontendKindWire::Tui | FrontendKindWire::Web => {
                        crate::command::dispatch::catalogue::FrontendKind::RELAYED_TUI
                    }
                    FrontendKindWire::Cli => {
                        crate::command::dispatch::catalogue::FrontendKind::RELAYED_CLI
                    }
                },
                hub: Some(request.hub),
                executor: request.executor,
            },
        );
        instance.set_input_available(request.input_available);
        self.opened.lock().unwrap().push(instance.clone());
        Ok(OpenedInstance {
            instance,
            initial: InitialTabSpec::Session {
                session,
                fatal_startup_error: true,
                pty_size: (request.term_size.cols, request.term_size.rows),
            },
            notices: vec![],
        })
    }
}

pub struct Daemon {
    pub api: Arc<DaemonApi>,
    pub root: PathBuf,
    pub socket: PathBuf,
    pub opened: Arc<Mutex<Vec<Arc<EngineInstance>>>>,
    _listener: crate::frontend::api::local::LocalListener,
    _claim: Option<DaemonClaim>,
    _dir: tempfile::TempDir,
}
impl Daemon {
    pub async fn start(registry: Arc<IntegrationRegistry>) -> Self {
        // Keep Unix socket paths short even when make's fixture root is long.
        #[cfg(unix)]
        let dir = tempfile::Builder::new()
            .prefix("ep-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(not(unix))]
        let dir = tempfile::Builder::new().prefix("ep-").tempdir().unwrap();
        let root = dir.path().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let templates = root.join("repo/aspec/work-items");
        std::fs::create_dir_all(&templates).unwrap();
        std::fs::write(
            templates.join("0000-template.md"),
            include_str!("../../aspec/work-items/0000-template.md"),
        )
        .unwrap();
        let env = EnvSnapshot::with_overrides([
            ("AWMAN_HOME", root.to_str().unwrap()),
            ("HOME", root.to_str().unwrap()),
        ]);
        let config = EffectiveConfig::resolve(
            ConfigLayers {
                global: ConfigFile::from_value(
                    serde_json::json!({"daemon":{"features":{"engine":{"allowEphemeral":true}}}}),
                    FileKind::Global,
                )
                .unwrap(),
                ..Default::default()
            },
            Default::default(),
            env.clone(),
        )
        .unwrap();
        let engines = engines(&root);
        let paths = ApiPaths::from_env(&env).unwrap();
        paths.ensure_root().unwrap();
        let store = Arc::new(SqliteSessionStore::open(paths.root()).unwrap());
        let lifecycle_store = crate::data::CommandLifecycleStore::open(store.clone()).unwrap();
        let setup_cancels = Arc::new(TokenRegistry::new());
        let canceller = CommandCanceller::new(
            store.clone(),
            lifecycle_store.clone(),
            Arc::new(TokenRegistry::new()),
            setup_cancels.clone(),
        );
        let worker = Arc::new(WorkerApi {
            live_config: std::sync::RwLock::new(Some(config.clone())),
            settings_tx: None,
            store,
            lifecycle_store,
            canceller,
            setup_cancels,
            paths,
            workdirs: vec![],
            workdir_roots: vec![],
            started_at: Instant::now(),
            task_handles: tokio::sync::Mutex::new(vec![]),
            engines: engines.clone(),
            sessions: Arc::new(SessionManager::in_memory(env)),
            event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            setup_buses: tokio::sync::Mutex::new(HashMap::new()),
        });
        let opened = Arc::new(Mutex::new(vec![]));
        let runtime = DaemonRuntime::for_engine_tests(
            worker,
            config,
            Some(HostParts {
                hub: Arc::new(crate::engine::terminal_hub::TerminalHub::new(
                    Default::default(),
                )),
                catalogue: CommandCatalogue::get(),
                registry,
                opener: Arc::new(Opener {
                    engines,
                    opened: opened.clone(),
                }),
                executor: None,
                build: env!("CARGO_PKG_VERSION").into(),
            }),
        );
        runtime
            .enable_ephemeral(FeatureName::Engine, Some("parity-suite".into()))
            .await
            .unwrap();
        let api = Arc::new(DaemonApi::new(runtime));
        let claim = DaemonGuard::new(api.runtime.core().daemon_paths().clone())
            .acquire(std::process::id())
            .unwrap();
        let socket = api.runtime.core().daemon_paths().local_endpoint();
        let (listener, bound) = crate::frontend::api::local::start_claimed(
            &claim,
            &socket,
            crate::frontend::api::routes::build_router_for_listener(
                api.clone(),
                crate::frontend::api::routes::ListenerPolicy::Local,
            ),
        )
        .unwrap();
        let transport = match bound.transport {
            crate::data::fs::daemon_sidecar::SidecarTransportKind::UnixSocket => {
                crate::command::daemon_runtime::LocalTransport::UnixSocket
            }
            crate::data::fs::daemon_sidecar::SidecarTransportKind::NamedPipe => {
                crate::command::daemon_runtime::LocalTransport::NamedPipe
            }
        };
        api.runtime
            .set_local_listener(transport, bound.address.clone());
        crate::data::fs::daemon_sidecar::ServerSidecar::current(
            std::process::id(),
            bound.sidecar_transport(),
        )
        .write(&api.runtime.core().daemon_paths().server_meta_file())
        .unwrap();
        claim.finish();
        Self {
            api,
            root,
            socket,
            opened,
            _listener: listener,
            _claim: None,
            _dir: dir,
        }
    }
    pub fn host(&self) -> &Arc<EngineInstanceHost> {
        self.api.runtime.engine_host()
    }
    pub fn hello(&self, role: FrontendRole) -> Hello {
        Hello {
            protocol: FRONTEND_SUBPROTOCOL.into(),
            build: env!("CARGO_PKG_VERSION").into(),
            kind: FrontendKindWire::Tui,
            role,
            annotations: Default::default(),
            cwd: self.root.join("repo"),
            argv: vec!["awman".into()],
            env: HelloEnv::from_pairs([
                ("HOME", self.root.display().to_string()),
                ("AWMAN_HOME", self.root.display().to_string()),
                ("PATH", "/usr/bin:/bin".into()),
                ("GITHUB_TOKEN", "secret".into()),
            ]),
            term_size: TermSize {
                cols: 100,
                rows: 30,
            },
            stdin_is_tty: true,
            stdout_is_tty: true,
            view: None,
            resume: None,
            launch_context: None,
        }
    }
    pub async fn remote(&self) -> Arc<crate::command::instance::remote::RemoteInstanceBackend> {
        self.remote_at(&self.root.join("repo")).await
    }
    pub async fn remote_at(
        &self,
        cwd: &Path,
    ) -> Arc<crate::command::instance::remote::RemoteInstanceBackend> {
        use crate::command::instance::remote::{RemoteInstanceBackend, RemoteOpen};
        let mut hello = self.hello(FrontendRole::Primary);
        hello.cwd = cwd.to_path_buf();
        RemoteInstanceBackend::connect(RemoteOpen {
            address: self.socket.clone(),
            pid: std::process::id(),
            hello,
            create: true,
            instance_id: None,
            catalogue: CommandCatalogue::get(),
            env: self.api.runtime.core().env().clone(),
        })
        .await
        .unwrap()
    }
}

/// Optional CI artifacts; never includes a Hello or environment snapshot.
pub fn save_artifact(name: &str, value: impl serde::Serialize) {
    let Ok(root) = std::env::var("AWMAN_TEST_PARITY_ARTIFACT_DIR") else {
        return;
    };
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        Path::new(&root).join(format!("{name}.json")),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
}

/// Keep subprocess probes on the executing inode even if another Cargo
/// invocation republishes this harness with unlink/relink while tests run.
pub fn copy_test_executable(destination: &Path) {
    #[cfg(target_os = "linux")]
    let source = PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let source = std::env::current_exe().unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::fs::copy(&source, destination) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(e) => panic!("copy executing harness: {e}"),
        }
    }
}

/// A concurrent fork can retain the copy's writer until it execs. Retry only
/// ETXTBSY: the target program has not run when exec refuses the busy inode.
pub fn output_test_executable(command: &mut std::process::Command) -> std::process::Output {
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match command.output() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result.expect("run copied test executable"),
        }
    }
}
