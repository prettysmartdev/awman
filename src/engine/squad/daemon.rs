//! `SquadDaemonEngine` — everything the squad daemon is, minus its transport.
//!
//! WI 0113 F-02 (decision Q4): squad is not an architectural exception. The
//! daemon's runtime — the task store, the reconciliation it performs on a
//! crash, the scheduler and the endpoint sidecar it publishes — is an engine,
//! exactly like `ContainerRuntime` or `WorkflowEngine`. Layer 2 collects the
//! flags, config and `Engines` and calls [`SquadDaemonEngine::bootstrap`];
//! Layer 3 builds a router over the handles and binds a socket.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::data::config::env::EnvSnapshot;
use crate::data::config::global::GlobalConfig;
use crate::data::fs::daemon_env::{DaemonEnvStore, EnvPersistence, EnvPersistenceSetting, NoStore};
use crate::data::fs::daemon_process::ServerMeta;
use crate::data::fs::{DataPaths, SquadPaths, TaskStore};
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::auth::AuthMode;
use crate::engine::error::EngineError;
use crate::engine::squad::env_state::{DaemonEnvState, Salt};
use crate::engine::squad::env_store;
use crate::engine::squad::scheduler::{SchedulerStatus, SquadScheduler};
use crate::engine::squad::supervisor::squad_process;
use crate::engine::squad::TaskEvaluator;

/// The hint `awman squad start` prints when no key hash exists yet.
const SQUAD_KEY_REFRESH_HINT: &str = "awman squad start --refresh-key";

/// Everything the daemon engine needs, collected by Layer 2 before bootstrap.
///
/// `evaluator` is the Layer 2 `LocalTaskEvaluator` handed down through the
/// existing [`TaskEvaluator`] trait: evaluating a task drives
/// `ExecWorkflowCommand`, which is Layer 2's go-between role, so the engine
/// only ever calls the trait (Tenet 1).
pub struct SquadDaemonDeps {
    pub runtime: Arc<dyn AgentRuntimeEngine>,
    pub squad_paths: SquadPaths,
    pub data_paths: DataPaths,
    pub env: EnvSnapshot,
    pub evaluator: Arc<dyn TaskEvaluator>,
    /// The port the daemon was asked to listen on. `0` lets the kernel pick.
    pub port: u16,
    /// `--dangerously-skip-auth`: serve without checking a bearer token.
    pub dangerously_skip_auth: bool,
}

/// The running scheduler: its status handle, its cancellation token and the
/// task driving its tick loop.
///
/// The join handle is behind a `Mutex<Option<_>>` so [`SquadDaemonEngine::shutdown`]
/// takes `&self`: a transport that shares the engine behind an `Arc` (every
/// HTTP handler holds one) must still be able to stop the scheduler without
/// first proving it holds the last reference.
struct SchedulerHandle {
    status: Arc<Mutex<SchedulerStatus>>,
    shutdown: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// A bootstrapped squad daemon: store open, schema migrated, orphaned runs
/// reconciled, scheduler ticking.
pub struct SquadDaemonEngine {
    store: Arc<TaskStore>,
    scheduler: SchedulerHandle,
    squad_paths: SquadPaths,
    bind_addr: SocketAddr,
    auth_mode: AuthMode,
    dangerously_skip_auth: bool,
    /// The daemon's payload-environment metadata (WI 0116 §4), shared with the
    /// scheduler and with Layer 2's local gateway.
    env_state: Arc<DaemonEnvState>,
}

impl SquadDaemonEngine {
    /// Bring the daemon up to the point where only a listener is missing.
    ///
    /// The order below is load-bearing and is kept contiguous so it remains
    /// auditable: DB relocation → open/migrate store → orphan reconciliation →
    /// stray-container scan → scheduler → endpoint publication (which happens
    /// last, from [`Self::publish_endpoint`], because the bound port is only
    /// known once Layer 3 has a listener).
    ///
    /// Runtime admission runs in Layer 2 *before* this is called — it is a
    /// policy about which tier squad supports — and is asserted here off
    /// [`Capabilities::squad_supported`] so a future caller cannot skip it.
    ///
    /// [`Capabilities::squad_supported`]: crate::engine::agent_runtime::Capabilities::squad_supported
    pub async fn bootstrap(deps: SquadDaemonDeps) -> Result<Self, EngineError> {
        let SquadDaemonDeps {
            runtime,
            squad_paths,
            data_paths,
            env,
            evaluator,
            port,
            dangerously_skip_auth,
        } = deps;

        if !runtime.capabilities().squad_supported() {
            return Err(EngineError::SquadRuntimeUnsupported {
                runtime: runtime.runtime_name().to_string(),
            });
        }

        // Relocate a database left under the pre-split API root before either
        // store is opened.
        let legacy_api_paths = crate::data::fs::ApiPaths::from_env(&env)?;
        let outcome = data_paths.migrate_legacy_db(legacy_api_paths.root())?;
        tracing::info!(migration = ?outcome, "squad database migration outcome");

        // Startup order is load-bearing: open the store, apply squad's idempotent
        // schema migration, then reconcile any run left `running` by a crash.
        let store = Arc::new(TaskStore::open(&data_paths.db_path())?);
        store.migrate()?;
        let reconciled = store.reconcile_orphaned_runs(chrono::Utc::now())?;
        if reconciled > 0 {
            tracing::warn!(count = reconciled, "squad reconciled orphaned runs");
        }

        // A stray pre-rename container is not owned by this daemon and is never
        // auto-cleaned. The note makes mid-rebuild development states explainable.
        if let Ok(strays) = runtime.list_running_with_name_prefix("awman-amie-") {
            if !strays.is_empty() {
                tracing::info!(
                    count = strays.len(),
                    "squad found stray pre-rename awman-amie containers; leaving them untouched"
                );
            }
        }

        // WI 0116 §5: resolve the env-persistence backend once, here, before
        // the scheduler can tick — but after everything that must happen for
        // the daemon to be a daemon at all. Both the probe and the load are
        // capped and best-effort: a locked keychain must not be able to delay
        // `listen`, and it is never an error that fails a start.
        let squad_config = GlobalConfig::load_with(&env)
            .unwrap_or_default()
            .squad
            .unwrap_or_default();
        let setting = squad_config.env_persistence_or_default();
        let resolved = tokio::task::spawn_blocking(move || env_store::resolve(&squad_config))
            .await
            .unwrap_or_else(|_| env_store::ResolvedEnvStore {
                store: Box::new(NoStore) as Box<dyn DaemonEnvStore>,
                fallback: None,
                probed: None,
            });
        let env_store::ResolvedEnvStore {
            store: env_store,
            fallback,
            probed,
        } = resolved;
        let persistence = match (&fallback, setting) {
            (Some(reason), _) => EnvPersistence::Unavailable(reason.to_string()),
            (None, EnvPersistenceSetting::None) => EnvPersistence::None,
            (None, EnvPersistenceSetting::Keychain) => EnvPersistence::Keychain,
        };
        let env_state = Arc::new(DaemonEnvState::new(env_store, persistence, Salt::random()));
        // Exactly one warning per daemon lifetime, at startup only. A daemon
        // writes on every push, so warning per attempt would bury the very log
        // `awman squad logs` prints. An explicit `envPersistence: "none"`
        // produces no reason and therefore no line: a choice is never nagged
        // about.
        if let Some(reason) = fallback {
            tracing::warn!("{}", reason.startup_warning());
        }
        {
            // The probe above already read the item, and `probed` carries what
            // it read. That keeps startup to one capped keychain call rather
            // than two — both of which happen before `listen`, so on a slow
            // keychain two of them could exceed the supervisor's ten-second
            // wait and make `ensure_running` report a daemon that was seconds
            // from being up. `spawn_blocking` stays: `probed` is `None`
            // whenever the probe had no usable answer, and that path still
            // reaches the store.
            let loading = Arc::clone(&env_state);
            let loaded = tokio::task::spawn_blocking(move || loading.load_from_store(probed))
                .await
                .unwrap_or(None);
            if let Some(loaded) = loaded {
                if !loaded.is_empty() {
                    tracing::info!(
                        names = ?loaded.names(),
                        "squad env: restored values from the OS keychain"
                    );
                }
            }
        }

        let scheduler =
            SquadScheduler::new(store.clone(), squad_paths.clone(), evaluator, env.clone())
                .with_runtime(runtime.clone())
                .with_env_state(Arc::clone(&env_state));
        let status = scheduler.status_handle();

        let auth_mode = AuthMode::resolve_for_daemon(
            &squad_paths.daemon(),
            dangerously_skip_auth,
            SQUAD_KEY_REFRESH_HINT,
        )?;

        let shutdown = CancellationToken::new();
        let task = tokio::spawn(scheduler.run(shutdown.clone()));

        Ok(Self {
            store,
            scheduler: SchedulerHandle {
                status,
                shutdown,
                task: Mutex::new(Some(task)),
            },
            squad_paths,
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            auth_mode,
            dangerously_skip_auth,
            env_state,
        })
    }

    /// The daemon's task store.
    pub fn store(&self) -> Arc<TaskStore> {
        self.store.clone()
    }

    /// The daemon's payload-environment state, for Layer 2's local gateway.
    pub fn env_state(&self) -> Arc<DaemonEnvState> {
        Arc::clone(&self.env_state)
    }

    /// The scheduler's shared liveness counters, for the daemon's status route.
    pub fn scheduler_handle(&self) -> Arc<Mutex<SchedulerStatus>> {
        Arc::clone(&self.scheduler.status)
    }

    /// The address the daemon asked for. Port `0` means "whatever the kernel
    /// gives us"; the real one reaches [`Self::publish_endpoint`].
    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    /// Whether requests must carry a bearer key, and the hash to check against.
    pub fn auth_mode(&self) -> &AuthMode {
        &self.auth_mode
    }

    /// Write the endpoint sidecar for the address a listener actually bound.
    ///
    /// The last step of bootstrap, deferred because the port is only known
    /// once Layer 3 has bound. Returns the endpoint URL so the caller can log
    /// and serve it without re-deriving the format.
    pub fn publish_endpoint(&self, addr: SocketAddr) -> Result<String, EngineError> {
        squad_process(&self.squad_paths).write_meta(&ServerMeta {
            port: addr.port(),
            bind_ip: "127.0.0.1".into(),
            scheme: "http".into(),
            auth_disabled: self.dangerously_skip_auth,
        })?;
        Ok(format!("http://{addr}"))
    }

    /// Stop the scheduler and wait for its in-flight evaluations to drain.
    pub async fn shutdown(&self) {
        self.scheduler.shutdown.cancel();
        let task = self
            .scheduler
            .task
            .lock()
            .expect("squad scheduler task mutex poisoned")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::data::config::env::{AWMAN_CONFIG_HOME, AWMAN_SQUAD_ROOT};
    use crate::data::fs::task_store::{MountScope, Task, TaskStatus};
    use crate::engine::agent_runtime::{Capabilities, DindSupport};
    use crate::engine::squad::{EvaluationOutcome, EvaluationRequest};

    /// A container-tier runtime that touches nothing. `bootstrap` only ever
    /// asks it for its capabilities and its stray-container list.
    struct FakeContainerRuntime {
        caps: Capabilities,
    }

    impl FakeContainerRuntime {
        fn container() -> Self {
            Self {
                caps: Capabilities {
                    arbitrary_env_vars: true,
                    arbitrary_host_mounts: true,
                    cpu_limits: true,
                    per_resource_stats: true,
                    persistent_lifecycle: false,
                    kit_declarative: false,
                    dind: DindSupport::OnRequest,
                    host_paths_visible: true,
                    session_label_supported: true,
                    has_image_store: true,
                },
            }
        }

        fn sandbox() -> Self {
            Self {
                caps: Capabilities {
                    arbitrary_env_vars: false,
                    arbitrary_host_mounts: false,
                    cpu_limits: false,
                    per_resource_stats: false,
                    persistent_lifecycle: true,
                    kit_declarative: true,
                    dind: DindSupport::Always,
                    host_paths_visible: false,
                    session_label_supported: false,
                    has_image_store: false,
                },
            }
        }
    }

    impl AgentRuntimeEngine for FakeContainerRuntime {
        fn ready_agent(
            &self,
            _agent: &str,
            _opts: crate::engine::agent_runtime::ReadyAgentOptions,
            _sink: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<(), crate::engine::error::EngineError> {
            Ok(())
        }
        fn image_exists(&self, _tag: &str) -> Result<bool, crate::engine::error::EngineError> {
            Ok(true)
        }
        fn image_home_dir(
            &self,
            _tag: &str,
        ) -> Result<Option<String>, crate::engine::error::EngineError> {
            Ok(None)
        }
        fn build_image(
            &self,
            _tag: &str,
            _dockerfile: &std::path::Path,
            _context: &std::path::Path,
            _no_cache: bool,
            _on_line: &mut dyn FnMut(&str),
        ) -> Result<(), crate::engine::error::EngineError> {
            Ok(())
        }
        fn runtime_name(&self) -> &'static str {
            if self.caps.kit_declarative {
                "docker-sbx-experimental"
            } else {
                "docker"
            }
        }
        fn display_name(&self) -> &'static str {
            "Fake Runtime"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(
            &self,
            _: crate::engine::agent_runtime::ResolvedAgentOptions,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("bootstrap never builds an agent")
        }
        fn list_running(
            &self,
            _: &crate::data::session::Session,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn stats(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
            unimplemented!("bootstrap never reads stats")
        }
        fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
            Ok(())
        }
        fn exec_args(&self, _: &str, _: &str, _: &[&str], _: &[(&str, &str)]) -> Vec<String> {
            vec![]
        }
        fn attach(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("bootstrap never attaches")
        }
        fn list_running_with_name_prefix(
            &self,
            _: &str,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn cli_binary(&self) -> &'static str {
            "fake"
        }
    }

    struct NeverCalledEvaluator;

    #[async_trait::async_trait]
    impl TaskEvaluator for NeverCalledEvaluator {
        async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
            panic!("bootstrap must not evaluate a task");
        }
    }

    fn deps(root: &std::path::Path, runtime: Arc<dyn AgentRuntimeEngine>) -> SquadDaemonDeps {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, root.to_str().unwrap()),
            (AWMAN_SQUAD_ROOT, root.join("squad").to_str().unwrap()),
        ]);
        let squad_paths = SquadPaths::from_env(&env).unwrap();
        squad_paths.ensure_root().unwrap();
        let data_paths = DataPaths::from_env(&env).unwrap();
        data_paths.ensure_root().unwrap();
        SquadDaemonDeps {
            runtime,
            squad_paths,
            data_paths,
            env,
            evaluator: Arc::new(NeverCalledEvaluator),
            port: 0,
            // Auth off: the test asserts bootstrap, not key provisioning, and
            // a hash would otherwise have to be minted first.
            dangerously_skip_auth: true,
        }
    }

    fn task(name: &str) -> Task {
        let now = chrono::Utc::now();
        Task {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: "test task".into(),
            repo_scope: std::path::PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            overlays: Vec::new(),
            interval_secs: 3600,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        }
    }

    /// A first start on an empty root: the store is created and migrated, and
    /// the daemon comes up ready for a listener.
    #[tokio::test]
    async fn bootstrap_on_a_fresh_root_opens_and_migrates_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = deps(tmp.path(), Arc::new(FakeContainerRuntime::container()));
        let db_path = deps.data_paths.db_path();
        assert!(!db_path.exists(), "nothing exists before bootstrap");

        let engine = SquadDaemonEngine::bootstrap(deps).await.unwrap();

        assert!(db_path.exists(), "the store must be created");
        // Migration ran, so the schema is queryable.
        assert!(engine.store().list().unwrap().is_empty());
        assert_eq!(engine.bind_addr().port(), 0);
        assert!(matches!(
            engine.auth_mode(),
            crate::engine::auth::AuthMode::Disabled
        ));
        // The scheduler is running: its status handle exists and no tick has
        // been observed to fail.
        assert_eq!(engine.scheduler_handle().lock().unwrap().in_flight, 0);

        engine.shutdown().await;
    }

    /// A daemon killed mid-evaluation leaves a `running` run row. Bootstrap
    /// must close it out before the scheduler can consider the task again.
    #[tokio::test]
    async fn bootstrap_reconciles_a_run_left_running_by_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = deps(tmp.path(), Arc::new(FakeContainerRuntime::container()));

        // Seed a store with an orphaned run, exactly as a crash would leave it.
        let seeded = TaskStore::open(&deps.data_paths.db_path()).unwrap();
        seeded.migrate().unwrap();
        let task = task("triage");
        seeded.create(&task).unwrap();
        seeded
            .start_run(&task.id, None, chrono::Utc::now())
            .unwrap();
        assert!(seeded.running_run_for(&task.id).unwrap().is_some());
        drop(seeded);

        let engine = SquadDaemonEngine::bootstrap(deps).await.unwrap();

        assert!(
            engine.store().running_run_for(&task.id).unwrap().is_none(),
            "the orphaned run must be interrupted before the scheduler ticks"
        );
        let runs = engine.store().runs_for("triage", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].error.as_deref(),
            Some("daemon restarted while this run was in flight")
        );

        engine.shutdown().await;
    }

    /// A database left under the pre-split API root is relocated before
    /// either store is opened, so an upgrade keeps its tasks.
    #[tokio::test]
    async fn bootstrap_relocates_a_legacy_database_before_opening_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = deps(tmp.path(), Arc::new(FakeContainerRuntime::container()));

        let legacy_root = crate::data::fs::ApiPaths::from_env(&deps.env)
            .unwrap()
            .root()
            .to_path_buf();
        std::fs::create_dir_all(&legacy_root).unwrap();
        let legacy_db = legacy_root.join(
            deps.data_paths
                .db_path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
        );
        let seeded = TaskStore::open(&legacy_db).unwrap();
        seeded.migrate().unwrap();
        seeded.create(&task("legacy")).unwrap();
        drop(seeded);

        let new_db = deps.data_paths.db_path();
        assert!(!new_db.exists(), "the new location starts empty");

        let engine = SquadDaemonEngine::bootstrap(deps).await.unwrap();

        assert!(new_db.exists(), "the legacy database must be relocated");
        assert!(!legacy_db.exists(), "and must not be left behind");
        let tasks = engine.store().list().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "legacy");

        engine.shutdown().await;
    }

    /// The admission rule Layer 2 enforces before calling in is asserted here
    /// too, so no future caller can bootstrap squad onto a sandbox runtime.
    #[tokio::test]
    async fn bootstrap_refuses_a_runtime_that_cannot_mount_task_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = deps(tmp.path(), Arc::new(FakeContainerRuntime::sandbox()));
        let db_path = deps.data_paths.db_path();

        let error = match SquadDaemonEngine::bootstrap(deps).await {
            Ok(_) => panic!("the sandbox tier must be refused"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .starts_with("squad requires a container runtime."),
            "{error}"
        );
        assert!(!db_path.exists(), "the store must never be opened");
    }
}
