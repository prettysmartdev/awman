//! `Dispatch` — Layer 2's gateway from frontends into typed `*Command` values.
//!
//! Frontends construct a `Dispatch` with a frontend-specific
//! [`CommandFrontend`] implementation (CLI, TUI, API). Dispatch reads
//! flag values from the frontend, applies catalogue-driven validation
//! (mutually-exclusive flags, type errors, implications), and returns a typed
//! [`BuiltCommand`] enum containing the constructed `*Command` struct.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::commands::api_server::{ApiServerCommand, ApiServerCommandFrontend};
use crate::command::commands::chat::{ChatCommand, ChatCommandFrontend};
use crate::command::commands::clean::{CleanCommand, CleanCommandFrontend};
use crate::command::commands::config::{ConfigCommand, ConfigCommandFrontend};
use crate::command::commands::exec_prompt::{ExecPromptCommand, ExecPromptCommandFrontend};
use crate::command::commands::exec_workflow::{ExecWorkflowCommand, ExecWorkflowCommandFrontend};
use crate::command::commands::init::{InitCommand, InitCommandFrontend};
use crate::command::commands::new::{NewCommand, NewCommandFrontend};
use crate::command::commands::ready::{ReadyCommand, ReadyCommandFrontend};
use crate::command::commands::remote::{RemoteCommand, RemoteCommandFrontend};
use crate::command::commands::specs::{SpecsCommand, SpecsCommandFrontend};
use crate::command::commands::squad::attach::{
    SquadAttachCommand, SquadAttachFrontend, SquadAttachOutcome,
};
use crate::command::commands::squad::commands::{SquadCommand, SquadCommandFrontend};
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::commands::squad::supervisor::SquadGatewayResolver;
use crate::command::commands::status::{StatusCommand, StatusCommandFrontend};
use crate::command::commands::Command;
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind, GatewayNeed};
use crate::command::error::CommandError;
use crate::data::config::global::GlobalConfig;
use crate::data::config::EffectiveConfig;
use crate::data::fs::{ApiPaths, AuthPathResolver, DataPaths, SquadPaths};
use crate::data::message::UserMessageSink;
use crate::data::session::Session;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{self, AgentRuntimeEngine, DetectedRuntime};
use crate::engine::auth::AuthEngine;
use crate::engine::container::ContainerRuntime;
use crate::engine::daemon::DaemonKind;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::sandbox::SandboxRuntime;

pub mod build;
pub mod catalogue;
pub mod frontend_action;
pub mod parsed_input;
pub mod projections;
pub mod resolved;
pub mod runtime_context;

pub use frontend_action::FrontendAction;
pub use parsed_input::ParsedCommandBoxInput;
pub use resolved::{BuildContext, CallerContext, ResolvedArgs, ResolvedFlags};
pub use runtime_context::RuntimeContext;

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

// ─── Pre-wired engines bundle ───────────────────────────────────────────────

/// All Layer 1 engine handles a `Dispatch` needs to construct a `*Command`.
/// `ReadyEngine` and `InitEngine` are NOT pre-constructed here — those
/// engines accept per-invocation flag values.
#[derive(Clone)]
pub struct Engines {
    /// Cross-paradigm trait-object handle. Used for build(), list_running(),
    /// stats(), stop(), exec_args(), is_available(), capabilities() and
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
    /// simply called `GlobalConfig::load()` wherever it stood — four separate
    /// reads, each with its own `unwrap_or_default()` swallowing a malformed
    /// file, and each able to see a different file than the one the daemon
    /// started with.
    ///
    /// A **session-backed** host reads `session.effective_config()` instead:
    /// that already merged this in, under the repo config, the environment
    /// and the flags. This field is for the code that has no session.
    pub global_config: Arc<GlobalConfig>,
}

impl Engines {
    /// Assemble the engines for a session-backed command invocation.
    ///
    /// This is the single owner of the Layer 1 graph formerly assembled by
    /// the binary entrypoint. Runtime selection is deliberately here rather
    /// than in a frontend so every session-backed host gets the same tier.
    pub fn build(global: &GlobalConfig, session: &Session) -> Result<Self, EngineError> {
        let detected = agent_runtime::detect(global)?;
        Self::from_detected(detected, session)
    }

    /// Assemble the engines used by either standalone daemon.
    ///
    /// Daemons do not have a `Session`: overlays resolve credentials from the
    /// process auth paths, auth uses the API key store, and workflow state is
    /// rooted under that daemon's own root. `paths` is the shared daemon data
    /// context retained by this common factory boundary.
    pub fn for_daemon(kind: DaemonKind, paths: &DataPaths) -> Result<Self, EngineError> {
        let auth_paths = AuthPathResolver::from_process_env()?;
        let api_paths = ApiPaths::from_process_env()?;
        // The daemon's one config read (F-31). A malformed file fails the
        // start with a message naming the file and the offending key, rather
        // than being defaulted away and leaving the daemon quietly running on
        // settings the user did not write.
        let global = GlobalConfig::load()?;
        let detected = agent_runtime::detect(&global)?;
        let runtime = detected.engine();
        let container_runtime = detected.container_runtime();
        let sandbox_runtime = detected.sandbox_runtime();
        let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
        // A daemon has no `Session`, so its credential-refresh settings come
        // from the global config it just loaded plus the process env.
        let daemon_config = EffectiveConfig::new(
            Default::default(),
            crate::data::config::env::Env::from_process(),
            Default::default(),
            global.clone(),
        );
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

        let workflow_root = match kind {
            DaemonKind::Api => api_paths.root().to_path_buf(),
            DaemonKind::Squad => SquadPaths::from_process_env()?.root().to_path_buf(),
        };
        let _shared_data_root = paths.root();
        Ok(Self {
            runtime,
            container_runtime,
            sandbox_runtime,
            git_engine: Arc::new(GitEngine::new()),
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
            git_engine: Arc::new(GitEngine::new()),
            overlay_engine,
            auth_engine,
            agent_engine,
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(
                session.git_root().to_path_buf(),
            )),
            credential_monitor,
            // The session already merged the global config; taking it from
            // there rather than re-loading is the whole point of F-31.
            global_config: Arc::new(session.effective_config().global().clone()),
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
            global_config: Arc::new(GlobalConfig::default()),
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
                 (docker-sbx-experimental); set runtime to \"docker\" or \
                 \"apple-containers\" to use it here",
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

    /// Agent-runtime detection with the documented CLI/TUI fallback policy,
    /// lifted out of `main.rs` (WI-0098 Finding B) so Layer 4 stays pure
    /// wiring. Picks the runtime named by `config`, applying three rules:
    ///
    /// * **Valid runtime** → `Ok((runtime, None))`.
    /// * **Unknown `runtime:` string** — a fatal configuration error, never a
    ///   silent Docker fallback. For a CLI invocation (`command_path`
    ///   non-empty) the [`EngineError::UnknownRuntime`] is returned so the
    ///   caller can print it and exit. For the bare-TUI invocation
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
        config: &GlobalConfig,
        command_path: &[&str],
    ) -> Result<(DetectedRuntime, Option<String>), EngineError> {
        match agent_runtime::detect(config) {
            Ok(detected) => Ok((detected, None)),
            Err(e @ EngineError::UnknownRuntime { .. }) => {
                // Invalid `runtime:` is fatal. CLI invocations bubble the error
                // up to be printed and exited on; the bare-TUI invocation
                // constructs inert default engines (never exercised — the
                // modal's only action is quit) and returns the message for the
                // startup modal.
                if !command_path.is_empty() {
                    return Err(e);
                }
                let fallback = agent_runtime::detect(&GlobalConfig::default())?;
                Ok((fallback, Some(e.to_string())))
            }
            Err(e) => {
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
                let fallback = agent_runtime::detect(&GlobalConfig::default())?;
                Ok((fallback, None))
            }
        }
    }
}

// ─── CommandFrontend trait ──────────────────────────────────────────────────

/// Frontend trait that supplies flag values to Dispatch. Extended by per-
/// command frontend traits (e.g. [`crate::command::commands::exec_workflow::ExecWorkflowCommandFrontend`])
/// for command-specific Q&A and reporting.
pub trait CommandFrontend: UserMessageSink + Send + Sync {
    /// Which frontend this is.
    ///
    /// The one fact about itself a frontend declares. Everything that used to
    /// be derived from it per-frontend — whether a human is there, whether the
    /// working directory is the user's — is decided once in Layer 2 from this
    /// (WI 0114 F-49). No default: a new frontend must say what it is.
    fn kind(&self) -> FrontendKind;

    /// Whether there is somewhere to read an answer from right now.
    ///
    /// The frontend reports the *fact*; Layer 2 applies the rule
    /// ([`ResolvedFlags::is_non_interactive`]). For the CLI the fact is
    /// whether stdin is a terminal, which only the CLI can see; for every
    /// other frontend it follows from the kind, which is why the default
    /// answers from [`FrontendKind::can_ask_a_human`].
    ///
    /// Before WI 0114 F-50 the rule itself lived in Layer 3, in
    /// `frontend::effective_non_interactive`, and each frontend re-applied it.
    fn input_available(&self) -> bool {
        self.kind().can_ask_a_human()
    }

    fn flag_bool(&self, command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError>;

    fn flag_string(
        &self,
        command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError>;

    fn flag_strings(&self, command_path: &[&str], flag: &str) -> Result<Vec<String>, CommandError>;

    fn flag_path(&self, command_path: &[&str], flag: &str)
        -> Result<Option<PathBuf>, CommandError>;

    fn flag_enum(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError>;

    fn flag_u16(&self, command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError>;

    fn flag_usize(&self, command_path: &[&str], flag: &str) -> Result<Option<usize>, CommandError>;

    fn argument(&self, command_path: &[&str], name: &str) -> Result<Option<String>, CommandError>;

    fn arguments(&self, command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError>;
}

// ─── Frontend supertrait ────────────────────────────────────────────────────

/// Frontend type accepted by [`Dispatch::run_command`]. A single concrete
/// frontend (CLI, TUI, or API) implements every per-command frontend
/// trait via this supertrait so that dispatch can move the frontend value
/// into the matching `Box<dyn *CommandFrontend>` for whichever variant
/// `build_command` returned. Layer 3 frontends typically derive this
/// automatically via a blanket impl over a single struct that implements
/// each trait.
pub trait DispatchFrontend:
    CommandFrontend
    + InitCommandFrontend
    + ReadyCommandFrontend
    + ChatCommandFrontend
    + StatusCommandFrontend
    + ConfigCommandFrontend
    + ExecPromptCommandFrontend
    + ExecWorkflowCommandFrontend
    + ApiServerCommandFrontend
    + SquadCommandFrontend
    + SquadAttachFrontend
    + RemoteCommandFrontend
    + NewCommandFrontend
    + SpecsCommandFrontend
    + CleanCommandFrontend
    + 'static
{
}

impl<T> DispatchFrontend for T where
    T: CommandFrontend
        + InitCommandFrontend
        + ReadyCommandFrontend
        + ChatCommandFrontend
        + StatusCommandFrontend
        + ConfigCommandFrontend
        + ExecPromptCommandFrontend
        + ExecWorkflowCommandFrontend
        + ApiServerCommandFrontend
        + SquadCommandFrontend
        + SquadAttachFrontend
        + RemoteCommandFrontend
        + NewCommandFrontend
        + SpecsCommandFrontend
        + CleanCommandFrontend
        + 'static
{
}

// ─── Outcome / error wrappers ───────────────────────────────────────────────

/// Catch-all outcome enum returned by `Dispatch::run_command`. Layer 3
/// inspects the variant to choose an appropriate rendering.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum CommandOutcome {
    Init(crate::command::commands::init::InitOutcome),
    Ready(crate::command::commands::ready::ReadyOutcome),
    Chat(crate::command::commands::chat::ChatOutcome),
    Status(crate::command::commands::status::StatusOutcome),
    Config(crate::command::commands::config::ConfigOutcome),
    ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome),
    ExecWorkflow(crate::command::commands::exec_workflow::ExecWorkflowOutcome),
    ApiServer(crate::command::commands::api_server::ApiServerOutcome),
    Squad(crate::command::commands::squad::commands::SquadOutcome),
    SquadAttach(SquadAttachOutcome),
    Remote(crate::command::commands::remote::RemoteOutcome),
    New(crate::command::commands::new::NewOutcome),
    Specs(crate::command::commands::specs::SpecsOutcome),
    Clean(crate::command::commands::clean::CleanOutcome),
    /// Trivial wrapper used by no-op leaf commands during the refactor.
    Empty,
}

impl CommandOutcome {
    /// The command's process/API exit code. Successful aggregate outcomes can
    /// still carry a failure: `new skill --pull-all` continues collecting
    /// libraries after one source fails, and must be visible to both CLI and
    /// API clients as a non-zero result.
    ///
    /// Every agent-session command reports the agent's own exit code. `Chat`
    /// was missing from this list until WI 0114 F-22: the TUI read
    /// `ChatOutcome::exit_code` itself while the CLI and API came through here
    /// and saw 0, so the same failing `awman chat` exited 0 from a script and
    /// showed red in the TUI.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Chat(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecWorkflow(outcome) => outcome.exit_code.unwrap_or(0),
            Self::ExecPrompt(outcome) => outcome.exit_code.unwrap_or(0),
            Self::SquadAttach(outcome) => outcome.exit_code,
            _ if self.is_partial_failure() => 1,
            _ => 0,
        }
    }

    pub fn is_partial_failure(&self) -> bool {
        matches!(self, Self::New(crate::command::commands::new::NewOutcome::Skill(skill))
            if skill.libraries.iter().any(|library| library.error.is_some()))
    }
}

/// One per `*Command` struct in `src/command/commands/`. Constructed by
/// [`Dispatch::build_command`] and consumed by [`Dispatch::run_command`].
///
/// There are deliberately no `Auth` or `Download` arms: neither command is in
/// the catalogue, so neither is reachable, and WI 0114 F-35 deletes both.
pub enum BuiltCommand {
    Init(InitCommand),
    Ready(ReadyCommand),
    Chat(ChatCommand),
    Specs(SpecsCommand),
    Status(StatusCommand),
    Config(ConfigCommand),
    ExecPrompt(ExecPromptCommand),
    ExecWorkflow(ExecWorkflowCommand),
    ApiServer(ApiServerCommand),
    Squad(SquadCommand),
    SquadAttach(SquadAttachCommand),
    Remote(RemoteCommand),
    New(NewCommand),
    Clean(CleanCommand),
}

/// What a frontend draws around a running command, decided by Layer 2.
///
/// Produced by [`Dispatch::launch_display`]. A frontend renders these; it does
/// not recompute them from flag names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchDisplay {
    /// The agent this invocation will run, resolved with the same precedence
    /// the command uses (`--agent`, then the session default, then `claude`).
    /// Empty only when the command path does not resolve at all.
    pub agent_display_name: String,
    /// Whether the run is unattended — see [`ResolvedFlags::unattended`].
    pub unattended: bool,
}

impl LaunchDisplay {
    /// The display for an invocation Layer 2 cannot resolve. The frontend
    /// still has to draw something; `run_command` reports the real error.
    pub fn unknown() -> Self {
        Self {
            agent_display_name: String::new(),
            unattended: false,
        }
    }
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

pub struct Dispatch<F: CommandFrontend> {
    catalogue: &'static CommandCatalogue,
    frontend: F,
    session: Arc<RwLock<Session>>,
    engines: Engines,
    squad_gateway: Option<Arc<dyn TaskGateway>>,
}

impl<F: CommandFrontend> Dispatch<F> {
    pub fn new(frontend: F, session: Arc<RwLock<Session>>, engines: Engines) -> Self {
        // The credential monitor is attached to `engines.agent_engine` when
        // the bundle is assembled (F-38); a disabled `authRefresh` leaves it
        // absent, making lease registration a no-op and preserving legacy
        // env-var delivery.
        Self {
            catalogue: CommandCatalogue::get(),
            frontend,
            session,
            engines,
            squad_gateway: None,
        }
    }

    pub fn catalogue(&self) -> &'static CommandCatalogue {
        self.catalogue
    }

    pub fn frontend(&self) -> &F {
        &self.frontend
    }

    pub fn frontend_mut(&mut self) -> &mut F {
        &mut self.frontend
    }

    pub fn session(&self) -> Arc<RwLock<Session>> {
        Arc::clone(&self.session)
    }

    pub fn engines(&self) -> &Engines {
        &self.engines
    }

    /// Apply the catalogue-driven runtime-tier admission without performing
    /// any asynchronous gateway work. Frontends may use this before handing a
    /// command to their executor so an immediately actionable refusal can be
    /// rendered synchronously; [`Dispatch::run_command`] always repeats the
    /// same check at the authoritative execution boundary.
    pub fn validate_runtime_admission(
        engines: &Engines,
        path: &[&str],
    ) -> Result<(), CommandError> {
        let catalogue = CommandCatalogue::get();
        let canonical: Vec<&str> = catalogue.canonical_path(path).into_iter().collect();
        if catalogue
            .lookup(&canonical)
            .is_some_and(|spec| spec.requires_container_tier)
        {
            require_container_tier(engines)?;
        }
        Ok(())
    }

    /// Offer a squad daemon gateway to whatever command is about to run.
    ///
    /// The handle is offered unconditionally; the catalogue decides whether a
    /// command receives it, through [`CommandSpec::gateway_need`]. A caller
    /// that already holds a gateway — the squad daemon, which is its own
    /// gateway, the TUI, which resolved one when its squad tab opened, or a
    /// test supplying a double — hands it over here, and [`Dispatch::admit`]
    /// then skips resolving a second one.
    ///
    /// Callers used to gate this on `path.first() == Some("squad")`, a
    /// command-name fact restated in Layer 3 (WI 0114 F-15). `gateway_need`
    /// is the same fact where the catalogue can maintain it.
    pub fn with_squad_gateway(mut self, gateway: Arc<dyn TaskGateway>) -> Self {
        self.squad_gateway = Some(gateway);
        self
    }

    /// Resolve every flag the catalogue declares for `path`.
    ///
    /// One walk over the spec's [`FlagSpec`]s: read each value through the
    /// frontend, reject mutually-exclusive pairs, apply `FlagDefault` where
    /// the frontend supplied nothing, then close over `implies` (WI 0113
    /// F-10). No command restates a default or an implication after this.
    pub fn resolve_flags(&self, path: &[&str]) -> Result<ResolvedFlags, CommandError> {
        let canonical: Vec<&str> = self.catalogue.canonical_path(path).into_iter().collect();
        let spec = self
            .catalogue
            .lookup(&canonical)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        ResolvedFlags::resolve(&self.frontend, &canonical, spec)
    }

    /// Read flags from the frontend and construct the typed `*Command`. No
    /// engine work happens at this point — the command is "ready to run".
    ///
    /// Canonicalise, resolve, look up, call: every per-command decision lives
    /// behind [`CommandSpec::build`], in the command's own `from_input`.
    pub fn build_command(&self, path: &[&str]) -> Result<BuiltCommand, CommandError> {
        let canonical: Vec<&str> = self.catalogue.canonical_path(path).into_iter().collect();
        let spec = self
            .catalogue
            .lookup(&canonical)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        let flags = ResolvedFlags::resolve(&self.frontend, &canonical, spec)?;
        let args = ResolvedArgs::resolve(&self.frontend, &canonical, spec.arguments)?;
        // Read the session from the shared state so every command operates
        // in the correct working directory (tab-specific in the TUI).
        let session = self
            .session
            .try_read()
            .map_err(|_| CommandError::Other("session is write-locked".into()))?
            .clone();
        let ctx = BuildContext {
            flags: &flags,
            args: &args,
            engines: &self.engines,
            session,
            managed_session: Arc::clone(&self.session),
            // A command that declares no `GatewayNeed` never sees the handle,
            // even when one was offered: the catalogue, not the caller,
            // decides which commands talk to a squad daemon.
            gateway: match spec.gateway_need {
                GatewayNeed::None => None,
                _ => self.squad_gateway.clone(),
            },
            caller: CallerContext::new(&canonical, self.frontend.kind()),
        };
        (spec.build)(&ctx)
    }

    /// What a frontend should display while this invocation runs.
    ///
    /// Both facts used to be recomputed in `App::spawn_command` from the raw
    /// parsed input — the agent name by reading the `"agent"` flag key and
    /// re-running the engine's precedence rules, the unattended indicator as
    /// `yolo || auto`. Both are Layer 2 decisions, and both drifted from what
    /// the command itself would resolve as soon as a default moved.
    ///
    /// Answered from the same [`ResolvedFlags`] and [`Session`] the command
    /// will be built from. It is *not* read off a `BuiltCommand`: a squad
    /// command cannot be built until [`Dispatch::admit`] has resolved its
    /// gateway, which is async, and the overlay title is needed before the
    /// command is spawned.
    ///
    /// An unknown path or an unresolvable flag yields
    /// [`LaunchDisplay::unknown`]; reporting the error is
    /// [`Dispatch::run_command`]'s job, not this call's.
    pub fn launch_display(&self, path: &[&str]) -> LaunchDisplay {
        let canonical: Vec<&str> = self.catalogue.canonical_path(path).into_iter().collect();
        let Some(spec) = self.catalogue.lookup(&canonical) else {
            return LaunchDisplay::unknown();
        };
        let Ok(flags) = ResolvedFlags::resolve(&self.frontend, &canonical, spec) else {
            return LaunchDisplay::unknown();
        };
        let Ok(session) = self.session.try_read() else {
            return LaunchDisplay::unknown();
        };
        let agent_flag = spec.find_flag("agent").and_then(|_| flags.string("agent"));
        let agent_display_name = crate::command::commands::LaunchPolicy::for_session(&session)
            .resolve_agent(&agent_flag)
            .map(|name| name.into_string())
            .unwrap_or_else(|_| agent_flag.unwrap_or_default());
        LaunchDisplay {
            agent_display_name,
            unattended: flags.unattended(),
        }
    }

    /// Tokenize a raw TUI command-box string into typed
    /// [`ParsedCommandBoxInput`]. All command-string interpretation lives
    /// here, never in the TUI.
    pub fn parse_command_box_input(raw: &str) -> Result<ParsedCommandBoxInput, CommandError> {
        parsed_input::parse(raw, CommandCatalogue::get())
    }
}

impl<F: DispatchFrontend> Dispatch<F> {
    /// Run the catalogue's pre-build admissions for `path`.
    ///
    /// The runtime tier comes first: a sandbox-class runtime cannot back
    /// squad at all, so refusing here avoids provisioning a key and then
    /// waiting ten seconds on a daemon child that was always going to refuse
    /// to start.
    ///
    /// A gateway is then resolved for whatever the spec's [`GatewayNeed`]
    /// asks for, unless one was already injected — the squad daemon supplies
    /// its own local gateway, and tests supply a double.
    async fn admit(&mut self, path: &[&str]) -> Result<(), CommandError> {
        let canonical: Vec<&str> = self.catalogue.canonical_path(path).into_iter().collect();
        let Some(spec) = self.catalogue.lookup(&canonical) else {
            // An unknown path is `build_command`'s error to report, with the
            // path the user actually typed.
            return Ok(());
        };
        Self::validate_runtime_admission(&self.engines, &canonical)?;
        if spec.gateway_need == GatewayNeed::None || self.squad_gateway.is_some() {
            return Ok(());
        }
        // The live process environment, not the session's startup snapshot: a
        // key minted earlier in this process is published into the former (see
        // `SquadSupervisor`), and the snapshot predates it.
        let resolver =
            SquadGatewayResolver::from_env(&crate::data::config::env::Env::from_process())?;
        let gateway = resolver.gateway_for(spec.gateway_need).await?;
        // The only moment the plaintext key exists outside the daemon's hash
        // file. A frontend that cannot show it drops it (see the trait's
        // default), which is why this is offered before the command runs
        // rather than folded into its output.
        if let Some(setup) = resolver.take_key_setup() {
            self.frontend.show_key_setup(&setup);
        }
        self.squad_gateway = gateway;
        Ok(())
    }

    /// Build the requested command and drive it to completion.
    ///
    /// Two admissions run before the command is built, both driven by the
    /// catalogue rather than by a frontend's own list of command names
    /// (WI 0113 F-04): the runtime-tier guard, and squad gateway resolution.
    /// Running is then one call through the [`Command`] trait.
    pub async fn run_command(mut self, path: &[&str]) -> Result<CommandOutcome, CommandError> {
        self.admit(path).await?;
        let session = Arc::clone(&self.session);
        let built = self.build_command(path)?;

        // Decision Q3: `SessionState` is the ruling record of what a session
        // is doing, and Layer 2 is what writes it. Every frontend that runs a
        // command runs it through here, so this is the one place the
        // in-flight command is recorded (WI 0114 F-22).
        session
            .write()
            .await
            .state_mut()
            .begin_command(path.join(" "), Vec::new());

        // Armed for the whole run. If the command unwinds — a panic in a
        // command thread — this future's locals drop during that unwind and
        // the guard records the failure the code below never reaches. Without
        // it, a panicked command leaves `SessionState` reading `Running`
        // forever, which is why the TUI grew an outbox of its own (F-22).
        let mut outcome_guard = UnfinishedCommandGuard::arm(Arc::clone(&session));

        let result = built.run_with_frontend(self.frontend).await;

        {
            let mut guard = session.write().await;
            let state = guard.state_mut();
            match &result {
                Ok(outcome) => state.finish_command(outcome.exit_code()),
                Err(err) => state.fail_command(err.to_string()),
            }
            state.set_current_workflow(None);
        }
        outcome_guard.disarm();

        result
    }
}

/// Records a failure on the session when [`Dispatch::run_command`] does not
/// reach its own outcome write — in practice, a panic in the command.
///
/// Decision Q3 says Layer 2 writes `SessionState`, and this is the case Layer
/// 2 could not cover before: the panicking thread never runs the code that
/// records an outcome, so the session stayed `Running` and whichever frontend
/// noticed the dead thread had to write the outcome itself. Now nobody above
/// Layer 2 does.
///
/// `try_write` rather than `write`, because `Drop` cannot await. The only
/// writer inside `run_command` is scoped and has already been released by the
/// time this drops, so the lock is free on the unwind path; a command that
/// somehow left a writer alive keeps the `Running` state rather than
/// deadlocking the unwind.
struct UnfinishedCommandGuard {
    session: Arc<RwLock<Session>>,
    armed: bool,
}

impl UnfinishedCommandGuard {
    fn arm(session: Arc<RwLock<Session>>) -> Self {
        Self {
            session,
            armed: true,
        }
    }

    /// The command recorded its own outcome; this guard has nothing to do.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// The message a session carries for a command that ended without one.
    ///
    /// Byte-identical to the text the TUI used to compose for this case,
    /// including the pointer at the panic log — `PanicLog` is Layer 0, so
    /// naming it here crosses nothing.
    fn unexpected_end_message() -> String {
        let log =
            crate::data::fs::PanicLog::from_env(&crate::data::config::env::Env::from_process());
        match log {
            Some(log) => format!(
                "command task ended unexpectedly (likely a panic — see {})",
                log.path().display()
            ),
            None => "command task ended unexpectedly (likely a panic)".to_string(),
        }
    }
}

impl Drop for UnfinishedCommandGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut guard) = self.session.try_write() {
            let state = guard.state_mut();
            state.fail_command(Self::unexpected_end_message());
            state.set_current_workflow(None);
        }
    }
}

/// Move `$frontend` into the `Box<dyn *CommandFrontend>` each command's
/// [`Command`] impl takes, run it, and wrap the typed outcome in the matching
/// [`CommandOutcome`] variant. One line per command: the mapping is the only
/// thing that differs between arms.
macro_rules! run_built_command {
    ($built:expr, $frontend:expr, { $($variant:ident => $frontend_trait:path),* $(,)? }) => {
        match $built {
            $(
                BuiltCommand::$variant(command) => {
                    let boxed: Box<dyn $frontend_trait> = Box::new($frontend);
                    command
                        .run_with_frontend(boxed)
                        .await
                        .map(CommandOutcome::$variant)
                }
            )*
        }
    };
}

impl BuiltCommand {
    /// Run this command against `frontend`.
    ///
    /// The enum survives WI 0113 F-10 because [`Command`] carries associated
    /// `Frontend` and `Outcome` types and so cannot be made into a trait
    /// object, and because `cli::run` still needs to reach inside for the
    /// `exec workflow` carve-out. What it no longer carries is any per-command
    /// logic — only the variant-to-frontend-trait mapping below.
    pub async fn run_with_frontend<F: DispatchFrontend>(
        self,
        frontend: F,
    ) -> Result<CommandOutcome, CommandError> {
        run_built_command!(self, frontend, {
            Init => InitCommandFrontend,
            Ready => ReadyCommandFrontend,
            Chat => ChatCommandFrontend,
            Specs => SpecsCommandFrontend,
            Status => StatusCommandFrontend,
            Config => ConfigCommandFrontend,
            ExecPrompt => ExecPromptCommandFrontend,
            ExecWorkflow => ExecWorkflowCommandFrontend,
            ApiServer => ApiServerCommandFrontend,
            Squad => SquadCommandFrontend,
            SquadAttach => SquadAttachFrontend,
            Remote => RemoteCommandFrontend,
            New => NewCommandFrontend,
            Clean => CleanCommandFrontend,
        })
    }
}

pub(crate) fn parse_squad_interval(command: &[&str], raw: &str) -> Result<u64, CommandError> {
    let value = raw.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix('s') {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3600)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(|| CommandError::InvalidFlagValue {
            command: command.iter().map(|part| (*part).to_string()).collect(),
            flag: "interval".into(),
            reason: "expected seconds or a duration such as 5m".into(),
        })
}

/// Parse `--agent-models` specs at the dispatch boundary, so Layer 2 only ever
/// sees the assembled map (WI 0110). The parse itself lives with the gateway
/// types beside the formatter that reverses it.
pub(crate) fn parse_squad_agent_models(
    command: &[&str],
    specs: &[String],
) -> Result<std::collections::BTreeMap<String, Vec<String>>, CommandError> {
    crate::command::commands::squad::gateway::parse_agent_models_specs(specs).map_err(|reason| {
        CommandError::InvalidFlagValue {
            command: command.iter().map(|part| (*part).to_string()).collect(),
            flag: "agent-models".into(),
            reason,
        }
    })
}

/// Convert the catalogue-validated launch-mode enum into the Layer 0 type.
/// Keeping this conversion at the dispatch boundary means command wiring only
/// ever sees a typed `LaunchMode`.
pub(crate) fn parse_launch_mode(
    raw: Option<String>,
    command: &[&str],
) -> Result<Option<crate::data::config::repo::LaunchMode>, CommandError> {
    match raw.as_deref() {
        None => Ok(None),
        Some("stdio") => Ok(Some(crate::data::config::repo::LaunchMode::Stdio)),
        Some("acp") => Ok(Some(crate::data::config::repo::LaunchMode::Acp)),
        Some(value) => Err(CommandError::InvalidFlagValue {
            command: command.iter().map(|part| (*part).to_string()).collect(),
            flag: "launch-mode".into(),
            reason: format!("unknown enum value {value:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording frontend used by Dispatch unit tests.
    pub(super) struct FakeCommandFrontend {
        pub bools: std::collections::HashMap<String, bool>,
        pub strings: std::collections::HashMap<String, String>,
        pub strings_vec: std::collections::HashMap<String, Vec<String>>,
        pub paths: std::collections::HashMap<String, PathBuf>,
        pub enums: std::collections::HashMap<String, String>,
        pub u16s: std::collections::HashMap<String, u16>,
        pub usizes: std::collections::HashMap<String, usize>,
        pub args: std::collections::HashMap<String, String>,
        pub args_vec: std::collections::HashMap<String, Vec<String>>,
        /// What this fake reports for
        /// [`CommandFrontend::input_available`]. `true` unless a test sets it.
        pub input_available: bool,
    }

    impl FakeCommandFrontend {
        pub fn new() -> Self {
            Self {
                bools: Default::default(),
                strings: Default::default(),
                strings_vec: Default::default(),
                paths: Default::default(),
                enums: Default::default(),
                u16s: Default::default(),
                usizes: Default::default(),
                args: Default::default(),
                args_vec: Default::default(),
                input_available: true,
            }
        }

        /// A caller with nowhere to read an answer from — a pipe, a CI job.
        pub fn without_input(mut self) -> Self {
            self.input_available = false;
            self
        }
    }

    impl crate::data::message::UserMessageSink for FakeCommandFrontend {
        fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl CommandFrontend for FakeCommandFrontend {
        fn kind(&self) -> FrontendKind {
            FrontendKind::Cli
        }
        fn input_available(&self) -> bool {
            self.input_available
        }
        fn flag_bool(&self, _p: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
            Ok(self.bools.get(flag).copied())
        }
        fn flag_string(&self, _p: &[&str], flag: &str) -> Result<Option<String>, CommandError> {
            Ok(self.strings.get(flag).cloned())
        }
        fn flag_strings(&self, _p: &[&str], flag: &str) -> Result<Vec<String>, CommandError> {
            Ok(self.strings_vec.get(flag).cloned().unwrap_or_default())
        }
        fn flag_path(&self, _p: &[&str], flag: &str) -> Result<Option<PathBuf>, CommandError> {
            Ok(self.paths.get(flag).cloned())
        }
        fn flag_enum(&self, _p: &[&str], flag: &str) -> Result<Option<String>, CommandError> {
            Ok(self.enums.get(flag).cloned())
        }
        fn flag_u16(&self, _p: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
            Ok(self.u16s.get(flag).copied())
        }
        fn flag_usize(&self, _p: &[&str], flag: &str) -> Result<Option<usize>, CommandError> {
            Ok(self.usizes.get(flag).copied())
        }
        fn argument(&self, _p: &[&str], name: &str) -> Result<Option<String>, CommandError> {
            Ok(self.args.get(name).cloned())
        }
        fn arguments(&self, _p: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
            Ok(self.args_vec.get(name).cloned().unwrap_or_default())
        }
    }

    /// `Dispatch` takes a shared session, so this wraps the shared fixture
    /// rather than re-spelling `Session::open`.
    fn make_session() -> Arc<RwLock<Session>> {
        let tmp = tempfile::tempdir().unwrap();
        Arc::new(RwLock::new(Session::for_tests(tmp.path())))
    }

    /// A malformed global config fails the daemon's start with a message
    /// naming the file, instead of being `unwrap_or_default()`ed away (F-31).
    #[test]
    fn a_malformed_global_config_fails_the_daemon_bootstrap() {
        use crate::data::fs::DataPaths;

        let _lock = crate::data::config::env::DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::data::config::env::ConfigHomeGuard::set(tmp.path());
        // `AWMAN_CONFIG_HOME` *is* the config home, so the file sits directly
        // under it.
        std::fs::write(tmp.path().join("config.json"), "{ \"runtime\": ").unwrap();

        let paths = DataPaths::at_root(tmp.path());
        let result = Engines::for_daemon(DaemonKind::Squad, &paths);

        let error = result
            .err()
            .expect("a malformed config must fail the start");
        let text = error.to_string();
        assert!(
            text.contains("config.json"),
            "the error must name the file: {text}"
        );
    }

    /// Who the caller is comes from the frontend's declared kind, once, at
    /// construction — not from a per-frontend `is_local_user_session`
    /// override (F-49).
    #[test]
    fn the_caller_context_records_the_frontend_and_whether_a_user_is_there() {
        use crate::command::dispatch::catalogue::FrontendKind;
        use crate::command::dispatch::resolved::CallerContext;

        for (kind, local) in [
            (FrontendKind::Cli, true),
            (FrontendKind::Tui, true),
            (FrontendKind::Api, false),
        ] {
            let caller = CallerContext::new(&["squad", "add"], kind);
            assert_eq!(caller.frontend(), kind);
            assert_eq!(caller.local_user(), local, "local_user for {kind:?}");
            assert_eq!(caller.leaf(), "add");
        }
    }

    /// A command that unwinds still leaves the session with a recorded
    /// outcome, so no frontend has to write one (decision Q3, F-22).
    ///
    /// Driven through the guard directly rather than by panicking a real
    /// command: what is under test is that dropping an armed guard records
    /// the failure, which is exactly what the unwind does to it.
    #[test]
    fn an_unfinished_command_is_recorded_as_failed_on_the_session() {
        use crate::data::session::CommandStatus;

        let session = make_session();
        {
            let mut guard = session.try_write().unwrap();
            guard
                .state_mut()
                .begin_command("exec workflow".to_string(), Vec::new());
        }

        drop(UnfinishedCommandGuard::arm(Arc::clone(&session)));

        let guard = session.try_read().unwrap();
        let command = guard
            .state()
            .current_command
            .as_ref()
            .expect("the command is still recorded");
        match &command.status {
            CommandStatus::Error(message) => assert!(
                message.contains("ended unexpectedly"),
                "unexpected message: {message}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// A command that records its own outcome disarms the guard, so the guard
    /// never overwrites it.
    #[test]
    fn a_disarmed_guard_leaves_the_recorded_outcome_alone() {
        use crate::data::session::CommandStatus;

        let session = make_session();
        {
            let mut guard = session.try_write().unwrap();
            let state = guard.state_mut();
            state.begin_command("status".to_string(), Vec::new());
            state.finish_command(0);
        }

        let mut armed = UnfinishedCommandGuard::arm(Arc::clone(&session));
        armed.disarm();
        drop(armed);

        let guard = session.try_read().unwrap();
        let command = guard.state().current_command.as_ref().unwrap();
        assert!(matches!(command.status, CommandStatus::Done));
        assert_eq!(command.exit_code, Some(0));
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
        let session = Session::for_tests(tmp.path());

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

    #[test]
    fn build_status_command_with_no_flags() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["status"]).unwrap();
        match built {
            BuiltCommand::Status(_) => {}
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn build_bare_squad_uses_the_status_subcommand() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["squad"]).unwrap();
        match built {
            BuiltCommand::Squad(command) => assert!(matches!(
                command.subcommand(),
                crate::command::commands::squad::commands::SquadSubcommand::Status(_)
            )),
            _ => panic!("expected Squad"),
        }
    }

    #[test]
    fn build_unknown_command_returns_unknown_command_error() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["bogus"]);
        match result {
            Err(CommandError::UnknownCommand { .. }) => {}
            Err(other) => panic!("expected UnknownCommand, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_specs_amend_missing_argument_errors() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["specs", "amend"]);
        match result {
            Err(CommandError::MissingRequiredArgument { .. }) => {}
            Err(other) => panic!("expected MissingRequiredArgument, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_chat_with_yolo_and_plan_returns_mutually_exclusive() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend.bools.insert("plan".into(), true);
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["chat"]);
        match result {
            Err(CommandError::MutuallyExclusive { .. }) => {}
            Err(other) => panic!("expected MutuallyExclusive, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn ready_json_implies_non_interactive_in_built_command() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("json".into(), true);
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["ready"]).unwrap();
        match built {
            BuiltCommand::Ready(cmd) => {
                assert!(
                    cmd.flags().non_interactive,
                    "json should imply non_interactive"
                );
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn exec_workflow_yolo_implies_worktree_in_built_command() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(
                    cmd.flags().worktree,
                    "yolo should imply worktree on exec workflow"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn exec_workflow_auto_implies_worktree_in_built_command() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("auto".into(), true);
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(
                    cmd.flags().worktree,
                    "auto should imply worktree on exec workflow"
                );
                assert!(cmd.flags().auto);
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn build_config_show_succeeds_with_no_args() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["config", "show"]).unwrap();
        assert!(matches!(built, BuiltCommand::Config(_)));
    }

    #[test]
    fn build_config_get_with_field_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .args
            .insert("field".into(), "terminal_scrollback_lines".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["config", "get"]).unwrap();
        assert!(matches!(built, BuiltCommand::Config(_)));
    }

    #[test]
    fn build_config_get_missing_field_returns_missing_required_argument() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["config", "get"]);
        assert!(
            matches!(result, Err(CommandError::MissingRequiredArgument { .. })),
            "missing field must return MissingRequiredArgument"
        );
    }

    #[test]
    fn build_new_workflow_with_format_flag() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.enums.insert("format".into(), "yaml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["new", "workflow"]).unwrap();
        assert!(matches!(built, BuiltCommand::New(_)));
    }

    #[test]
    fn build_api_start_with_port() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.u16s.insert("port".into(), 1234);
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["api", "start"]).unwrap();
        assert!(matches!(built, BuiltCommand::ApiServer(_)));
    }

    #[test]
    fn build_chat_default_flags_all_false() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["chat"]).unwrap();
        match built {
            BuiltCommand::Chat(cmd) => {
                let f = cmd.flags();
                assert!(!f.yolo && !f.plan && !f.non_interactive && !f.allow_docker);
            }
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn build_remote_exec_workflow_with_workflow_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch
            .build_command(&["remote", "exec", "workflow"])
            .unwrap();
        assert!(matches!(built, BuiltCommand::Remote(_)));
    }

    #[test]
    fn build_remote_exec_prompt_with_prompt_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.args.insert("prompt".into(), "hello".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch
            .build_command(&["remote", "exec", "prompt"])
            .unwrap();
        assert!(matches!(built, BuiltCommand::Remote(_)));
    }

    #[test]
    fn build_exec_prompt_with_prompt_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.args.insert("prompt".into(), "do something".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "prompt"]).unwrap();
        assert!(matches!(built, BuiltCommand::ExecPrompt(_)));
    }

    #[test]
    fn build_exec_prompt_with_empty_prompt_builds_ok_when_issue_may_provide_input() {
        // A whitespace-only prompt is normalised to None at dispatch time;
        // final validation (prompt-or-issue required) happens at run time.
        let mut frontend = FakeCommandFrontend::new();
        frontend.args.insert("prompt".into(), "   ".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "prompt"]);
        assert!(
            result.is_ok(),
            "whitespace prompt should build OK (validation deferred to runtime)"
        );
    }

    #[test]
    fn build_exec_workflow_missing_workflow_argument_returns_missing_required_argument() {
        // workflow is required and neither flag nor positional arg is set
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        assert!(
            matches!(result, Err(CommandError::MissingRequiredArgument { .. })),
            "missing workflow must return MissingRequiredArgument"
        );
    }

    #[test]
    fn alias_wf_resolves_to_exec_workflow() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        // "wf" is a string alias under "exec"; dispatch should resolve it.
        let built = dispatch.build_command(&["exec", "wf"]).unwrap();
        assert!(
            matches!(built, BuiltCommand::ExecWorkflow(_)),
            "exec wf must dispatch to ExecWorkflow"
        );
    }

    // ─── parse_command_box_input ──────────────────────────────────────────────

    #[test]
    fn parse_command_box_input_exec_workflow_with_yolo() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input(
            "exec workflow my-workflow.toml --yolo",
        )
        .unwrap();
        assert_eq!(parsed.path, vec!["exec", "workflow"]);
        assert!(matches!(
            parsed.flags.get("yolo"),
            Some(parsed_input::FlagValue::Bool(true))
        ));
        match parsed.arguments.get("workflow") {
            Some(parsed_input::ArgValue::Single(s)) => {
                assert_eq!(s, "my-workflow.toml");
            }
            other => panic!("expected Single workflow argument, got: {other:?}"),
        }
    }

    #[test]
    fn parse_command_box_input_rejects_unknown_top_level_command() {
        let result = Dispatch::<FakeCommandFrontend>::parse_command_box_input("not-a-command");
        assert!(
            matches!(result, Err(CommandError::UnknownCommand { .. })),
            "unknown command must return UnknownCommand, got: {result:?}"
        );
    }

    #[test]
    fn parse_command_box_input_rejects_unknown_flag() {
        let result = Dispatch::<FakeCommandFrontend>::parse_command_box_input("status --bogus");
        assert!(
            matches!(result, Err(CommandError::UnknownFlag { .. })),
            "unknown flag must return UnknownFlag, got: {result:?}"
        );
    }

    #[test]
    fn parse_command_box_input_remote_exec_workflow() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input(
            "remote exec workflow my-workflow.toml --follow",
        )
        .unwrap();
        assert_eq!(parsed.path, vec!["remote", "exec", "workflow"]);
        match parsed.arguments.get("workflow") {
            Some(parsed_input::ArgValue::Single(s)) => {
                assert_eq!(s, "my-workflow.toml");
            }
            other => panic!("expected Single workflow argument, got: {other:?}"),
        }
        assert!(matches!(
            parsed.flags.get("follow"),
            Some(parsed_input::FlagValue::Bool(true))
        ));
    }

    #[test]
    fn parse_command_box_input_short_flag_non_interactive() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input("ready -n").unwrap();
        assert_eq!(parsed.path, vec!["ready"]);
        assert!(matches!(
            parsed.flags.get("non-interactive"),
            Some(parsed_input::FlagValue::Bool(true))
        ));
    }

    #[test]
    fn exec_workflow_no_yolo_no_auto_worktree_false() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        // Neither yolo nor auto is set; worktree must not be implied.
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(
                    !cmd.flags().worktree,
                    "worktree must be false when neither yolo nor auto is set"
                );
                assert!(!cmd.flags().yolo);
                assert!(!cmd.flags().auto);
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn exec_workflow_yolo_plus_explicit_worktree_true_stays_true() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend.bools.insert("worktree".into(), true);
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(cmd.flags().yolo);
                assert!(
                    cmd.flags().worktree,
                    "worktree must be true when both yolo and --worktree are set"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    // ── Issue flag dispatch tests ─────────────────────────────────────────────

    #[test]
    fn build_exec_workflow_issue_flag_populates_issue_source() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .strings
            .insert("issue".into(), "owner/repo#84".into());
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert_eq!(
                    cmd.flags().issue_source.issue.as_deref(),
                    Some("owner/repo#84"),
                    "issue_source.issue must be populated from --issue flag"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn build_exec_workflow_issue_and_work_item_are_mutually_exclusive() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .strings
            .insert("issue".into(), "owner/repo#84".into());
        frontend
            .strings
            .insert("work-item".into(), "0084-my-item.md".into());
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        match result {
            Err(CommandError::MutuallyExclusive { .. }) => {}
            Err(other) => panic!("expected MutuallyExclusive, got {other:?}"),
            Ok(_) => panic!("expected error for mutually exclusive flags"),
        }
    }

    #[test]
    fn build_exec_prompt_issue_flag_populates_issue_source() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.strings.insert("issue".into(), "42".into());
        // No positional prompt — that's ok at dispatch time (validated at runtime).
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["exec", "prompt"]).unwrap();
        match built {
            BuiltCommand::ExecPrompt(cmd) => {
                assert_eq!(
                    cmd.flags().issue_source.issue.as_deref(),
                    Some("42"),
                    "issue_source.issue must be populated from --issue flag"
                );
            }
            _ => panic!("expected ExecPrompt"),
        }
    }

    #[test]
    fn build_exec_prompt_issue_and_prompt_are_both_optional() {
        // When only --issue is set (no positional prompt), build_command must succeed.
        // The runtime check (at least one of prompt/issue) happens in run_with_frontend.
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .strings
            .insert("issue".into(), "owner/repo#1".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "prompt"]);
        assert!(
            result.is_ok(),
            "build_command must succeed when only --issue is set: {}",
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        );
        match result.unwrap() {
            BuiltCommand::ExecPrompt(cmd) => {
                assert!(
                    cmd.flags().prompt.is_none(),
                    "prompt must be None when no positional argument is provided"
                );
                assert_eq!(
                    cmd.flags().issue_source.issue.as_deref(),
                    Some("owner/repo#1")
                );
            }
            _ => panic!("expected ExecPrompt"),
        }
    }

    // ─── WI-0092: Dynamic Workflows — dispatch layer tests ───────────────────

    #[test]
    fn exec_workflow_dynamic_without_path_builds_successfully() {
        // --dynamic omits the positional workflow path; dispatch must not error.
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("dynamic".into(), true);
        frontend.strings.insert("work-item".into(), "0042".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        assert!(
            result.is_ok(),
            "--dynamic without workflow path must succeed at dispatch: {}",
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        );
        match result.unwrap() {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(cmd.flags().dynamic, "dynamic flag must be true");
                assert!(
                    cmd.flags().workflow.is_none(),
                    "workflow path must be None for --dynamic"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn exec_workflow_dynamic_without_work_item_returns_error() {
        // --dynamic requires --work-item; missing it must error at dispatch.
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("dynamic".into(), true);
        // No work-item set.
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("--dynamic requires --work-item"),
                    "error must name the missing flag, got: {msg}"
                );
            }
            Ok(_) => panic!("--dynamic without --work-item must return an error"),
        }
    }

    #[test]
    fn exec_workflow_leader_without_dynamic_returns_error() {
        // --leader is only valid with --dynamic.
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .strings
            .insert("leader".into(), "claude::claude-opus-4-8".into());
        frontend
            .args
            .insert("workflow".into(), "/tmp/wf.toml".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("--leader is only valid with --dynamic"),
                    "error must state the constraint, got: {msg}"
                );
            }
            Ok(_) => panic!("--leader without --dynamic must return an error"),
        }
    }

    #[test]
    fn exec_workflow_dynamic_parses_leader_flag() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("dynamic".into(), true);
        frontend.strings.insert("work-item".into(), "0042".into());
        frontend
            .strings
            .insert("leader".into(), "claude::claude-opus-4-8".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        assert!(
            result.is_ok(),
            "build must succeed: {}",
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        );
        match result.unwrap() {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert_eq!(
                    cmd.flags().leader.as_deref(),
                    Some("claude::claude-opus-4-8"),
                    "leader flag must be preserved in ExecWorkflowCommandFlags"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn exec_workflow_dynamic_with_plan_returns_error() {
        // --dynamic enforces yolo so --plan is incompatible.
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("dynamic".into(), true);
        frontend.bools.insert("plan".into(), true);
        frontend.strings.insert("work-item".into(), "0042".into());
        let dispatch = Dispatch::new(
            frontend,
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("--dynamic cannot be used with --plan"),
                    "error must explain the conflict, got: {msg}"
                );
            }
            Ok(_) => panic!("--dynamic --plan must return an error"),
        }
    }

    #[test]
    fn exec_workflow_static_without_path_returns_missing_required_argument() {
        // Non-dynamic invocation still requires the positional workflow path.
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let result = dispatch.build_command(&["exec", "workflow"]);
        assert!(
            matches!(result, Err(CommandError::MissingRequiredArgument { .. })),
            "static exec workflow without path must return MissingRequiredArgument"
        );
    }

    // ── WI-0098 Finding B: Engines::detect runtime-detection policy ───────────
    //
    // The three documented paths lifted out of `main.rs`: valid runtime, an
    // unknown `runtime:` string (fatal for CLI, modal for TUI), and a runtime
    // unavailable on this host (fatal only when the command requires a runtime).

    fn config_with_runtime(runtime: Option<&str>) -> GlobalConfig {
        GlobalConfig {
            runtime: runtime.map(String::from),
            ..Default::default()
        }
    }

    fn session_at(root: &std::path::Path) -> Session {
        Session::open_at_git_root(
            root.to_path_buf(),
            root.to_path_buf(),
            crate::data::session::SessionOpenOptions::default(),
        )
        .expect("open test session")
    }

    fn with_daemon_global_config<T>(config: GlobalConfig, test: impl FnOnce() -> T) -> T {
        let home = tempfile::tempdir().expect("create global config home");
        // `ConfigHomeGuard`, not `CWD_LOCK`: the variable this serialises is
        // `AWMAN_CONFIG_HOME`, and the `clean` and `overlay` tests that also
        // repoint it never touched `CWD_LOCK`. Holding the wrong lock let
        // them move the config home mid-test, so `Engines::for_daemon` read a
        // config it was never given and built the wrong runtime tier.
        let _guard = crate::data::config::env::ConfigHomeGuard::set(home.path());
        config.save().expect("save global config");
        test()
    }

    #[test]
    fn engines_build_and_for_daemon_produce_container_tier_under_default_config() {
        let root = tempfile::tempdir().expect("create test root");
        let session = session_at(root.path());
        let built = Engines::build(&GlobalConfig::default(), &session)
            .expect("default session engines build");
        assert!(built.container_runtime.is_some());
        assert!(built.sandbox_runtime.is_none());

        with_daemon_global_config(GlobalConfig::default(), || {
            let daemon = Engines::for_daemon(DaemonKind::Api, &DataPaths::at_root(root.path()))
                .expect("default daemon engines build");
            assert!(daemon.container_runtime.is_some());
            assert!(daemon.sandbox_runtime.is_none());
        });
    }

    /// `docker-sbx-experimental` is platform-gated in `SandboxRuntime::dsbx`
    /// (linux and x86_64 macos both refuse with `BackendUnsupportedOnPlatform`
    /// — see `engine::sandbox::runtime`'s own `dsbx_errors_on_*` tests, which
    /// use the same branch-on-`cfg!` pattern). `Engines::build`/`for_daemon`
    /// only assemble a sandbox-tier bundle on the platforms where the backend
    /// actually constructs.
    #[test]
    fn engines_build_and_for_daemon_produce_sandbox_tier_for_experimental_runtime() {
        let root = tempfile::tempdir().expect("create test root");
        let session = session_at(root.path());
        let config = config_with_runtime(Some("docker-sbx-experimental"));

        if cfg!(target_os = "linux") || cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            match Engines::build(&config, &session) {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, .. }) => {
                    assert_eq!(backend, "docker-sbx-experimental")
                }
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got {e:?}"),
                Ok(_) => panic!("dsbx build must fail on this platform"),
            }

            with_daemon_global_config(config, || {
                match Engines::for_daemon(DaemonKind::Squad, &DataPaths::at_root(root.path())) {
                    Err(EngineError::BackendUnsupportedOnPlatform { backend, .. }) => {
                        assert_eq!(backend, "docker-sbx-experimental")
                    }
                    Err(e) => panic!("expected BackendUnsupportedOnPlatform, got {e:?}"),
                    Ok(_) => panic!("dsbx for_daemon must fail on this platform"),
                }
            });
            return;
        }

        let built = Engines::build(&config, &session).expect("sandbox session engines build");
        assert!(built.container_runtime.is_none());
        assert!(built.sandbox_runtime.is_some());

        with_daemon_global_config(config, || {
            let daemon = Engines::for_daemon(DaemonKind::Squad, &DataPaths::at_root(root.path()))
                .expect("sandbox daemon engines build");
            assert!(daemon.container_runtime.is_none());
            assert!(daemon.sandbox_runtime.is_some());
        });
    }

    #[test]
    fn detect_valid_runtime_returns_runtime_and_no_modal_message() {
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("docker"));
        let (detected, modal) = Engines::detect(cat, &cfg, &["status"])
            .expect("a valid runtime must detect successfully");
        assert_eq!(detected.engine().runtime_name(), "docker");
        assert!(
            modal.is_none(),
            "no fatal-modal message on the valid-runtime path"
        );
    }

    #[test]
    fn detect_unknown_runtime_cli_returns_unknown_runtime_error() {
        // A CLI invocation (non-empty command path) with a misspelled runtime is
        // a fatal configuration error the caller prints and exits on.
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("totally-bogus-runtime"));
        // `DetectedRuntime` is not `Debug`, so match rather than `expect_err`.
        match Engines::detect(cat, &cfg, &["status"]) {
            Err(EngineError::UnknownRuntime { .. }) => {}
            Err(other) => panic!("expected UnknownRuntime, got {other:?}"),
            Ok(_) => panic!("an unknown runtime must be an error for CLI invocations"),
        }
    }

    #[test]
    fn detect_unknown_runtime_tui_builds_default_engines_and_returns_modal_message() {
        // The bare-TUI invocation (empty command path) must still construct inert
        // default (Docker) engines so the fatal modal can render, and return the
        // error text for that modal.
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("totally-bogus-runtime"));
        let (detected, modal) =
            Engines::detect(cat, &cfg, &[]).expect("the TUI path must still yield default engines");
        assert_eq!(
            detected.engine().runtime_name(),
            "docker",
            "the TUI fallback must be the default Docker runtime"
        );
        let msg = modal.expect("the TUI path must return a fatal-modal message");
        assert!(
            msg.contains("totally-bogus-runtime"),
            "modal message must name the bad runtime; got: {msg}"
        );
    }

    // The unavailable-on-host path needs a runtime that this host cannot
    // construct. `apple-containers` is unavailable on every non-macOS host, so
    // these two tests exercise the fatal-vs-warn branch there.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn detect_unavailable_runtime_is_fatal_when_command_requires_runtime() {
        let cat = CommandCatalogue::get();
        let cfg = config_with_runtime(Some("apple-containers"));
        // `status` requires a runtime → the unavailable runtime is fatal.
        assert!(cat.requires_runtime(&["status"]));
        match Engines::detect(cat, &cfg, &["status"]) {
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
        let cfg = config_with_runtime(Some("apple-containers"));
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

    #[test]
    fn command_outcome_exit_code_covers_execution_and_partial_skill_failure() {
        assert_eq!(
            CommandOutcome::ExecWorkflow(
                crate::command::commands::exec_workflow::ExecWorkflowOutcome {
                    workflow: "workflow.toml".into(),
                    exit_code: Some(17),
                    worktree_used: false,
                }
            )
            .exit_code(),
            17
        );
        assert_eq!(
            CommandOutcome::ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome {
                agent: None,
                exit_code: Some(3)
            })
            .exit_code(),
            3
        );
        let partial = CommandOutcome::New(crate::command::commands::new::NewOutcome::Skill(
            crate::command::commands::new::NewSkillOutcome {
                interview: false,
                global: false,
                path: None,
                pull: true,
                libraries: vec![crate::command::commands::new::PullLibraryOutcome {
                    slug: "broken".into(),
                    dir: "broken".into(),
                    updated: false,
                    skills_found: Vec::new(),
                    error: Some("unreachable".into()),
                }],
            },
        ));
        assert!(partial.is_partial_failure());
        assert_eq!(partial.exit_code(), 1);
        assert_eq!(CommandOutcome::Empty.exit_code(), 0);
    }
}
