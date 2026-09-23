//! `SquadDaemonHandles` — the Layer 2 bootstrap of the squad daemon.
//!
//! WI 0113 F-02 (decision Q4). Layer 2 collects the flags, config and
//! `Engines`, runs runtime admission, constructs the evaluator and the local
//! gateway, and calls [`SquadDaemonEngine::bootstrap`]. What it hands to
//! Layer 3 is this bundle of typed handles: a router, a socket and an HTTP
//! status map are all a frontend has left to contribute.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::command::commands::squad::commands::SquadServeConfig;
use crate::command::commands::squad::evaluation::LocalTaskEvaluator;
use crate::command::commands::squad::gateway::{DaemonStatus, LocalTaskGateway, TaskGateway};
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::Env;
use crate::data::fs::{DataPaths, SquadPaths};
use crate::data::session::{SessionId, SessionOpenOptions};
use crate::data::session_manager::{ManagedSession, SessionManager};
use crate::data::workflow_state::WorkflowState;
use crate::engine::squad::{SquadDaemonDeps, SquadDaemonEngine, TaskEvaluator};

/// What a workflow lookup found, so the transport maps a state rather than a
/// chain of `Option`s onto HTTP.
pub enum SquadWorkflowLookup {
    Found(Box<WorkflowState>),
    /// No task by that name exists.
    TaskNotFound,
    /// The task exists but has no run with workflow state to report.
    NoWorkflow,
}

/// The bootstrapped daemon, ready for a listener.
pub struct SquadDaemonHandles {
    engine: SquadDaemonEngine,
    gateway: Arc<LocalTaskGateway>,
    sessions: Arc<SessionManager>,
    session_id: SessionId,
    engines: Engines,
}

impl SquadDaemonHandles {
    /// Bring the daemon up: admission, engine bootstrap, gateway, session.
    ///
    /// Runtime admission is deliberately first — before the engine opens any
    /// database — because it is a policy about which tier squad supports, and
    /// a refusal must leave no file behind. `SquadDaemonEngine::bootstrap`
    /// asserts the same rule off `Capabilities::squad_supported`.
    pub async fn bootstrap(
        config: SquadServeConfig,
        engines: Engines,
        evaluator: Arc<dyn TaskEvaluator>,
    ) -> Result<Self, CommandError> {
        require_container_tier(&engines)?;

        let env = Env::from_process();
        let squad_paths = SquadPaths::from_env(&env)?;
        let data_paths = DataPaths::from_env(&env)?;

        let engine = SquadDaemonEngine::bootstrap(SquadDaemonDeps {
            runtime: engines.runtime.clone(),
            squad_paths: squad_paths.clone(),
            data_paths,
            env,
            evaluator,
            port: config.port,
            dangerously_skip_auth: config.dangerously_skip_auth,
        })
        .await?;

        let gateway = Arc::new(LocalTaskGateway::new(
            engine.store(),
            engines.clone(),
            engine.scheduler_handle(),
            squad_paths,
            engine.env_state(),
        ));
        // Compute `required_env` once at boot so the first client to check
        // coverage learns what this daemon wants — including for tasks created
        // before it started — without waiting for a task mutation. Best-effort:
        // an advisory list must never fail a daemon start.
        if let Err(error) = gateway.refresh_required_env().await {
            tracing::debug!(error = %error, "squad env: initial required_env refresh failed");
        }

        let cwd = std::env::current_dir().map_err(|error| {
            CommandError::Other(format!("cannot resolve squad working directory: {error}"))
        })?;
        let sessions = Arc::new(SessionManager::in_memory());
        let session_id = sessions.open_or_create(cwd, SessionOpenOptions::default())?;

        Ok(Self {
            engine,
            gateway,
            sessions,
            session_id,
            engines,
        })
    }

    /// The production bootstrap: build the Layer 2 evaluator from the run
    /// frontends the caller's frontend supplies, then bootstrap.
    pub async fn bootstrap_with_frontends(
        config: SquadServeConfig,
        engines: Engines,
        frontends: Arc<dyn crate::command::commands::squad::evaluation::SquadRunFrontends>,
    ) -> Result<Self, CommandError> {
        let evaluator = Arc::new(LocalTaskEvaluator::new(
            engines.clone(),
            Env::from_process(),
            frontends,
        ));
        Self::bootstrap(config, engines, evaluator).await
    }

    /// The gateway the daemon's own command route dispatches through.
    pub fn gateway(&self) -> Arc<dyn TaskGateway> {
        self.gateway.clone()
    }

    /// The session every dispatched command runs against.
    pub fn session(&self) -> ManagedSession {
        self.sessions
            .get(&self.session_id)
            .expect("squad daemon session must be registered")
    }

    /// The daemon's shared session owner, for the HTTP transport state.
    pub fn session_manager(&self) -> Arc<SessionManager> {
        Arc::clone(&self.sessions)
    }

    /// The engine bundle Dispatch needs.
    pub fn engines(&self) -> &Engines {
        &self.engines
    }

    /// The address the daemon asked to listen on.
    pub fn bind_addr(&self) -> SocketAddr {
        self.engine.bind_addr()
    }

    /// Bearer-auth state, as the daemon engine resolved it at Layer 1.
    ///
    /// There is one `AuthMode` now (WI 0114 F-14), so squad's bearer check is
    /// API mode's by construction and the conversion this used to perform is
    /// gone.
    pub fn auth_mode(&self) -> crate::engine::auth::AuthMode {
        self.engine.auth_mode().clone()
    }

    /// Record the address a listener actually bound and return the endpoint
    /// URL. Called from the bind hook, which is the only place the kernel's
    /// choice of port is known.
    pub fn publish_endpoint(&self, addr: SocketAddr) -> Result<String, CommandError> {
        Ok(self.engine.publish_endpoint(addr)?)
    }

    /// Daemon status for the read route, with the bound endpoint filled in.
    pub async fn status(&self, bound_addr: Option<String>) -> Result<DaemonStatus, CommandError> {
        let mut status = self.gateway.status().await?;
        status.bound_addr = bound_addr;
        Ok(status)
    }

    /// The workflow state of a task's running run, if it has one.
    pub async fn workflow_state(&self, name: &str) -> Result<SquadWorkflowLookup, CommandError> {
        // Preserve the route's distinction between an unknown task and a task
        // with no active workflow while delegating the actual state lookup to
        // the Layer 2 gateway.
        match self.gateway.get(name).await {
            Ok(_) => {}
            Err(CommandError::Other(_)) => return Ok(SquadWorkflowLookup::TaskNotFound),
            Err(error) => return Err(error),
        }
        match self.gateway.workflow_state(name).await? {
            Some(workflow) => Ok(SquadWorkflowLookup::Found(Box::new(workflow))),
            None => Ok(SquadWorkflowLookup::NoWorkflow),
        }
    }

    /// Stop the scheduler and drain its in-flight evaluations. Idempotent, and
    /// callable through a shared reference so the transport need not own the
    /// handles exclusively to shut the daemon down.
    pub async fn shutdown(&self) {
        self.engine.shutdown().await;
    }
}
