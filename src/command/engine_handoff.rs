//! The builtin-vs-daemon hand-off (WI 0142 Part B §1). Inputs: `tui.backend`
//! (`auto|daemon|builtin`, from config, `AWMAN_TUI_BACKEND` or `--set`), each
//! integration's `HostRequirement` from the registry, and the daemon's state
//! (sidecar + `GET /v1/status`, including the `awman.frontend.v1` version
//! handshake). `decide()` is the pure decision table; `EngineHandoff::resolve()`
//! acts on it (start the daemon in the background, enable `engine` ephemerally,
//! restart a stale idle daemon, ask before restarting a busy one) and applies
//! the fallback rules (`auto` → builtin with a one-line notice; `daemon` → exit
//! nonzero).
//!
//! Layer 2. Frontend entry points call it as wiring and construct the chosen
//! `InstanceBackend`; no frontend reads `tui.backend` itself.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.10 (D12).
//!
//! Layout: the pure core (`decide` and its input types) has no I/O. The
//! effects (sidecar read, status probe, spawn, PATCH, stop) sit behind the
//! [`DaemonLink`] seam; [`LocalDaemonLink`] is the real one and reuses
//! [`DaemonClient`] and `DaemonSupervisor`, so there is no second HTTP client
//! and no second daemon spawner.

use std::sync::Arc;
use std::time::Duration;

use crate::command::commands::daemon::actions::Request;
use crate::command::commands::daemon::client::DaemonClient;
use crate::command::daemon_api::core::StatusResponse;
use crate::command::error::CommandError;
use crate::command::integrations::{HostRequirement, IntegrationRegistry};
use crate::data::annotations::Annotations;
use crate::data::config::effective::EffectiveConfig;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::tui::TuiBackend;
use crate::data::daemon_state::FeatureState;
use crate::data::frontend_protocol::FRONTEND_SUBPROTOCOL;
use crate::data::fs::DataPaths;
use crate::engine::daemon::supervisor::DaemonSupervisor;
use crate::engine::remote::local::LocalEndpoint;

/// Key of the frontend protocol version in `GET /v1/status` `protocol`.
pub const STATUS_FRONTEND_KEY: &str = "frontend";
/// Exit code for "the daemon needs a restart and nobody confirmed it".
pub const EXIT_STALE_DAEMON: u8 = 2;
/// How long a freshly started or restarted daemon gets to answer.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

// ───────────────────────────── pure core ─────────────────────────────

/// State of the daemon's `engine` feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineProbe {
    Off { allow_ephemeral: bool },
    Ephemeral,
    Persistent,
    Draining,
}

/// What the sidecar and `GET /v1/status` say about the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonProbe {
    NotRunning,
    Unreachable(String),
    Running {
        pid: u32,
        build: String,
        frontend_protocol: Option<String>,
        engine: EngineProbe,
        running_jobs: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffInputs {
    pub backend: TuiBackend,
    /// Merged requirement of every enabled integration.
    pub requirement: HostRequirement,
    /// `daemon.features.engine.enabled` in the effective config.
    pub engine_configured_on: bool,
    pub daemon: DaemonProbe,
    pub this_build: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEnable {
    /// The daemon already serves the engine.
    AlreadyOn,
    /// `PATCH /v1/features/engine {enabled, ephemeral}`.
    Ephemeral,
    /// No usable daemon: start one with the engine on.
    StartDaemon,
}

/// What a failed hand-off does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Run builtin and show a one-line notice.
    Builtin,
    /// Exit nonzero.
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Builtin,
    Daemon {
        enable: EngineEnable,
        on_failure: Fallback,
    },
    /// The running daemon is from another build or protocol. Idle
    /// (`running_jobs == 0`) restarts silently; busy asks first. `then` is
    /// what to do once it is replaced.
    RestartStaleDaemon {
        running_jobs: usize,
        then: Box<Decision>,
        on_failure: Fallback,
    },
    /// The daemon cannot serve the engine; apply `on_failure`.
    Refused {
        reason: String,
        on_failure: Fallback,
    },
}

/// The 0142 §1 decision table plus the version handshake. Pure.
pub fn decide(inputs: &HandoffInputs) -> Decision {
    let (wants_daemon, on_failure) = match inputs.backend {
        TuiBackend::Builtin => return Decision::Builtin,
        TuiBackend::Daemon => (true, Fallback::Exit),
        TuiBackend::Auto => {
            let engine_on = inputs.engine_configured_on
                || matches!(
                    inputs.daemon,
                    DaemonProbe::Running {
                        engine: EngineProbe::Persistent,
                        ..
                    }
                );
            (
                inputs.requirement.requires_daemon() || engine_on,
                Fallback::Builtin,
            )
        }
    };
    if !wants_daemon {
        return Decision::Builtin;
    }
    match &inputs.daemon {
        DaemonProbe::NotRunning => Decision::Daemon {
            enable: EngineEnable::StartDaemon,
            on_failure,
        },
        DaemonProbe::Unreachable(why) => Decision::Refused {
            reason: format!("daemon is not reachable: {why}"),
            on_failure,
        },
        DaemonProbe::Running {
            build,
            frontend_protocol,
            engine,
            running_jobs,
            ..
        } => {
            let compatible = *build == inputs.this_build
                && frontend_protocol.as_deref() == Some(FRONTEND_SUBPROTOCOL);
            if !compatible {
                return Decision::RestartStaleDaemon {
                    running_jobs: *running_jobs,
                    then: Box::new(Decision::Daemon {
                        enable: EngineEnable::StartDaemon,
                        on_failure,
                    }),
                    on_failure,
                };
            }
            match engine {
                EngineProbe::Persistent | EngineProbe::Ephemeral => Decision::Daemon {
                    enable: EngineEnable::AlreadyOn,
                    on_failure,
                },
                EngineProbe::Off {
                    allow_ephemeral: true,
                } => Decision::Daemon {
                    enable: EngineEnable::Ephemeral,
                    on_failure,
                },
                EngineProbe::Off {
                    allow_ephemeral: false,
                } => Decision::Refused {
                    reason: "the daemon's engine is off and \
                             daemon.features.engine.allowEphemeral is false"
                        .into(),
                    on_failure,
                },
                EngineProbe::Draining => Decision::Refused {
                    reason: "the daemon's engine is shutting down".into(),
                    on_failure,
                },
            }
        }
    }
}

/// One-line fallback notice: `engine: unavailable: <reason>`.
pub fn fallback_notice(reason: &str) -> String {
    format!("engine: unavailable: {reason}")
}

/// The advice for a busy, incompatible daemon (TUI dialog text and CLI stderr).
pub fn restart_advice(running_jobs: usize) -> String {
    format!(
        "The awman daemon is from an older version and is running {running_jobs} job{}. \
         Restart it now? Running work will be stopped.",
        if running_jobs == 1 { "" } else { "s" }
    )
}

// ───────────────────────────── effects ─────────────────────────────

/// Why `PATCH /v1/features/engine` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnableError {
    /// `409 ephemeral_not_allowed`.
    NotAllowed,
    Other(String),
}

/// Every effect the driver performs on the daemon.
#[async_trait::async_trait]
pub trait DaemonLink: Send + Sync {
    /// Sidecar + `GET /v1/status`.
    async fn probe(&self) -> DaemonProbe;
    /// Start a background daemon with `engine` on. Serialised by 0140's
    /// startup lock; a lost race connects to the winner's daemon.
    async fn start(&self) -> Result<(), String>;
    async fn enable_engine_ephemeral(&self) -> Result<(), EnableError>;
    /// Stop the running daemon and wait for it to be gone.
    async fn stop(&self) -> Result<(), String>;
    fn endpoint(&self) -> Result<LocalEndpoint, String>;
}

/// Asked before restarting a daemon that has running work.
#[async_trait::async_trait]
pub trait HandoffPrompter: Send + Sync {
    /// TUI: a `YesNo` dialog. CLI: print [`restart_advice`] to stderr and
    /// return `false`.
    async fn confirm_daemon_restart(&self, running_jobs: usize) -> bool;
}

#[derive(Debug, Clone)]
pub enum BackendSelection {
    Builtin,
    Daemon {
        endpoint: LocalEndpoint,
        pid: u32,
        enabled: EngineEnable,
        /// The engine is on by config rather than ephemerally.
        persistent: bool,
        /// Integrations that required the daemon (for `describe`).
        integrations: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct HandoffResult {
    pub selection: BackendSelection,
    /// Merged detected annotations for `Hello.annotations` (empty on builtin).
    pub annotations: Annotations,
    /// One-line fallback notice (TUI status bar, CLI stderr).
    pub notice: Option<String>,
}

/// A hand-off that must end the process.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HandoffError {
    pub message: String,
    pub exit_code: u8,
}

impl HandoffError {
    fn failed(reason: &str) -> Self {
        Self {
            message: fallback_notice(reason),
            exit_code: 1,
        }
    }
}

pub struct EngineHandoff {
    config: EffectiveConfig,
    env: EnvSnapshot,
    registry: Arc<IntegrationRegistry>,
    link: Arc<dyn DaemonLink>,
    prompter: Arc<dyn HandoffPrompter>,
}

impl EngineHandoff {
    pub fn new(
        env: EnvSnapshot,
        config: EffectiveConfig,
        registry: Arc<IntegrationRegistry>,
        prompter: Arc<dyn HandoffPrompter>,
    ) -> Self {
        let link = Arc::new(LocalDaemonLink::new(env.clone(), config.clone()));
        Self::with_link(env, config, registry, link, prompter)
    }

    pub fn with_link(
        env: EnvSnapshot,
        config: EffectiveConfig,
        registry: Arc<IntegrationRegistry>,
        link: Arc<dyn DaemonLink>,
        prompter: Arc<dyn HandoffPrompter>,
    ) -> Self {
        Self {
            config,
            env,
            registry,
            link,
            prompter,
        }
    }

    /// Probe → decide → act → apply the fallback rules.
    pub async fn resolve(&self) -> Result<HandoffResult, HandoffError> {
        let backend = self.config.tui_backend();
        if backend == TuiBackend::Builtin {
            // `builtin` means the host integration does not run: no detect.
            return Ok(builtin(None));
        }
        let detected = self
            .registry
            .detect_all(&self.env, &self.config)
            .map_err(|e| HandoffError::failed(&e.to_string()))?;
        let inputs = HandoffInputs {
            backend,
            requirement: detected.requirement.clone(),
            engine_configured_on: self.config.daemon().engine.enabled,
            daemon: self.link.probe().await,
            this_build: env!("CARGO_PKG_VERSION").to_string(),
        };
        let decision = decide(&inputs);
        let outcome = self.act(decision).await;
        match outcome {
            Ok(Some((enabled, persistent))) => {
                let (endpoint, pid) = self
                    .link
                    .endpoint()
                    .map(|e| {
                        let pid = e.pid;
                        (e, pid)
                    })
                    .map_err(|e| HandoffError::failed(&e))?;
                Ok(HandoffResult {
                    selection: BackendSelection::Daemon {
                        endpoint,
                        pid,
                        enabled,
                        persistent,
                        integrations: detected.required_by.iter().map(|s| s.to_string()).collect(),
                    },
                    annotations: detected.annotations,
                    notice: None,
                })
            }
            Ok(None) => Ok(builtin(None)),
            Err((reason, Fallback::Builtin, _)) => Ok(builtin(Some(fallback_notice(&reason)))),
            Err((reason, Fallback::Exit, code)) => Err(HandoffError {
                message: fallback_notice(&reason),
                exit_code: code,
            }),
        }
    }

    /// `Ok(None)` = builtin, `Ok(Some((enable, persistent)))` = daemon ready,
    /// `Err((reason, fallback, exit_code))` = failed.
    async fn act(
        &self,
        decision: Decision,
    ) -> Result<Option<(EngineEnable, bool)>, (String, Fallback, u8)> {
        match decision {
            Decision::Builtin => Ok(None),
            Decision::Refused { reason, on_failure } => Err((reason, on_failure, 1)),
            Decision::Daemon { enable, on_failure } => self
                .enable(enable)
                .await
                .map(Some)
                .map_err(|reason| (reason, on_failure, 1)),
            Decision::RestartStaleDaemon {
                running_jobs,
                then,
                on_failure,
            } => {
                if running_jobs > 0 && !self.prompter.confirm_daemon_restart(running_jobs).await {
                    let reason = format!(
                        "the running daemon is from another awman version with {running_jobs} \
                         running job{}; restart it with `awman daemon stop`, or use \
                         `--set tui.backend=builtin`",
                        if running_jobs == 1 { "" } else { "s" }
                    );
                    return Err((reason, on_failure, EXIT_STALE_DAEMON));
                }
                self.link
                    .stop()
                    .await
                    .map_err(|e| (format!("could not stop the old daemon: {e}"), on_failure, 1))?;
                match *then {
                    Decision::Daemon { enable, on_failure } => self
                        .enable(enable)
                        .await
                        .map(Some)
                        .map_err(|reason| (reason, on_failure, 1)),
                    other => Box::pin(self.act(other)).await,
                }
            }
        }
    }

    /// Make the daemon's engine available; returns `(enable, persistent)`.
    async fn enable(&self, enable: EngineEnable) -> Result<(EngineEnable, bool), String> {
        match enable {
            EngineEnable::AlreadyOn => {
                let persistent = matches!(
                    self.link.probe().await,
                    DaemonProbe::Running {
                        engine: EngineProbe::Persistent,
                        ..
                    }
                );
                Ok((enable, persistent))
            }
            EngineEnable::Ephemeral => match self.link.enable_engine_ephemeral().await {
                Ok(()) => Ok((enable, false)),
                Err(EnableError::NotAllowed) => Err("the daemon refused ephemeral engine \
                     (daemon.features.engine.allowEphemeral is false)"
                    .into()),
                Err(EnableError::Other(e)) => Err(e),
            },
            EngineEnable::StartDaemon => {
                self.link
                    .start()
                    .await
                    .map_err(|e| format!("daemon failed to start: {e}"))?;
                Ok((enable, false))
            }
        }
    }

    /// `backend: daemon (engine ephemeral, rex) pid 1234` / `backend: builtin`.
    pub fn describe(result: &HandoffResult) -> String {
        describe(&result.selection)
    }
}

fn builtin(notice: Option<String>) -> HandoffResult {
    HandoffResult {
        selection: BackendSelection::Builtin,
        annotations: Annotations::new(),
        notice,
    }
}

/// `awman config show`'s resolved-backend line.
pub fn describe(selection: &BackendSelection) -> String {
    match selection {
        BackendSelection::Builtin => "backend: builtin".into(),
        BackendSelection::Daemon {
            pid,
            persistent,
            integrations,
            ..
        } => {
            let mut parts = vec![if *persistent {
                "engine on".to_string()
            } else {
                "engine ephemeral".to_string()
            }];
            parts.extend(integrations.iter().cloned());
            format!("backend: daemon ({}) pid {pid}", parts.join(", "))
        }
    }
}

// ───────────────────────────── real link ─────────────────────────────

/// The real [`DaemonLink`]: sidecar discovery and HTTP through
/// [`DaemonClient`], spawn and stop through `DaemonSupervisor`.
pub struct LocalDaemonLink {
    env: EnvSnapshot,
    config: EffectiveConfig,
}

impl LocalDaemonLink {
    pub fn new(env: EnvSnapshot, config: EffectiveConfig) -> Self {
        Self { env, config }
    }

    fn supervisor(&self) -> Result<DaemonSupervisor, String> {
        let paths = DataPaths::from_env(&self.env).map_err(|e| e.to_string())?;
        Ok(DaemonSupervisor::for_paths(paths.daemon()))
    }

    fn client(&self) -> Result<Option<DaemonClient>, CommandError> {
        Ok(DaemonClient::discover(&self.env)?.map(|c| c.with_timeout(STATUS_TIMEOUT)))
    }
}

/// The detached daemon's argv when the hand-off must start one: the invoking
/// `--set` overrides, then the engine switched on, then `daemon start`.
/// Kept in one place so the bootstrap's ephemeral-start switch changes here only.
pub fn ephemeral_start_args(set_args: &[String]) -> Vec<String> {
    let mut args = crate::command::commands::daemon::lifecycle::background_args(set_args);
    let at = args.len() - 2;
    args.splice(
        at..at,
        [
            "--set".to_string(),
            "daemon.features.engine.enabled=true".to_string(),
        ],
    );
    args
}

pub fn probe_from_status(status: &StatusResponse) -> DaemonProbe {
    let engine = match status.features.get("engine") {
        None => EngineProbe::Off {
            allow_ephemeral: false,
        },
        Some(record) => match record.state {
            FeatureState::On | FeatureState::Degraded => EngineProbe::Persistent,
            FeatureState::Ephemeral => EngineProbe::Ephemeral,
            FeatureState::Draining => EngineProbe::Draining,
            FeatureState::Off => EngineProbe::Off {
                // The status response does not carry the setting; the PATCH
                // answers `409 ephemeral_not_allowed` if it is false.
                allow_ephemeral: true,
            },
        },
    };
    DaemonProbe::Running {
        pid: status.pid,
        build: status.version.clone(),
        frontend_protocol: status
            .protocol
            .get(STATUS_FRONTEND_KEY)
            .map(|v| format!("awman.frontend.v{v}")),
        engine,
        running_jobs: status.features.values().map(|f| f.running + f.queued).sum(),
    }
}

#[async_trait::async_trait]
impl DaemonLink for LocalDaemonLink {
    async fn probe(&self) -> DaemonProbe {
        let client = match self.client() {
            Ok(Some(c)) => c,
            Ok(None) => return DaemonProbe::NotRunning,
            Err(e) => return DaemonProbe::Unreachable(e.to_string()),
        };
        match client
            .call_as::<StatusResponse>(&Request::get("/v1/status"))
            .await
        {
            Ok(status) => probe_from_status(&status),
            Err(e) => DaemonProbe::Unreachable(e.to_string()),
        }
    }

    async fn start(&self) -> Result<(), String> {
        let supervisor = self.supervisor()?;
        let binary = std::env::current_exe().map_err(|e| e.to_string())?;
        let args = ephemeral_start_args(&self.config.layers().set.set_args());
        // A concurrent invocation may win 0140's startup lock; the loser's
        // process exits and we connect to the winner's daemon below.
        supervisor
            .spawn_detached(&binary, &args)
            .map_err(|e| e.to_string())?;
        self.wait_ready().await
    }

    async fn enable_engine_ephemeral(&self) -> Result<(), EnableError> {
        let client = match self.client() {
            Ok(Some(c)) => c,
            Ok(None) => return Err(EnableError::Other("daemon is not running".into())),
            Err(e) => return Err(EnableError::Other(e.to_string())),
        };
        let request = Request::new(
            "PATCH",
            "/v1/features/engine",
            Some(serde_json::json!({"enabled": true, "ephemeral": true})),
        );
        match client.call(&request).await {
            Ok(_) => Ok(()),
            Err(CommandError::DaemonHttpStatus { status: 409, body })
                if body.contains("ephemeral_not_allowed") =>
            {
                Err(EnableError::NotAllowed)
            }
            Err(e) => Err(EnableError::Other(e.to_string())),
        }
    }

    async fn stop(&self) -> Result<(), String> {
        let supervisor = self.supervisor()?;
        supervisor.terminate().map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while supervisor.running_pid().ok().flatten().is_some() {
            if tokio::time::Instant::now() >= deadline {
                return Err("it did not exit in time".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    fn endpoint(&self) -> Result<LocalEndpoint, String> {
        let paths = DataPaths::from_env(&self.env).map_err(|e| e.to_string())?;
        crate::engine::remote::local::discover(&paths.daemon())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no daemon is running".to_string())
    }
}

impl LocalDaemonLink {
    async fn wait_ready(&self) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Ok(Some(client)) = self.client() {
                if client.call(&Request::get("/v1/status")).await.is_ok() {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("the daemon did not become ready; see `awman daemon logs`".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
#[path = "engine_handoff_tests.rs"]
mod tests;
