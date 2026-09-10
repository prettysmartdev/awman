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

use crate::command::commands::http_core::HttpCore;
use crate::command::commands::squad::env_sync;
use crate::command::commands::squad::gateway::{RemoteTaskGateway, TaskGateway};
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::catalogue::GatewayNeed;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::engine::auth::ApiKey;
use crate::engine::error::EngineError;
use crate::engine::squad::{key_setup, SquadEndpoint, SquadKeyState, SquadSupervisor};

/// A squad bearer key this process just minted, split so a frontend can show
/// the banner and still copy the raw key or the bare export line on its own.
///
/// The plaintext exists nowhere else — only its hash reaches disk — so a
/// frontend that is handed one of these and does not display it has lost the
/// key for good.
#[derive(Debug, Clone)]
pub struct SquadKeySetup {
    /// The rendered banner plus shell snippet, ready to display.
    pub body: String,
    pub key: String,
    pub zshrc_snippet: String,
}

impl SquadKeySetup {
    /// The disclosure a key state carries, if it carries one. Only a key this
    /// process minted has anything to show: `Ready` means the key came from
    /// the environment, and `Missing` means there is none to show.
    fn from_key_state(state: &SquadKeyState, env: &EnvSnapshot) -> Option<Self> {
        let SquadKeyState::Minted { setup, key } = state else {
            return None;
        };
        Some(Self {
            zshrc_snippet: key_setup::export_snippet(key, key_setup::ShellFlavor::from_env(env)),
            body: setup.clone(),
            key: key.clone(),
        })
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
    env: EnvSnapshot,
    /// The disclosure `gateway_for` read out of `key_state`, waiting for the
    /// frontend to show it. `key_state` is itself one-shot, so this is where
    /// a minted key lives between resolving the gateway and displaying it.
    pending_key_setup: Mutex<Option<SquadKeySetup>>,
}

impl SquadGatewayResolver {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, CommandError> {
        Ok(Self {
            inner: SquadSupervisor::from_env(env)?,
            env: env.clone(),
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

    /// Take the one-shot setup snippet for a key this process minted.
    pub fn take_generated_key_setup(&self) -> Option<String> {
        self.inner.take_generated_key_setup()
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
                    SquadKeyState::Minted { setup, key } => {
                        *self.pending_setup_slot() = SquadKeySetup::from_key_state(
                            &SquadKeyState::Minted { setup, key },
                            &self.env,
                        );
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
    pub async fn open_for_frontend(
        &self,
        engines: &Engines,
    ) -> Result<SquadStartup, SquadStartError> {
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
    pub async fn refresh_key_for_frontend(&self) -> Result<SquadStartup, SquadStartError> {
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
    pub async fn open_from_env(
        env: &EnvSnapshot,
        engines: &Engines,
    ) -> Result<SquadStartup, SquadStartError> {
        Self::from_env(env)
            .map_err(SquadStartError::attributed)?
            .open_for_frontend(engines)
            .await
    }

    /// [`Self::open_from_env`]'s key-refresh counterpart.
    pub async fn refresh_key_from_env(env: &EnvSnapshot) -> Result<SquadStartup, SquadStartError> {
        Self::from_env(env)
            .map_err(SquadStartError::attributed)?
            .refresh_key_for_frontend()
            .await
    }

    async fn startup_from_endpoint(
        &self,
        endpoint: SquadEndpoint,
    ) -> Result<SquadStartup, SquadStartError> {
        let key_state = self
            .inner
            .key_state()
            .map_err(SquadStartError::from_engine)?;
        // A daemon this process cannot authenticate to is not worth a view:
        // every request would be refused with a bare 401.
        if matches!(key_state, SquadKeyState::Missing) {
            return Err(SquadStartError::KeyMissing);
        }
        let key_setup = SquadKeySetup::from_key_state(&key_state, &self.env);
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
