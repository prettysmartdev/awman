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

    #[cfg(test)]
    /// Assemble a hermetic container-tier engine bundle rooted at `root`.
    pub fn for_tests(root: &std::path::Path) -> Self {
        let runtime = Arc::new(ContainerRuntime::docker());
        let auth_paths = AuthPathResolver::at_home(root);
        let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
        let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
        Self {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine: Arc::new(GitEngine::new()),
            overlay_engine,
            auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, ApiPaths::at_root(root))),
            agent_engine,
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(root)),
            credential_monitor: None,
            global_config: Arc::new(ConfigFile::default()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_without_a_container_runtime_has_no_notices_and_reports_its_tier() {
        let root = tempfile::tempdir().unwrap();
        let mut engines = Engines::for_tests(root.path());
        assert!(engines.take_runtime_notices().is_empty());
        assert!(!engines.is_sandboxed());

        engines.container_runtime = None;
        assert!(engines.take_runtime_notices().is_empty());
    }

    /// A malformed global config fails the daemon's start with a message
    /// naming the file, instead of being `unwrap_or_default()`ed away (F-31).
    #[test]
    fn a_malformed_global_config_fails_the_daemon_bootstrap() {
        let _lock = crate::data::config::env::DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::data::config::env::ConfigHomeGuard::set(tmp.path());
        // `AWMAN_HOME` *is* the config home, so the file sits directly
        // under it.
        std::fs::write(tmp.path().join("config.json"), "{ \"runtime\": ").unwrap();

        let result = Engines::for_env(crate::data::config::env::Env::from_process());

        let error = result
            .err()
            .expect("a malformed config must fail the start");
        let text = error.to_string();
        assert!(
            text.contains("config.json"),
            "the error must name the file: {text}"
        );
    }

    /// `Engines::from_detected` must hand `AgentEngine` the runtime it
    /// actually detected, not a fresh Docker handle.
    ///
    /// `AgentEngine::resolve_agent_options` branches on
    /// `self.runtime.capabilities().kit_declarative` (F-40b). When the two
    /// disagree, a sandbox user gets `ResolvedAgentOptions::Container` out of
    /// the agent engine and `SandboxRuntime::build` then refuses it with
    /// `OptionVariantMismatch` — every `chat`, `exec prompt` and `exec
    /// workflow` fails. The other sandbox tests build an `AgentEngine`
    /// directly, so only a test that goes through `Engines` catches it.
    #[test]
    fn a_sandbox_detected_runtime_gives_the_agent_engine_a_sandbox_runtime() {
        use crate::engine::agent_runtime::{DetectedRuntime, ResolvedAgentOptions};
        use crate::engine::sandbox::SandboxRuntime;

        let tmp = tempfile::tempdir().unwrap();
        let session = session_with_host_home(tmp.path());

        let detected = DetectedRuntime::Sandbox(Arc::new(SandboxRuntime::for_tests()));
        let engines = Engines::from_detected(detected, &session).unwrap();

        assert!(
            engines
                .agent_engine
                .runtime()
                .capabilities()
                .kit_declarative,
            "the agent engine must hold the detected sandbox runtime"
        );

        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let resolved = engines
            .agent_engine
            .resolve_agent_options(&session, &agent, &Default::default(), &Default::default())
            .expect("resolving options under a sandbox runtime");
        assert!(
            matches!(resolved, ResolvedAgentOptions::Sandbox(_)),
            "a sandbox-class runtime must resolve sandbox options; got {resolved:?}"
        );
    }

    // ── WI-0098 Finding B: Engines::detect runtime-detection policy ───────────
    //
    // The three documented paths lifted out of `main.rs`: valid runtime, an
    // unknown `runtime:` string (fatal for CLI, modal for TUI), and a runtime
    // unavailable on this host (fatal only when the command requires a runtime).

    fn config_with_runtime(runtime: Option<&str>) -> ConfigFile {
        runtime.map_or_else(ConfigFile::default, ConfigFile::with_runtime)
    }

    /// A session whose env snapshot carries a host `HOME`, as every real
    /// snapshot does (`Env::from_process` fills it in). The overlay and auth
    /// engines resolve host paths from the session's snapshot, never the
    /// process. The home is a subdirectory, so `~/.awman/config.json` is not
    /// the fixture's repo config file.
    fn session_with_host_home(root: &std::path::Path) -> Session {
        let home = root.join("host-home");
        std::fs::create_dir_all(&home).expect("create host home");
        Session::for_tests_with_env(
            root,
            crate::data::config::env::EnvSnapshot::with_overrides([(
                "HOME",
                home.to_str().expect("fixture path must be UTF-8"),
            )]),
        )
    }

    fn with_daemon_global_config<T>(config: ConfigFile, test: impl FnOnce() -> T) -> T {
        let home = tempfile::tempdir().expect("create global config home");
        // `ConfigHomeGuard`, not `CWD_LOCK`: the variable this serialises is
        // `AWMAN_HOME`, and the `clean` and `overlay` tests that also
        // repoint it never touched `CWD_LOCK`. Holding the wrong lock let
        // them move the config home mid-test, so `Engines::for_env` read a
        // config it was never given and built the wrong runtime tier.
        let _guard = crate::data::config::env::ConfigHomeGuard::set(home.path());
        config
            .save(
                &home.path().join("config.json"),
                crate::data::config::FileKind::Global,
            )
            .expect("save global config");
        test()
    }

    #[test]
    fn engines_build_and_for_env_produce_container_tier_under_default_config() {
        let root = tempfile::tempdir().expect("create test root");
        let session = session_with_host_home(root.path());
        let built = Engines::build(&session).expect("default session engines build");
        assert!(built.container_runtime.is_some());
        assert!(built.sandbox_runtime.is_none());

        with_daemon_global_config(ConfigFile::default(), || {
            let daemon = Engines::for_env(crate::data::config::env::Env::from_process())
                .expect("default daemon engines build");
            assert!(daemon.container_runtime.is_some());
            assert!(daemon.sandbox_runtime.is_none());
        });
    }

    /// `docker-sbx-experimental` is platform-gated in `SandboxRuntime::dsbx`
    /// (linux refuses with `BackendUnsupportedOnPlatform`
    /// — see `engine::sandbox::runtime`'s own `dsbx_errors_on_*` tests, which
    /// use the same branch-on-`cfg!` pattern). `Engines::build`/`for_env`
    /// only assemble a sandbox-tier bundle on the platforms where the backend
    /// actually constructs.
    #[test]
    fn engines_build_and_for_env_produce_sandbox_tier_for_experimental_runtime() {
        let root = tempfile::tempdir().expect("create test root");
        let config = config_with_runtime(Some("sbx"));
        let home = tempfile::tempdir().expect("create config home");
        config
            .save(
                &home.path().join("config.json"),
                crate::data::config::FileKind::Global,
            )
            .expect("save global config");
        let session = Session::open_at_git_root(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            crate::data::session::SessionOpenOptions {
                env: Some(crate::data::config::EnvSnapshot::with_overrides([
                    (
                        crate::data::config::AWMAN_HOME,
                        home.path().to_str().unwrap(),
                    ),
                    ("HOME", root.path().to_str().unwrap()),
                ])),
                ..Default::default()
            },
        )
        .expect("open test session");

        if cfg!(target_os = "linux") {
            match Engines::build(&session) {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, .. }) => {
                    assert_eq!(backend, "docker-sbx-experimental")
                }
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got {e:?}"),
                Ok(_) => panic!("dsbx build must fail on this platform"),
            }

            with_daemon_global_config(config, || {
                match Engines::for_env(crate::data::config::env::Env::from_process()) {
                    Err(EngineError::BackendUnsupportedOnPlatform { backend, .. }) => {
                        assert_eq!(backend, "docker-sbx-experimental")
                    }
                    Err(e) => panic!("expected BackendUnsupportedOnPlatform, got {e:?}"),
                    Ok(_) => panic!("dsbx for_env must fail on this platform"),
                }
            });
            return;
        }

        let built = Engines::build(&session).expect("sandbox session engines build");
        assert!(built.container_runtime.is_none());
        assert!(built.sandbox_runtime.is_some());

        with_daemon_global_config(config, || {
            let daemon = Engines::for_env(crate::data::config::env::Env::from_process())
                .expect("sandbox daemon engines build");
            assert!(daemon.container_runtime.is_none());
            assert!(daemon.sandbox_runtime.is_some());
        });
    }

    #[test]
    fn detect_valid_runtime_returns_runtime_and_no_modal_message() {
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("docker"));
        let (detected, modal) = Engines::detect(cat, &cfg, &["agents", "status"])
            .expect("a valid runtime must detect successfully");
        assert_eq!(detected.engine().runtime_name(), "docker");
        assert!(
            modal.is_none(),
            "no fatal-modal message on the valid-runtime path"
        );
    }

    // The unavailable-on-host path needs a runtime that this host cannot
    // construct. `apple-containers` is unavailable on every non-macOS host, so
    // these two tests exercise the fatal-vs-warn branch there.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn detect_unavailable_runtime_is_fatal_when_command_requires_runtime() {
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("apple"));
        // `agents status` requires a runtime → the unavailable runtime is fatal.
        assert!(cat.requires_runtime(&["agents", "status"]));
        match Engines::detect(cat, &cfg, &["agents", "status"]) {
            Err(EngineError::UnknownRuntime { .. }) => {
                panic!("an unavailable (not unknown) runtime must not surface as UnknownRuntime")
            }
            Err(_) => {}
            Ok(_) => panic!("an unavailable runtime must be fatal for a runtime-requiring command"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn detect_unavailable_runtime_warns_and_falls_back_when_command_allows() {
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("apple"));
        // `config` does not require a runtime → warn on stderr and fall back to
        // the default Docker runtime so `awman config` stays reachable.
        assert!(!cat.requires_runtime(&["config", "show"]));
        let (detected, modal) = Engines::detect(cat, &cfg, &["config", "show"])
            .expect("config commands must fall back rather than fail");
        assert_eq!(
            detected.engine().runtime_name(),
            "docker",
            "the fallback must be the default Docker runtime"
        );
        assert!(
            modal.is_none(),
            "the unavailable-but-tolerated path yields no TUI modal message"
        );
    }
}
