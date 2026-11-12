//! One daemon's runtime and database handles. SQLite WAL serialises writes
//! from both stores on the same database; no feature opens its own copy.
use super::executor::SharedExecutor;
use crate::data::{
    config::env::EnvSnapshot,
    fs::{
        api_db::SqliteSessionStore, daemon_paths::DaemonPaths, data_paths::DataPaths,
        task_store::TaskStore,
    },
};
/// The daemon's terminal hub (WI 0141): one per process, shared by every
/// feature's PTYs. Re-exported here because 0140 defined the `Core` slot.
pub use crate::engine::terminal_hub::{TerminalHub, TerminalHubConfig};
use crate::engine::{
    agent::AgentEngine, agent_runtime::AgentRuntimeEngine, auth::AuthEngine,
    container::ContainerRuntime, credential_refresh::CredentialRefreshMonitor, error::EngineError,
    git::GitEngine, overlay::OverlayEngine, sandbox::SandboxRuntime,
};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
pub struct CoreDeps {
    pub env: EnvSnapshot,
    pub data_paths: DataPaths,
    pub daemon_paths: DaemonPaths,
    pub runtime: Arc<dyn AgentRuntimeEngine>,
    pub container_runtime: Option<Arc<ContainerRuntime>>,
    pub sandbox_runtime: Option<Arc<SandboxRuntime>>,
    pub auth_engine: Arc<AuthEngine>,
    pub overlay_engine: Arc<OverlayEngine>,
    pub git_engine: Arc<GitEngine>,
    pub agent_engine: Arc<AgentEngine>,
    pub credential_monitor: Option<Arc<CredentialRefreshMonitor>>,
    pub max_concurrent_jobs: usize,
    pub terminal_hub: Arc<TerminalHub>,
}
pub struct Core {
    deps: CoreDeps,
    session_store: Arc<SqliteSessionStore>,
    task_store: Arc<TaskStore>,
    executor: Arc<SharedExecutor>,
    shutdown: CancellationToken,
    started_at: Instant,
}
impl Core {
    pub fn open(deps: CoreDeps) -> Result<Arc<Self>, EngineError> {
        deps.daemon_paths.ensure_root()?;
        let legacy = crate::data::fs::api_paths::ApiPaths::from_env(&deps.env)?;
        deps.data_paths.migrate_legacy_db(legacy.root())?;
        let session_store = Arc::new(SqliteSessionStore::open_at(&deps.data_paths.db_path())?);
        Self::with_session_store(deps, session_store)
    }
    /// Assemble a core around an existing store. Embedded callers can share
    /// a database connection, including an in-memory store, without opening
    /// a second database or running filesystem migration discovery.
    pub fn with_session_store(
        deps: CoreDeps,
        session_store: Arc<SqliteSessionStore>,
    ) -> Result<Arc<Self>, EngineError> {
        let task_store = Arc::new(TaskStore::from_connection(
            session_store.shared_connection(),
        ));
        task_store.migrate()?;
        let executor = SharedExecutor::new(deps.max_concurrent_jobs);
        Ok(Arc::new(Self {
            deps,
            session_store,
            task_store,
            executor,
            shutdown: CancellationToken::new(),
            started_at: Instant::now(),
        }))
    }
    pub fn env(&self) -> &EnvSnapshot {
        &self.deps.env
    }
    pub fn data_paths(&self) -> &DataPaths {
        &self.deps.data_paths
    }
    pub fn daemon_paths(&self) -> &DaemonPaths {
        &self.deps.daemon_paths
    }
    pub fn runtime(&self) -> &Arc<dyn AgentRuntimeEngine> {
        &self.deps.runtime
    }
    pub fn container_runtime(&self) -> Option<&Arc<ContainerRuntime>> {
        self.deps.container_runtime.as_ref()
    }
    pub fn sandbox_runtime(&self) -> Option<&Arc<SandboxRuntime>> {
        self.deps.sandbox_runtime.as_ref()
    }
    pub fn auth_engine(&self) -> &Arc<AuthEngine> {
        &self.deps.auth_engine
    }
    pub fn overlay_engine(&self) -> &Arc<OverlayEngine> {
        &self.deps.overlay_engine
    }
    pub fn git_engine(&self) -> &Arc<GitEngine> {
        &self.deps.git_engine
    }
    pub fn agent_engine(&self) -> &Arc<AgentEngine> {
        &self.deps.agent_engine
    }
    pub fn credential_monitor(&self) -> Option<&Arc<CredentialRefreshMonitor>> {
        self.deps.credential_monitor.as_ref()
    }
    pub fn session_store(&self) -> &Arc<SqliteSessionStore> {
        &self.session_store
    }
    pub fn task_store(&self) -> &Arc<TaskStore> {
        &self.task_store
    }
    pub fn executor(&self) -> &Arc<SharedExecutor> {
        &self.executor
    }
    pub fn terminal_hub(&self) -> &Arc<TerminalHub> {
        &self.deps.terminal_hub
    }
    pub fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }
    pub fn started_at(&self) -> Instant {
        self.started_at
    }
}
