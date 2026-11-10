//! `DaemonResolver`: how squad commands and the TUI's squad tab reach the
//! running daemon's squad feature.
//!
//! Squad is a feature of the one awman daemon. There is no squad daemon, no
//! squad key and no gateway forwarding: commands call `/v1/squad/*` over the
//! daemon's owner-only local listener ([`DaemonSquadClient`]). Starting the
//! daemon is `awman daemon start`; the TUI's squad tab may start it in the
//! background after the user confirms.

use std::sync::Arc;
use std::time::Duration;

use crate::command::commands::daemon::actions::Request;
use crate::command::commands::daemon::client::DaemonClient;
use crate::command::commands::squad::client::DaemonSquadClient;
use crate::command::commands::squad::env_sync;
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::catalogue::GatewayNeed;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::config_json::set_config_field;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::file::{global_config_path, ConfigFile, FileKind};
use crate::data::fs::DataPaths;
use crate::engine::daemon::DaemonSupervisor;
use crate::engine::squad::{SquadHealth, SquadSupervisor};

/// How long the TUI waits for a daemon it started to answer.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// A squad view opened for an interactive frontend.
pub struct SquadStartup {
    pub gateway: Arc<dyn TaskGateway>,
}

/// The outcome of one attempt to open squad for a frontend.
pub type SquadStartupResult = Result<SquadStartup, SquadStartError>;

/// Why opening squad for a frontend failed, typed so the caller maps an
/// outcome rather than a message prefix.
#[derive(Debug)]
pub enum SquadStartError {
    /// The configured runtime is sandbox-class and cannot back squad at all.
    SandboxRuntime(String),
    /// Anything else, already phrased for a user.
    Other(String),
}

impl std::fmt::Display for SquadStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SandboxRuntime(message) | Self::Other(message) => f.write_str(message),
        }
    }
}

/// Resolves the squad gateway for one environment (one `AWMAN_HOME`).
pub struct DaemonResolver {
    env: EnvSnapshot,
}

impl DaemonResolver {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, CommandError> {
        // Fail early on an unresolvable home, as every caller would later.
        DataPaths::from_env(env)?;
        Ok(Self { env: env.clone() })
    }

    /// The running daemon's local client, if a daemon is running.
    pub fn client(&self) -> Result<Option<DaemonClient>, CommandError> {
        DaemonClient::discover(&self.env)
    }

    /// Whether the daemon is running. Starts nothing.
    pub fn daemon_is_running(&self) -> Result<bool, CommandError> {
        Ok(
            DaemonSupervisor::for_paths(DataPaths::from_env(&self.env)?.daemon())
                .running_pid()?
                .is_some(),
        )
    }

    /// Whether the persistent global config already enables squad. The TUI
    /// uses this to ask before changing the user's config.
    pub fn squad_enabled_in_config(&self) -> Result<bool, CommandError> {
        let path = global_config_path(&self.env)?;
        let config = ConfigFile::load(&path, FileKind::Global)?;
        Ok(config
            .daemon
            .and_then(|daemon| daemon.features)
            .and_then(|features| features.squad)
            .and_then(|squad| squad.enabled)
            .unwrap_or(false))
    }

    /// Persist the setting before starting a daemon, preserving every other
    /// global field. Return whether a write was needed.
    fn enable_squad_in_config(&self) -> Result<bool, SquadStartError> {
        let path = global_config_path(&self.env).map_err(config_error)?;
        let config = ConfigFile::load(&path, FileKind::Global).map_err(config_error)?;
        let mut value = config.to_value();
        if value["daemon"]["features"]["squad"]["enabled"] == true {
            return Ok(false);
        }
        set_config_field(
            &mut value,
            "daemon.features.squad.enabled",
            serde_json::Value::Bool(true),
        );
        ConfigFile::from_value(value, FileKind::Global)
            .and_then(|config| config.save(&path, FileKind::Global))
            .map_err(config_error)?;
        Ok(true)
    }

    /// A squad gateway to the running daemon, after bringing its task
    /// environment up to date (a digest check; nothing is sent unless a value
    /// differs).
    async fn synced_gateway(client: DaemonClient) -> DaemonSquadClient {
        let gateway = DaemonSquadClient::new(client);
        env_sync::sync_env(&gateway, false).await;
        gateway
    }

    /// Resolve the gateway a command's catalogue [`GatewayNeed`] calls for.
    ///
    /// `Running` needs the daemon and refuses with
    /// [`CommandError::DaemonNotRunning`] without it; `IfRunning` answers
    /// `None` when no daemon runs. Neither starts anything.
    pub async fn gateway_for(
        &self,
        need: GatewayNeed,
    ) -> Result<Option<Arc<dyn TaskGateway>>, CommandError> {
        match need {
            GatewayNeed::None => Ok(None),
            GatewayNeed::Running => {
                let client = DaemonClient::connect(&self.env)?;
                Ok(Some(
                    Arc::new(Self::synced_gateway(client).await) as Arc<dyn TaskGateway>
                ))
            }
            GatewayNeed::IfRunning => Ok(self
                .client()?
                .map(|client| Arc::new(DaemonSquadClient::new(client)) as Arc<dyn TaskGateway>)),
        }
    }

    /// Probe squad once and report its [`SquadHealth`]. Starts nothing.
    pub async fn health(&self, timeout: Duration) -> SquadHealth {
        tokio::time::timeout(timeout, self.probe_health())
            .await
            .unwrap_or(SquadHealth::Unreachable)
    }

    /// One health probe for a caller that holds no resolver (the TUI's
    /// indicator poller). An unresolvable home is a daemon this process got
    /// no answer from: `Unreachable`.
    pub async fn health_from_env(env: &EnvSnapshot, timeout: Duration) -> SquadHealth {
        match Self::from_env(env) {
            Ok(resolver) => resolver.health(timeout).await,
            Err(_) => SquadHealth::Unreachable,
        }
    }

    async fn probe_health(&self) -> SquadHealth {
        match self.daemon_is_running() {
            Ok(true) => {}
            Ok(false) => return SquadHealth::NotRunning,
            Err(_) => return SquadHealth::Unreachable,
        }
        let Ok(Some(client)) = self.client() else {
            return SquadHealth::Unreachable;
        };
        match DaemonSquadClient::new(client).list().await {
            Ok(tasks) => SquadSupervisor::health_of_running(Some(&tasks)),
            Err(_) => SquadSupervisor::health_of_running(None),
        }
    }

    /// Whether squad can run at all under these engines.
    pub fn admit_runtime(engines: &Engines) -> Result<(), SquadStartError> {
        require_container_tier(engines)
            .map_err(|error| SquadStartError::SandboxRuntime(error.to_string()))
    }

    /// Open squad after the TUI's confirmation. Persist the enabled setting,
    /// reload a running daemon when needed, or start one in the background,
    /// then return a synced gateway.
    pub async fn open_for_frontend(&self, engines: &Engines) -> SquadStartupResult {
        Self::admit_runtime(engines)?;
        let config_changed = self.enable_squad_in_config()?;
        let client = match self.client().map_err(other)? {
            Some(client) => {
                if config_changed {
                    client
                        .call(&Request::post("/v1/config/reload", None))
                        .await
                        .map_err(|error| {
                            SquadStartError::Other(format!(
                                "squad was enabled in config, but the running daemon could not reload it: {error}"
                            ))
                        })?;
                }
                client
            }
            None => self.start_in_background().await?,
        };
        // A higher-priority override may still leave squad off; say so
        // instead of building a tab whose every poll is refused.
        let feature: serde_json::Value = client
            .call(&Request::get("/v1/features/squad"))
            .await
            .map_err(other)?;
        if feature["state"] == "off" {
            return Err(SquadStartError::Other(
                "the squad feature is still off after enabling it in config; check daemon environment overrides"
                    .into(),
            ));
        }
        Ok(SquadStartup {
            gateway: Arc::new(Self::synced_gateway(client).await) as Arc<dyn TaskGateway>,
        })
    }

    /// Build a resolver from `env` and open squad for a frontend in one call.
    pub async fn open_from_env(env: &EnvSnapshot, engines: &Engines) -> SquadStartupResult {
        Self::from_env(env)
            .map_err(other)?
            .open_for_frontend(engines)
            .await
    }

    async fn start_in_background(&self) -> Result<DaemonClient, SquadStartError> {
        let paths = DataPaths::from_env(&self.env).map_err(other)?.daemon();
        let supervisor = DaemonSupervisor::for_paths(paths.clone());
        let binary = std::env::current_exe().map_err(|e| {
            SquadStartError::Other(format!("cannot determine the awman binary: {e}"))
        })?;
        supervisor
            .spawn_detached(&binary, &["daemon".into(), "start".into()])
            .map_err(other)?;
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if let Some(client) = self.client().map_err(other)? {
                return Ok(client);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SquadStartError::Other(format!(
                    "the awman daemon did not start; see {}",
                    paths.log_file().display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn other(error: impl std::fmt::Display) -> SquadStartError {
    SquadStartError::Other(format!("could not reach the awman daemon: {error}"))
}

fn config_error(error: impl std::fmt::Display) -> SquadStartError {
    SquadStartError::Other(format!("could not enable squad in config: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::data_paths::AWMAN_HOME;

    fn empty_home() -> (tempfile::TempDir, EnvSnapshot) {
        let tmp = tempfile::tempdir().unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_HOME, tmp.path().to_string_lossy().to_string())]);
        (tmp, env)
    }

    #[test]
    fn enabling_squad_persists_only_that_global_field() {
        let (_tmp, env) = empty_home();
        let path = global_config_path(&env).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"daemon":{"maxConcurrentJobs":7,"features":{"squad":{"enabled":false,"envPersistence":"none"}}}}"#,
        )
        .unwrap();
        let resolver = DaemonResolver::from_env(&env).unwrap();

        assert!(!resolver.squad_enabled_in_config().unwrap());
        assert!(resolver.enable_squad_in_config().unwrap());
        assert!(resolver.squad_enabled_in_config().unwrap());
        let written = ConfigFile::load(&path, FileKind::Global)
            .unwrap()
            .to_value();
        assert_eq!(written["daemon"]["features"]["squad"]["enabled"], true);
        assert_eq!(
            written["daemon"]["features"]["squad"]["envPersistence"],
            "none"
        );
        assert_eq!(written["daemon"]["maxConcurrentJobs"], 7);

        let first_write = std::fs::read(&path).unwrap();
        assert!(!resolver.enable_squad_in_config().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), first_write);
    }

    #[test]
    fn malformed_config_is_not_replaced_when_enabling_squad() {
        let (_tmp, env) = empty_home();
        let path = global_config_path(&env).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ invalid json").unwrap();
        let resolver = DaemonResolver::from_env(&env).unwrap();
        assert!(resolver.enable_squad_in_config().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ invalid json");
    }

    #[tokio::test]
    async fn health_without_a_daemon_is_not_running() {
        let (_tmp, env) = empty_home();
        let health = DaemonResolver::health_from_env(&env, Duration::from_secs(2)).await;
        assert_eq!(health, SquadHealth::NotRunning);
    }

    #[tokio::test]
    async fn task_commands_need_the_daemon_and_status_does_not() {
        let (_tmp, env) = empty_home();
        let resolver = DaemonResolver::from_env(&env).unwrap();
        assert!(!resolver.daemon_is_running().unwrap());
        let error = resolver
            .gateway_for(GatewayNeed::Running)
            .await
            .err()
            .expect("no daemon");
        assert!(matches!(error, CommandError::DaemonNotRunning), "{error:?}");
        assert!(resolver
            .gateway_for(GatewayNeed::IfRunning)
            .await
            .unwrap()
            .is_none());
        assert!(resolver
            .gateway_for(GatewayNeed::None)
            .await
            .unwrap()
            .is_none());
    }
}
