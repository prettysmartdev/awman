//! [`Engines`] — the one owner of the Layer 1 graph a command runs against:
//! how a session-backed host and a daemon bind runtime and monitor settings.

use std::sync::Arc;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::error::CommandError;
use crate::command::startup::RuntimeStartupFailure;
use crate::data::config::effective::process_binding::ProcessBinding;
use crate::data::config::{ConfigFile, ConfigLayers, ConfigOverrides, EffectiveConfig, FlagConfig};
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
        crate::engine::credential_refresh::CredentialRefreshMonitor::with_binding_in(
            crate::engine::credential_refresh::MonitorConfig {
                refresh_threshold: settings.threshold,
                tick_interval: settings.tick,
            },
            crate::engine::auth::credential::CredentialBinding::platform_default(
                AuthPathResolver::at_home(
                    config
                        .env()
                        .home_dir()
                        .or_else(dirs::home_dir)
                        .unwrap_or_else(std::env::temp_dir),
                ),
            ),
            config.env(),
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
    /// Lease admission snapshot, reapplied before executing any workflow.
    pub lease_workflow_review: Option<Arc<crate::command::worker::policy::LeaseWorkflowReview>>,

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

    /// The immutable process config snapshot this bundle was assembled from,
    /// including resolved startup overrides and process-scoped defaults.
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
    /// and the flags. Dispatch also uses the process fields here to inherit
    /// the bound defaults and to refuse incompatible local overrides.
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
        let config = Self::resolve_process_config(env, ConfigOverrides::default())?;
        Self::for_process_config(&config)
    }

    /// Resolve once, before constructing any runtime or credential monitor.
    pub fn resolve_process_config(
        env: crate::data::config::env::EnvSnapshot,
        set: ConfigOverrides,
    ) -> Result<EffectiveConfig, EngineError> {
        Ok(EffectiveConfig::resolve(
            ConfigLayers::load(&env, None, set)?,
            FlagConfig::default(),
            env,
        )?)
    }

    pub fn for_process_config(config: &EffectiveConfig) -> Result<Self, EngineError> {
        let detected = agent_runtime::detect_effective(config)?;
        for notice in detected.take_notices() {
            tracing::info!("{notice}");
        }
        Self::with_process_runtime(
            config,
            detected.engine(),
            detected.container_runtime(),
            detected.sandbox_runtime(),
        )
    }

    /// Assemble a process graph around an explicitly supplied backend. Embedded
    /// hosts can inject their runtime; its actual identity must match config.
    pub fn with_process_runtime(
        config: &EffectiveConfig,
        runtime: Arc<dyn AgentRuntimeEngine>,
        container_runtime: Option<Arc<ContainerRuntime>>,
        sandbox_runtime: Option<Arc<SandboxRuntime>>,
    ) -> Result<Self, EngineError> {
        let binding = ProcessBinding::from_config(config)?;
        Self::check_runtime_identity(&binding, runtime.as_ref())?;
        let auth_paths = AuthPathResolver::at_home(
            config
                .env()
                .home_dir()
                .or_else(dirs::home_dir)
                .ok_or(crate::data::error::DataError::HomeNotFound)?,
        );
        let api_paths = ApiPaths::from_env(config.env())?;
        let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
        // A daemon has no `Session`, so its credential-refresh settings come
        // from the global config it just loaded plus the process env.
        let credential_monitor = credential_refresh_monitor(config);
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
            lease_workflow_review: None,
            runtime,
            container_runtime,
            sandbox_runtime,
            git_engine: Arc::new(GitEngine::with_env(config.env().clone())),
            overlay_engine,
            auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths)),
            agent_engine,
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(
                workflow_root,
            )),
            credential_monitor,
            global_config: Arc::new({
                let mut snapshot = config.config().clone();
                if let Some(dir) = &binding.builtin_state_dir {
                    snapshot
                        .runtime
                        .get_or_insert_with(Default::default)
                        .builtin
                        .get_or_insert_with(Default::default)
                        .state_dir = Some(dir.clone());
                }
                snapshot
            }),
        })
    }

    /// Clone handles from the one daemon core; never detect or open a runtime here.
    pub fn from_core(
        core: &crate::engine::daemon::core::Core,
        global_config: Arc<ConfigFile>,
    ) -> Self {
        Self {
            ephemeral_secrets: Arc::new(Default::default()),
            lease_workflow_review: None,
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
        Self::from_detected_with_config(detected, session, &session.effective_config())
    }

    pub(crate) fn from_detected_with_config(
        detected: DetectedRuntime,
        session: &Session,
        config: &EffectiveConfig,
    ) -> Result<Self, EngineError> {
        let binding = ProcessBinding::from_config(config)?;
        let runtime = detected.engine();
        Self::check_runtime_identity(&binding, runtime.as_ref())?;
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
        let credential_monitor = credential_refresh_monitor(config);
        let agent_engine = Arc::new(
            AgentEngine::new(overlay_engine.clone(), runtime.clone())
                .with_lease_factory(lease_factory_for(credential_monitor.as_ref())),
        );

        Ok(Self {
            ephemeral_secrets: Arc::new(Default::default()),
            lease_workflow_review: None,
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
            global_config: Arc::new(binding.snapshot(config)),
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

    /// Detect exactly the configured backend. Invalid or unsupported runtimes
    /// fail explicitly for every frontend; no substitute engines are created.
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
        _catalogue: &CommandCatalogue,
        result: Result<DetectedRuntime, EngineError>,
        _command_path: &[&str],
    ) -> Result<(DetectedRuntime, Option<RuntimeStartupFailure>), EngineError> {
        result.map(|detected| (detected, None))
    }

    /// The immutable process values are retained in the engine bundle's config
    /// snapshot. Reading the typed binding does not resolve config a second time.
    pub fn process_binding(&self) -> ProcessBinding {
        ProcessBinding::from_snapshot(&self.global_config)
    }

    pub(crate) fn check_runtime_identity(
        binding: &ProcessBinding,
        runtime: &dyn AgentRuntimeEngine,
    ) -> Result<(), EngineError> {
        use crate::data::config::runtime::RuntimeKind;
        let actual = match runtime.runtime_name() {
            "docker" => Some(RuntimeKind::Docker),
            "apple-containers" => Some(RuntimeKind::AppleContainers),
            "docker-sbx-experimental" => Some(RuntimeKind::DockerSbxExperimental),
            "builtin" => Some(RuntimeKind::BuiltinExperimental),
            _ => None,
        };
        if actual != Some(binding.runtime_kind) {
            return Err(EngineError::Other(format!(
                "runtime construction disagrees with runtime.kind: configured {}; constructed {}",
                binding.runtime_kind,
                runtime.runtime_name()
            )));
        }
        Ok(())
    }

    /// Sessions opened later inherit the process baseline. Explicit repo/task
    /// and invocation values remain visible so conflicts can be refused after
    /// the command's --set layer has been applied.
    pub fn inherit_process_settings(&self, session: &mut Session) -> Result<(), CommandError> {
        use crate::data::config::Layer;
        let config = session.effective_config();
        let snapshot = self.global_config.to_value();
        let mut inherited = Vec::new();
        for key in ProcessBinding::KEYS {
            let source = config.source_of(key);
            let env_name = crate::data::config::fields::CONFIG_KEYS
                .iter()
                .find(|entry| entry.path == *key)
                .expect("process key")
                .env_name(&[]);
            // Startup argv remains authoritative over the same global/env
            // baseline. A different client's explicit environment must survive
            // inheritance so validation refuses a conflicting backend.
            let inherit = source == Layer::Default
                || source == Layer::Global
                || (source == Layer::Env
                    && config.env().get(&env_name) == self.git_engine.env().get(&env_name));
            if inherit {
                let pointer = format!("/{}", key.replace('.', "/"));
                if let Some(value) = snapshot.pointer(&pointer) {
                    if config.value().pointer(&pointer) != Some(value) {
                        if source == Layer::Global {
                            tracing::warn!(key, "using the bound process setting; global changes require restarting awman");
                        }
                        inherited.push(((*key).to_string(), value.clone()));
                    }
                }
            }
        }
        session.add_overrides(ConfigOverrides::new(inherited))?;
        Ok(())
    }

    pub fn validate_process_config(&self, config: &EffectiveConfig) -> Result<(), CommandError> {
        let bound = self.process_binding();
        let requested = ProcessBinding::from_config(config)?;
        if let Some(difference) = bound.difference(&requested) {
            // Preserve the established repository state-directory refusal.
            if difference.key == "runtime.builtin.stateDir"
                && config.source_of(&difference.key) == crate::data::config::Layer::Repo
            {
                return Err(EngineError::BuiltinStateDirMismatch {
                    process: bound.builtin_state_dir.unwrap(),
                    session: requested.builtin_state_dir.unwrap(),
                }
                .into());
            }
            return Err(CommandError::ProcessScopedSetting {
                key: difference.key,
                bound: difference.bound,
                requested: difference.requested,
            });
        }
        Ok(())
    }

    /// Pre-allocation admission for API argv. Start from the bound process
    /// snapshot, then add command-local values with normal --set precedence.
    pub fn validate_process_set_args(&self, set: &[String]) -> Result<(), CommandError> {
        let mut overrides = ConfigOverrides::parse_set_args(set)
            .map_err(|e| CommandError::InvalidRequest(e.to_string()))?;
        // Admission has no repository/task config yet. Only resolve the
        // process keys here: session settings may depend on those later layers.
        overrides
            .0
            .retain(|(key, _)| ProcessBinding::KEYS.contains(&key.as_str()));
        if overrides.is_empty() {
            return Ok(());
        }
        let config = EffectiveConfig::resolve(
            ConfigLayers {
                global: (*self.global_config).clone(),
                set: overrides,
                ..Default::default()
            },
            FlagConfig::default(),
            self.git_engine.env().clone(),
        )?;
        self.validate_process_config(&config)
    }
}
