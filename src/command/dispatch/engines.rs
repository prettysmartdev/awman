//! [`Engines`] — the one owner of the Layer 1 graph a command runs against:
//! how a session-backed host and a daemon assemble it, and which runtime each
//! gets when the configured one is unknown or unavailable.

use std::sync::Arc;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::startup::RuntimeStartupFailure;
use crate::data::config::{ConfigFile, EffectiveConfig};
use crate::data::fs::{ApiPaths, AuthPathResolver};
use crate::data::session::Session;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{self, AgentRuntimeEngine, DetectedRuntime};
use crate::engine::auth::AuthEngine;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::sandbox::SandboxRuntime;

/// Build the live credential monitor this engine bundle's launches register
/// leases with, or `None` when `authRefresh.enabled` is false.
///
/// Before WI 0114 F-38 this installed a process-global `OnceLock` that the
/// container backends reached behind everybody's back, and "no monitor
/// installed" was how the kill switch was implemented. The monitor is now
/// carried explicitly — attached to `AgentEngine` here, emitted onto
/// `ResolvedContainerOptions` at build-options time, and read by the backends
/// from the options they were handed. `None` means leases are disabled,
/// exactly as an uninstalled global did.
pub fn credential_refresh_monitor(
    config: &EffectiveConfig,
) -> Option<Arc<crate::engine::credential_refresh::CredentialRefreshMonitor>> {
    let settings = config.auth_refresh();
    if !settings.enabled {
        return None;
    }
    Some(
        crate::engine::credential_refresh::CredentialRefreshMonitor::new(
            crate::engine::credential_refresh::MonitorConfig {
                refresh_threshold: settings.threshold,
                tick_interval: settings.tick,
            },
        ),
    )
}

/// The lease-factory handle for a monitor, or `None` when there is none.
fn lease_factory_for(
    monitor: Option<&Arc<crate::engine::credential_refresh::CredentialRefreshMonitor>>,
) -> Option<crate::engine::credential_refresh::LeaseFactoryHandle> {
    monitor.map(|m| {
        crate::engine::credential_refresh::LeaseFactoryHandle::new(Arc::new(Arc::clone(m)))
    })
}

/// All Layer 1 engine handles a `Dispatch` needs to construct a `*Command`.
/// `ReadyEngine` and `InitEngine` are NOT pre-constructed here — those
/// engines accept per-invocation flag values.
#[derive(Clone)]
pub struct Engines {
    pub ephemeral_secrets: Arc<super::ephemeral_secrets::EphemeralSecrets>,
    /// Cross-paradigm trait-object handle. Used for build(), list_running(),
    /// stats(), stop(), attach(), is_available(), capabilities() and
    /// other operations that exist on both paradigms.
    pub runtime: Arc<dyn AgentRuntimeEngine>,

    /// Container-paradigm-specific handle, set when `runtime` is a
    /// ContainerRuntime. None when the active runtime is a SandboxRuntime.
    /// Used for image-paradigm operations (build_image, image_exists,
    /// image_home_dir, start_background) that only exist on the container
    /// side. Points at the same underlying object as `runtime`.
    pub container_runtime: Option<Arc<ContainerRuntime>>,

    /// Sandbox-paradigm-specific handle, mirror of `container_runtime` for
    /// sandbox-only operations. None when running under a ContainerRuntime.
    pub sandbox_runtime: Option<Arc<SandboxRuntime>>,

    pub git_engine: Arc<GitEngine>,
    pub overlay_engine: Arc<OverlayEngine>,
    pub auth_engine: Arc<AuthEngine>,
    pub agent_engine: Arc<AgentEngine>,
    pub workflow_state_store: Arc<crate::data::WorkflowStateStore>,

    /// The live credential-refresh monitor, or `None` when
    /// `authRefresh.enabled` is false. Built once per engine bundle (F-38
    /// replaced the process-global `OnceLock`). `agent_engine` already holds
    /// it as a lease factory; this handle is for the callers that need the
    /// monitor itself — the workflow pre-step guard and auth-failure
    /// recovery, `security.md` triggers (c) and (d).
    pub credential_monitor:
        Option<Arc<crate::engine::credential_refresh::CredentialRefreshMonitor>>,

    /// The global config this bundle was assembled from.
    ///
    /// A **daemon's** single config source. Daemons have no `Session`, so
    /// before WI 0114 F-31 the daemon-side code that needed a global setting
    /// simply loaded the global config file wherever it stood — four separate
    /// reads, each with its own `unwrap_or_default()` swallowing a malformed
    /// file, and each able to see a different file than the one the daemon
    /// started with.
    ///
    /// A **session-backed** host reads `session.effective_config()` instead:
    /// that already merged this in, under the repo config, the environment
    /// and the flags. This field is for the code that has no session.
    pub global_config: Arc<ConfigFile>,
}

impl Engines {
    pub fn command_secrets(&self) -> Option<Arc<crate::data::secret::CommandSecrets>> {
        self.ephemeral_secrets.current()
    }

    /// Assemble the engines for a session-backed command invocation.
    ///
    /// This is the single owner of the Layer 1 graph formerly assembled by
    /// the binary entrypoint. Runtime selection is deliberately here rather
    /// than in a frontend so every session-backed host gets the same tier.
    pub fn build(session: &Session) -> Result<Self, EngineError> {
        let detected = agent_runtime::detect_effective(&session.effective_config())?;
        // Hosts without a startup frontend still record an in-place builtin
        // state upgrade rather than dropping its notice.
        for notice in detected.take_notices() {
            tracing::info!("{notice}");
        }
        Self::from_detected(detected, session)
    }

    pub fn for_env(env: crate::data::config::env::EnvSnapshot) -> Result<Self, EngineError> {
        let auth_paths = AuthPathResolver::at_home(
            env.home_dir()
                .or_else(dirs::home_dir)
                .ok_or(crate::data::error::DataError::HomeNotFound)?,
        );
        let api_paths = ApiPaths::from_env(&env)?;
        // The daemon's one config read (F-31). A malformed file fails the
        // start with a message naming the file and the offending key, rather
        // than being defaulted away and leaving the daemon quietly running on
        // settings the user did not write.
        let global = ConfigFile::load(
            &crate::data::config::file::global_config_path(&env)?,
            crate::data::config::FileKind::Global,
        )?;
        // Env and `--set` overrides of a daemon arrive through its argv and
        // environment; the env layer hook resolves the latter.
        let daemon_config = EffectiveConfig::resolve(
            crate::data::config::ConfigLayers {
                env: crate::data::config::effective::env_overrides(&env, &[&global])?,
                global: global.clone(),
                ..Default::default()
            },
            Default::default(),
            env,
        )?;
        // Only the runtime's process-wide parts (for builtin: the state
        // directory and the ambient-override check) come from the global
        // config. Every launch resolves its own builtin settings (network,
        // vCPUs, memory) from the task's session through
        // `AgentEngine::builtin_launch_settings`, so a task inherits its
        // repo's narrowing.
        let detected = agent_runtime::detect_effective(&daemon_config)?;
        for notice in detected.take_notices() {
            tracing::info!("{notice}");
        }
        let runtime = detected.engine();
        let container_runtime = detected.container_runtime();
        let sandbox_runtime = detected.sandbox_runtime();
        let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
        // A daemon has no `Session`, so its credential-refresh settings come
        // from the global config it just loaded plus the process env.
        let credential_monitor = credential_refresh_monitor(&daemon_config);
        // The *detected* runtime, not a fresh Docker handle: `AgentEngine`
        // branches on `runtime.capabilities()` to decide which paradigm's
        // options to resolve (F-40b), so handing it a Docker handle under a
        // sandbox-class runtime resolves container options that the sandbox
        // runtime then refuses to build.
        let agent_engine = Arc::new(
            AgentEngine::new(overlay_engine.clone(), runtime.clone())
                .with_lease_factory(lease_factory_for(credential_monitor.as_ref())),
        );

        let workflow_root = api_paths.data_paths().root().to_path_buf();
        Ok(Self {
            ephemeral_secrets: Arc::new(Default::default()),
            runtime,
            container_runtime,
            sandbox_runtime,
            git_engine: Arc::new(GitEngine::with_env(daemon_config.env().clone())),
            overlay_engine,
            auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths)),
            agent_engine,
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(
                workflow_root,
            )),
            credential_monitor,
            global_config: Arc::new(global),
        })
    }

    /// Clone handles from the one daemon core; never detect or open a runtime here.
    pub fn from_core(
        core: &crate::engine::daemon::core::Core,
        global_config: Arc<ConfigFile>,
    ) -> Self {
        Self {
            ephemeral_secrets: Arc::new(Default::default()),
            runtime: Arc::new(
                crate::command::commands::api_server::owned_runtime::CommandOwnedRuntime::new(
                    core.runtime().clone(),
                    "",
                )
                .with_hub(core.terminal_hub().clone()),
            ),
            container_runtime: core.container_runtime().cloned(),
            sandbox_runtime: core.sandbox_runtime().cloned(),
            git_engine: core.git_engine().clone(),
            overlay_engine: core.overlay_engine().clone(),
            auth_engine: core.auth_engine().clone(),
            agent_engine: core.agent_engine().clone(),
            credential_monitor: core.credential_monitor().cloned(),
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(
                core.data_paths().root().to_path_buf(),
            )),
            global_config,
        }
    }
    pub(crate) fn from_detected(
        detected: DetectedRuntime,
        session: &Session,
    ) -> Result<Self, EngineError> {
        let runtime = detected.engine();
        let container_runtime = detected.container_runtime();
        let sandbox_runtime = detected.sandbox_runtime();
        let overlay_engine = Arc::new(OverlayEngine::new(session)?);
        let auth_engine = Arc::new(AuthEngine::new(session)?);
        // `AgentEngine` is given the *detected* runtime, whatever its
        // paradigm. It is no longer container-specific: `resolve_agent_options`
        // branches on `self.runtime.capabilities().kit_declarative` (F-40b), so
        // the handle it holds decides which paradigm's options come out.
        // Container-paradigm flows that genuinely need the concrete type still
        // go through `Engines::require_container_runtime()`.
        //
        // A disabled `authRefresh` leaves the monitor unbuilt, making lease
        // registration a no-op and preserving legacy env-var delivery — the
        // same kill switch the uninstalled process-global used to be.
        let credential_monitor = credential_refresh_monitor(&session.effective_config());
        let agent_engine = Arc::new(
            AgentEngine::new(overlay_engine.clone(), runtime.clone())
                .with_lease_factory(lease_factory_for(credential_monitor.as_ref())),
        );

        Ok(Self {
            ephemeral_secrets: Arc::new(Default::default()),
            runtime,
            container_runtime,
            sandbox_runtime,
            git_engine: Arc::new(GitEngine::with_env(session.env().clone())),
            overlay_engine,
            auth_engine,
            agent_engine,
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(
                session.git_root().to_path_buf(),
            )),
            credential_monitor,
            // The session already merged the global config; taking it from
            // there rather than re-loading is the whole point of F-31.
            global_config: Arc::new(session.effective_config().layers().global.clone()),
        })
    }

    /// The container-paradigm runtime handle, or — when the active runtime is
    /// sandbox-class — a `NotImplemented` error. Container-paradigm flows
    /// (agent setup, image builds, background containers) call this instead
    /// of unwrapping `container_runtime` so a sandbox-configured user gets an
    pub fn require_container_runtime(&self) -> Result<&Arc<ContainerRuntime>, EngineError> {
        self.container_runtime
            .as_ref()
            .ok_or(EngineError::NotImplemented(
                "this command does not yet route to the sandbox runtime \
                 (sbx); set runtime.kind to \"docker\" or \
                 \"apple\" to use it here",
            ))
    }

    /// The typed sandbox-tier handle, for the paradigm-specific operations
    /// the cross-paradigm trait deliberately does not carry. Errors when the
    /// active runtime is not sandbox-class.
    pub fn require_sandbox_runtime(&self) -> Result<&Arc<SandboxRuntime>, EngineError> {
        self.sandbox_runtime
            .as_ref()
            .ok_or(EngineError::NotImplemented(
                "this operation is specific to the sandbox runtime \
                 (docker-sbx-experimental)",
            ))
    }

    /// Whether the active runtime is sandbox-class.
    pub fn is_sandboxed(&self) -> bool {
        self.sandbox_runtime.is_some()
    }

    /// Drain notices the container runtime has queued since the last call
    /// (startup, or a deferred backend opening later). A sandbox-class bundle
    /// has no container runtime and so no notices.
    pub fn take_runtime_notices(&self) -> Vec<String> {
        self.container_runtime
            .as_ref()
            .map(|runtime| runtime.take_notices())
            .unwrap_or_default()
    }

    /// Agent-runtime detection with the documented CLI/TUI fallback policy,
    /// lifted out of `main.rs` (WI-0098 Finding B) so Layer 4 stays pure
    /// wiring. Picks the runtime named by `config`, applying three rules:
    ///
    /// * **Valid runtime** → `Ok((runtime, None))`.
    /// * **Unknown `runtime:` string** — a fatal configuration error, never a
    ///   silent Docker fallback, for every command that `requires_runtime`.
    ///   For such a CLI invocation the [`EngineError::UnknownRuntime`] is
    ///   returned so the caller can print it and exit. Commands that need no
    ///   runtime (`awman config …`) get a warning and inert default engines,
    ///   so the setting can be repaired. For the bare-TUI invocation
    ///   (`command_path` empty) inert default (Docker) engines are still
    ///   constructed — the TUI boots only far enough to show a fatal modal —
    ///   and the error text is returned as the second tuple field for that
    ///   modal.
    /// * **Runtime unavailable on this host** (e.g. `apple-containers` on
    ///   Linux) → fatal only when the command `requires_runtime`; otherwise a
    ///   warning is printed to stderr and detection falls back to the default
    ///   Docker runtime, keeping `awman config` reachable to fix the setting.
    ///
    /// Returns the detected runtime handles paired with the optional TUI
    /// fatal-modal message (`Some` only on the unknown-runtime TUI path).
    /// `main` combines the returned [`DetectedRuntime`] with the
    /// session-derived engines to assemble the full [`Engines`] bundle.
    pub fn detect(
        catalogue: &CommandCatalogue,
        config: &ConfigFile,
        command_path: &[&str],
    ) -> Result<(DetectedRuntime, Option<RuntimeStartupFailure>), EngineError> {
        Self::detect_result(catalogue, agent_runtime::detect(config), command_path)
    }

    /// Session-backed runtime selection includes repo overrides for builtin settings.
    pub fn detect_effective(
        catalogue: &CommandCatalogue,
        config: &EffectiveConfig,
        command_path: &[&str],
    ) -> Result<(DetectedRuntime, Option<RuntimeStartupFailure>), EngineError> {
        Self::detect_result(
            catalogue,
            agent_runtime::detect_effective(config),
            command_path,
        )
    }

    fn detect_result(
        catalogue: &CommandCatalogue,
        result: Result<DetectedRuntime, EngineError>,
        command_path: &[&str],
    ) -> Result<(DetectedRuntime, Option<RuntimeStartupFailure>), EngineError> {
        match result {
            Ok(detected) => Ok((detected, None)),
            Err(e @ EngineError::UnknownRuntime { .. }) => {
                // Invalid `runtime:` is fatal for anything that would run an
                // agent. CLI invocations bubble the error up to be printed and
                // exited on; the bare-TUI invocation constructs inert default
                // engines (never exercised — the modal's only action is quit)
                // and returns the message for the startup modal. Commands that
                // need no runtime (`awman config …`) warn and continue, so the
                // setting stays fixable from the CLI.
                if !command_path.is_empty() {
                    if catalogue.requires_runtime(command_path) {
                        return Err(e);
                    }
                    eprintln!(
                        "warning: {e}; continuing without it so `awman config` can update the \
                         setting"
                    );
                    let fallback = agent_runtime::detect(&ConfigFile::default())?;
                    return Ok((fallback, None));
                }
                let fallback = agent_runtime::detect(&ConfigFile::default())?;
                Ok((fallback, Some(RuntimeStartupFailure::new(e))))
            }
            Err(e) => {
                if command_path.is_empty() {
                    let inert = agent_runtime::detect(&ConfigFile::default())?;
                    return Ok((inert, Some(RuntimeStartupFailure::new(e))));
                }
                // A configured runtime this host can't construct must not lock
                // the user out of `awman config` — the documented way to switch
                // the runtime back. The catalogue decides which commands need a
                // runtime; for the rest, warn and continue on the default
                // Docker runtime, which config commands never touch.
                if catalogue.requires_runtime(command_path) {
                    return Err(e);
                }
                eprintln!(
                    "warning: configured runtime is unavailable on this host ({e}); \
                     continuing with the default Docker runtime so `awman config` \
                     can update the setting"
                );
                let fallback = agent_runtime::detect(&ConfigFile::default())?;
                Ok((fallback, None))
            }
        }
    }
}
