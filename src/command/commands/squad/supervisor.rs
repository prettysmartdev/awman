//! `SquadGatewayResolver` — the Layer 2 half of squad daemon supervision.
//!
//! The lifecycle itself (is a daemon running, what key does this process hold,
//! start one on demand) is Layer 1's [`SquadSupervisor`]. What stays here is
//! the part that is genuinely Layer 2: turning the endpoint the supervisor
//! reports into the [`RemoteTaskGateway`] commands speak through, and mapping
//! `EngineError` onto `CommandError`.
//!
//! WI 0114 F-28 moves the transport itself down, at
//! which point this becomes a pure error-mapping shim.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::command::commands::squad::env_sync;
use crate::command::commands::squad::gateway::{RemoteTaskGateway, TaskGateway};
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::catalogue::GatewayNeed;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::engine::auth::ApiKey;
use crate::engine::error::EngineError;
use crate::engine::remote::HttpCore;
use crate::engine::squad::key_setup::{KeyDisclosure, ShellFlavor};
use crate::engine::squad::{SquadEndpoint, SquadHealth, SquadKeyState, SquadSupervisor};

/// A squad bearer key this process just minted, split so a frontend can show
/// the banner and still copy the raw key or the bare export line on its own.
///
/// The plaintext exists nowhere else — only its hash reaches disk — so a
/// frontend that is handed one of these and does not display it has lost the
/// key for good.
#[derive(Debug, Clone)]
pub struct SquadKeySetup {
    /// The plaintext key.
    pub key: String,
    /// The shell it was resolved against, so a frontend can name the startup
    /// file the export belongs in ([`ShellFlavor::rc_file`]).
    pub shell: ShellFlavor,
    /// The line to add to that file — also what a "copy the snippet" action
    /// puts on the clipboard.
    pub export_line: String,
}

impl SquadKeySetup {
    /// The disclosure for a key just minted for `shell`.
    pub fn for_key(key: &str, shell: ShellFlavor) -> Self {
        Self::from_disclosure(&KeyDisclosure::new(key, shell))
    }

    fn from_disclosure(disclosure: &KeyDisclosure) -> Self {
        Self {
            key: disclosure.key.clone(),
            shell: disclosure.shell,
            export_line: disclosure.export_line.clone(),
        }
    }

    /// The disclosure a key state carries, if it carries one. Only a key this
    /// process minted has anything to show: `Ready` means the key came from
    /// the environment, and `Missing` means there is none to show.
    fn from_key_state(state: &SquadKeyState) -> Option<Self> {
        let SquadKeyState::Minted(disclosure) = state else {
            return None;
        };
        Some(Self::from_disclosure(disclosure))
    }

    /// The startup file the export belongs in, as displayed to the user.
    pub fn rc_file(&self) -> &'static str {
        self.shell.rc_file()
    }
}

/// A squad daemon opened for an interactive frontend: the gateway to talk to
/// it through, what this process can authenticate with, and the one-shot key
/// disclosure when this call is what minted it.
pub struct SquadStartup {
    pub gateway: Arc<dyn TaskGateway>,
    pub key_state: SquadKeyState,
    pub key_setup: Option<SquadKeySetup>,
}

/// The outcome of one attempt to open a squad daemon for a frontend.
///
/// Named so a frontend handing the attempt to a background thread can declare
/// the receiving channel without re-spelling the pair.
pub type SquadStartupResult = Result<SquadStartup, SquadStartError>;

/// Why opening a squad daemon for a frontend failed, typed so the caller maps
/// an outcome rather than a message prefix (WI 0113 F-04).
#[derive(Debug)]
pub enum SquadStartError {
    /// `awman api` holds the machine; the two daemons are mutually exclusive.
    DaemonConflict(String),
    /// The configured runtime is sandbox-class and cannot back squad at all.
    SandboxRuntime(String),
    /// The daemon is up and healthy, but this process holds no key for it.
    /// Not a failure of the daemon — the recovery is to mint a new key.
    KeyMissing,
    /// Anything else, already phrased for a user.
    Other(String),
}

impl std::fmt::Display for SquadStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Each of these already says where the problem is: a conflict
            // names `awman api`, a runtime refusal names the runtime, and
            // `Other` is attributed at the point it is classified.
            Self::DaemonConflict(message)
            | Self::SandboxRuntime(message)
            | Self::Other(message) => f.write_str(message),
            Self::KeyMissing => write!(f, "{}", CommandError::SquadKeyMissing),
        }
    }
}

impl SquadStartError {
    /// Classify a Layer 1 failure by its variant. The Layer 1 squad variants
    /// carry text that already names squad; everything else is attributed
    /// here, because a bare "permission denied" says nothing about what the
    /// user asked for.
    fn from_engine(error: EngineError) -> Self {
        match error {
            EngineError::SquadDaemonConflict(message) => Self::DaemonConflict(message),
            EngineError::SquadRuntimeUnsupported { .. } => Self::SandboxRuntime(error.to_string()),
            EngineError::SquadDaemonStartup(message)
            | EngineError::SquadDaemonUnreachable(message) => Self::Other(message),
            other => Self::attributed(other),
        }
    }

    /// Attribute a failure that carries no squad context of its own. A bare
    /// "permission denied" says nothing about what the user asked for.
    fn attributed(error: impl std::fmt::Display) -> Self {
        Self::Other(format!("failed to start the squad daemon: {error}"))
    }
}

pub struct SquadGatewayResolver {
    inner: SquadSupervisor,
    /// The disclosure `gateway_for` read out of `key_state`, waiting for the
    /// frontend to show it. `key_state` is itself one-shot, so this is where
    /// a minted key lives between resolving the gateway and displaying it.
    pending_key_setup: Mutex<Option<SquadKeySetup>>,
}

impl SquadGatewayResolver {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, CommandError> {
        Ok(Self {
            inner: SquadSupervisor::from_env(env)?,
            pending_key_setup: Mutex::new(None),
        })
    }

    /// The Layer 1 supervisor, for callers that need the lifecycle without a
    /// gateway.
    pub fn supervisor(&self) -> &SquadSupervisor {
        &self.inner
    }

    /// The key this supervisor minted during `ensure_running`, if any.
    pub fn generated_key(&self) -> Option<ApiKey> {
        self.inner.generated_key()
    }

    /// Take the one-shot disclosure for a key this process minted.
    pub fn take_key_disclosure(&self) -> Option<SquadKeySetup> {
        self.inner
            .take_key_disclosure()
            .map(|disclosure| SquadKeySetup::from_disclosure(&disclosure))
    }

    /// What this process can authenticate to squad with.
    pub fn key_state(&self) -> Result<SquadKeyState, CommandError> {
        Ok(self.inner.key_state()?)
    }

    /// Whether a squad daemon is already running for this squad root.
    pub fn daemon_is_running(&self) -> Result<bool, CommandError> {
        Ok(self.inner.daemon_is_running()?)
    }

    /// Mint a fresh key and restart the daemon onto it.
    pub async fn refresh_key(&self) -> Result<RemoteTaskGateway, CommandError> {
        let endpoint = self.inner.refresh_key().await?;
        Self::synced_gateway(endpoint).await
    }

    /// Build a gateway and bring the daemon's payload environment up to date
    /// through it (WI 0116 §4a).
    ///
    /// Every path that yields a *keyed* gateway to a running daemon goes
    /// through here — both the already-running and the freshly-spawned branch —
    /// so a long-lived daemon picks up a rotated token from the next command
    /// without a restart. What actually happens is a coverage *check*: the
    /// daemon reports a digest per name it holds, and nothing is sent unless a
    /// value genuinely differs. Steady state costs one small GET and puts no
    /// secret on the wire.
    ///
    /// Deliberately not called from [`Self::gateway_from_meta`] or
    /// [`Self::probe_gateway`]: `awman squad status` and the TUI's 10-second
    /// indicator poller use those, and neither should be pushing anything.
    async fn synced_gateway(endpoint: SquadEndpoint) -> Result<RemoteTaskGateway, CommandError> {
        let gateway = Self::gateway_for_endpoint(endpoint)?;
        env_sync::sync_env(&gateway, false).await;
        Ok(gateway)
    }

    /// A gateway to the daemon named by its endpoint sidecar, if one exists.
    pub fn gateway_from_meta(&self) -> Result<Option<RemoteTaskGateway>, CommandError> {
        self.inner
            .endpoint_from_meta()?
            .map(Self::gateway_for_endpoint)
            .transpose()
    }

    /// A gateway for a read-only health probe. Never mints a key — see
    /// [`SquadSupervisor::probe_endpoint`].
    pub fn probe_gateway(&self) -> Result<Option<RemoteTaskGateway>, CommandError> {
        self.inner
            .probe_endpoint()?
            .map(Self::gateway_for_endpoint)
            .transpose()
    }

    /// Probe the daemon once and report its [`SquadHealth`].
    ///
    /// Starts nothing and mints no key: the gateway is
    /// [`SquadSupervisor::probe_endpoint`]'s read-only one, so a health check
    /// never has the side effect of provisioning a daemon.
    ///
    /// `timeout` bounds the whole probe, so a hung daemon cannot stack
    /// probes behind it — a caller polling on an interval keeps at most one
    /// in flight. A probe that runs out of time reports `Unreachable`, which
    /// is what a daemon that does not answer in time *is*.
    ///
    /// The verdict itself is [`SquadSupervisor::health`], at Layer 1. This
    /// method is the transport half, and lives here only until the Layer 1
    /// HTTP client lands (WI 0114 F-28) and the whole probe can move down
    /// beside the classification.
    pub async fn health(&self, timeout: Duration) -> SquadHealth {
        tokio::time::timeout(timeout, self.probe_health())
            .await
            .unwrap_or(SquadHealth::Unreachable)
    }

    /// One health probe for a caller that holds no resolver — the TUI's
    /// bottom-row indicator poller, which re-reads the environment on every
    /// tick so a key minted mid-session is picked up without a restart.
    ///
    /// Infallible, and that is the point. A resolver that cannot even be
    /// built — no squad root resolvable from this environment — is a daemon
    /// this process got no answer from, which is exactly what `Unreachable`
    /// means; deciding that is a health classification and belongs here, with
    /// [`Self::health`] and [`SquadSupervisor::health`], not in a frontend
    /// (Tenet 2, F-17).
    pub async fn health_from_env(env: &EnvSnapshot, timeout: Duration) -> SquadHealth {
        match Self::from_env(env) {
            Ok(resolver) => resolver.health(timeout).await,
            Err(_) => SquadHealth::Unreachable,
        }
    }

    async fn probe_health(&self) -> SquadHealth {
        let gateway = match self.probe_gateway() {
            Ok(Some(gateway)) => gateway,
            // No endpoint sidecar, no key, or a malformed one. Whether a
            // process is running at all is `health`'s to decide.
            Ok(None) | Err(_) => return self.inner.health(None),
        };
        match gateway.list().await {
            Ok(tasks) => self.inner.health(Some(&tasks)),
            Err(_) => self.inner.health(None),
        }
    }

    /// A gateway to a running daemon, starting one only when needed.
    pub async fn ensure_running(&self) -> Result<RemoteTaskGateway, CommandError> {
        let endpoint = self.inner.ensure_running().await?;
        Self::synced_gateway(endpoint).await
    }

    // ── Dispatch-facing resolution (WI 0113 F-04) ─────────────────────────

    /// Resolve the gateway a command's catalogue [`GatewayNeed`] calls for.
    ///
    /// `Running` starts a daemon when none is running, and refuses with
    /// [`CommandError::SquadKeyMissing`] when this process holds no key for
    /// the one that answers — the request's own answer would be a bare
    /// `HTTP 401` naming neither the variable to set nor the fact that the
    /// key cannot be read back.
    ///
    /// `IfRunning` reads the endpoint sidecar and answers `None` when there
    /// is none, which is what lets `squad status` still succeed with a "not
    /// running" summary. It starts nothing.
    ///
    /// A key minted along the way is left in [`Self::take_key_setup`] for the
    /// caller to display: this is the only moment the plaintext exists
    /// outside the daemon's hash file.
    pub async fn gateway_for(
        &self,
        need: GatewayNeed,
    ) -> Result<Option<Arc<dyn TaskGateway>>, CommandError> {
        match need {
            GatewayNeed::None => Ok(None),
            GatewayNeed::Running => {
                let gateway = self.ensure_running().await?;
                match self.key_state()? {
                    state @ SquadKeyState::Minted(_) => {
                        *self.pending_setup_slot() = SquadKeySetup::from_key_state(&state);
                    }
                    SquadKeyState::Ready => {}
                    SquadKeyState::Missing => return Err(CommandError::SquadKeyMissing),
                }
                Ok(Some(Arc::new(gateway) as Arc<dyn TaskGateway>))
            }
            GatewayNeed::IfRunning => Ok(self
                .gateway_from_meta()?
                .map(|gateway| Arc::new(gateway) as Arc<dyn TaskGateway>)),
        }
    }

    /// Take the disclosure a preceding [`Self::gateway_for`] left behind, if
    /// it minted a key. `None` on every later call, so a caller may show the
    /// result unconditionally.
    pub fn take_key_setup(&self) -> Option<SquadKeySetup> {
        self.pending_setup_slot().take()
    }

    /// Open a squad daemon for an interactive frontend, reporting every
    /// failure as a typed outcome the caller maps to a dialog.
    ///
    /// This is [`Self::gateway_for`]'s `Running` path with the runtime-tier
    /// admission folded in, because a frontend opening a squad view has no
    /// `Dispatch` to run the catalogue's `requires_container_tier` guard for
    /// it. A sandbox-class runtime cannot back squad at all, so asking the
    /// daemon anything would be asking a question whose answer could not be
    /// honoured.
    pub async fn open_for_frontend(&self, engines: &Engines) -> SquadStartupResult {
        require_container_tier(engines)
            .map_err(|error| SquadStartError::SandboxRuntime(error.to_string()))?;
        let endpoint = self
            .inner
            .ensure_running()
            .await
            .map_err(SquadStartError::from_engine)?;
        self.startup_from_endpoint(endpoint).await
    }

    /// Mint a fresh key, restart the daemon onto it, and return what an
    /// ordinary open returns — so a frontend drains a recovery and a first
    /// run through one path. The key is always `Minted` here, which is the
    /// point: the recovery ends by showing the user the key they were missing.
    pub async fn refresh_key_for_frontend(&self) -> SquadStartupResult {
        let endpoint = self
            .inner
            .refresh_key()
            .await
            .map_err(SquadStartError::from_engine)?;
        self.startup_from_endpoint(endpoint).await
    }

    /// Whether squad can run at all under these engines.
    ///
    /// Separated from [`Self::open_for_frontend`] for the one caller that has
    /// to ask before it asks the *user* anything: offering to start a daemon
    /// under a sandbox-class runtime would be putting a question whose "yes"
    /// could not be honoured.
    pub fn admit_runtime(engines: &Engines) -> Result<(), SquadStartError> {
        require_container_tier(engines)
            .map_err(|error| SquadStartError::SandboxRuntime(error.to_string()))
    }

    /// Build a resolver from `env` and open a daemon for a frontend in one
    /// call, so a frontend never has to map a `CommandError` of its own.
    pub async fn open_from_env(env: &EnvSnapshot, engines: &Engines) -> SquadStartupResult {
        Self::from_env(env)
            .map_err(SquadStartError::attributed)?
            .open_for_frontend(engines)
            .await
    }

    /// [`Self::open_from_env`]'s key-refresh counterpart.
    pub async fn refresh_key_from_env(env: &EnvSnapshot) -> SquadStartupResult {
        Self::from_env(env)
            .map_err(SquadStartError::attributed)?
            .refresh_key_for_frontend()
            .await
    }

    async fn startup_from_endpoint(&self, endpoint: SquadEndpoint) -> SquadStartupResult {
        let key_state = self
            .inner
            .key_state()
            .map_err(SquadStartError::from_engine)?;
        // A daemon this process cannot authenticate to is not worth a view:
        // every request would be refused with a bare 401.
        if matches!(key_state, SquadKeyState::Missing) {
            return Err(SquadStartError::KeyMissing);
        }
        let key_setup = SquadKeySetup::from_key_state(&key_state);
        // A frontend opening a squad view is a keyed connection to a running
        // daemon like any other, so it syncs too (WI 0116 §4a).
        let gateway = Self::synced_gateway(endpoint)
            .await
            .map_err(|error| SquadStartError::Other(error.to_string()))?;
        Ok(SquadStartup {
            gateway: Arc::new(gateway) as Arc<dyn TaskGateway>,
            key_state,
            key_setup,
        })
    }

    fn pending_setup_slot(&self) -> std::sync::MutexGuard<'_, Option<SquadKeySetup>> {
        self.pending_key_setup
            .lock()
            .expect("squad pending key-setup mutex poisoned")
    }

    fn gateway_for_endpoint(endpoint: SquadEndpoint) -> Result<RemoteTaskGateway, CommandError> {
        Ok(RemoteTaskGateway::new(HttpCore::new(
            &endpoint.address,
            "v1",
            endpoint.key.as_ref(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::AWMAN_SQUAD_ROOT;

    /// `health_from_env` classifies in Layer 2 for a caller that holds no
    /// resolver. With a squad root that exists but has no daemon behind it,
    /// the verdict is `NotRunning` — not `Unreachable`, which is what the
    /// TUI's poller used to answer for anything it could not resolve itself
    /// (F-17, finding 10 of the WI 0114 final review).
    #[tokio::test]
    async fn health_from_env_classifies_an_empty_squad_root_as_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(
            AWMAN_SQUAD_ROOT,
            tmp.path().to_string_lossy().to_string(),
        )]);

        let health = SquadGatewayResolver::health_from_env(&env, Duration::from_secs(2)).await;

        assert_eq!(health, SquadHealth::NotRunning);
    }
}
