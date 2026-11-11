//! `Dispatch` — Layer 2's gateway from frontends into typed `*Command` values.
//!
//! Frontends construct a `Dispatch` with a frontend-specific
//! [`CommandFrontend`] implementation (CLI, TUI, API). Dispatch reads
//! flag values from the frontend, applies catalogue-driven validation
//! (mutually-exclusive flags, type errors, implications), and returns a typed
//! [`BuiltCommand`] enum containing the constructed `*Command` struct.

use crate::command::commands::link::{LinkCommand, LinkFrontend};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::commands::agents::{AgentsCommand, AgentsCommandFrontend};
use crate::command::commands::attach::{AttachCommand, AttachCommandFrontend};
use crate::command::commands::chat::{ChatCommand, ChatCommandFrontend};
use crate::command::commands::clean::{CleanCommand, CleanCommandFrontend};
use crate::command::commands::config::{ConfigCommand, ConfigCommandFrontend};
use crate::command::commands::daemon::api::{DaemonApiCommand, DaemonApiCommandFrontend};
use crate::command::commands::daemon::lifecycle::{DaemonCommand, DaemonCommandFrontend};
use crate::command::commands::exec_prompt::{ExecPromptCommand, ExecPromptCommandFrontend};
use crate::command::commands::exec_workflow::{ExecWorkflowCommand, ExecWorkflowCommandFrontend};
use crate::command::commands::init::{InitCommand, InitCommandFrontend};
use crate::command::commands::new::{NewCommand, NewCommandFrontend};
use crate::command::commands::ready::{ReadyCommand, ReadyCommandFrontend};
use crate::command::commands::remote::{RemoteCommand, RemoteCommandFrontend};
use crate::command::commands::specs::{SpecsCommand, SpecsCommandFrontend};
use crate::command::commands::squad::attach::{SquadAttachCommand, SquadAttachFrontend};
use crate::command::commands::squad::commands::{SquadCommand, SquadCommandFrontend};
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::commands::squad::supervisor::DaemonResolver;
use crate::command::commands::worker::WorkerLinkCommand;
use crate::command::commands::Command;
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind, GatewayNeed};
use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;
use crate::data::session::Session;

pub mod build;
pub mod catalogue;
pub mod engines;
pub mod frontend_action;
pub mod outcomes;
pub mod parsed_input;
pub mod projections;
pub mod resolved;
pub mod runtime_context;

pub use engines::{credential_refresh_monitor, Engines};
pub use frontend_action::FrontendAction;
pub use outcomes::CommandOutcome;
pub use parsed_input::ParsedCommandBoxInput;
pub use resolved::{BuildContext, CallerContext, ResolvedArgs, ResolvedFlags};
pub use runtime_context::RuntimeContext;

/// Presentation facts supplied by a CLI. Backend selection and the wire
/// conversation remain in Dispatch; the adapter only renders their results.
#[derive(Debug, Clone, Copy)]
pub struct CliRelayFacts {
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub term_size: crate::data::frontend_protocol::TermSize,
}

/// CLI completion without losing a remote command's exit code or outcome.
pub enum CliCompletion {
    Local(Box<Result<CommandOutcome, CommandError>>),
    Relayed(crate::data::frontend_protocol::CommandResultWire),
}

impl CliCompletion {
    fn local(result: Result<CommandOutcome, CommandError>) -> Self {
        Self::Local(Box::new(result))
    }
}

/// The rendering half of a CLI relay. No daemon discovery, command parsing,
/// admission or fallback policy is implemented by this trait's adapters.
pub trait CliRelayFrontend: UserMessageSink + Send + Sync + 'static {
    fn relay_bytes(&mut self, stream: crate::data::frontend_protocol::OutputStream, bytes: &[u8]);
    fn relay_facts(&self) -> CliRelayFacts;
    fn relay_notice(&mut self, text: &str);
    fn relay_output(
        &mut self,
        stream: crate::data::frontend_protocol::OutputStream,
        message: crate::data::message::UserMessage,
    );
    fn relay_dialog(
        &mut self,
        request: crate::data::dialog::DialogRequest,
        interrupted: &tokio_util::sync::CancellationToken,
    ) -> crate::data::dialog::DialogResponse;
}

// ─── CommandFrontend trait ──────────────────────────────────────────────────

/// Frontend trait that supplies flag values to Dispatch. Extended by per-
/// command frontend traits (e.g. [`crate::command::commands::exec_workflow::ExecWorkflowCommandFrontend`])
/// for command-specific Q&A and reporting.
pub trait CommandFrontend: UserMessageSink + Send + Sync {
    fn pick_queue_item(
        &mut self,
        _items: &[crate::command::daemon_api::worker_pull::QueueRow],
    ) -> Result<Option<String>, CommandError> {
        Err(CommandError::Other(
            "this frontend has no queue picker".into(),
        ))
    }
    fn confirm_queue_checkout(
        &mut self,
        _prompt: &crate::data::prompt::Prompt<crate::command::worker::queue_source::CheckoutChoice>,
    ) -> crate::command::worker::queue_source::CheckoutChoice {
        crate::command::worker::queue_source::CheckoutChoice::FreshClone
    }
    fn bind_worker_run(
        &mut self,
        _input: ParsedCommandBoxInput,
        _events: crate::command::commands::api_server::event_bus::EventBusSender,
        _writer: crate::data::fs::api_command_log::CommandStateWriter,
    ) -> Result<(), CommandError> {
        Err(CommandError::Other(
            "this frontend cannot host a worker run".into(),
        ))
    }
    /// Which frontend this is.
    ///
    /// The one fact about itself a frontend declares. Everything that used to
    /// be derived from it per-frontend — whether a human is there, whether the
    /// working directory is the user's — is decided once in Layer 2 from this
    /// (WI 0114 F-49). No default: a new frontend must say what it is.
    fn kind(&self) -> FrontendKind;

    /// Policy resolved by Layer 2 for any frontend writer that presents agent
    /// bytes on a host terminal.
    fn set_host_output_policy(&mut self, _policy: crate::data::terminal::HostOutputPolicy) {}

    /// Whether there is somewhere to read an answer from right now.
    ///
    /// The frontend reports the *fact*; Layer 2 applies the rule
    /// ([`ResolvedFlags::is_headless`]). For the CLI the fact is
    /// whether stdin is a terminal, which only the CLI can see; for every
    /// other frontend it follows from the kind, which is why the default
    /// answers from [`FrontendKind::can_ask_a_human`].
    ///
    /// Before WI 0114 F-50 the rule itself lived in Layer 3, in
    /// `frontend::effective_non_interactive`, and each frontend re-applied it.
    fn confirm_browser_link(&mut self) -> Result<bool, CommandError> {
        Ok(false)
    }

    fn read_link_token(&mut self) -> Result<String, CommandError> {
        Err(CommandError::Other("token input unavailable".into()))
    }

    fn input_available(&self) -> bool {
        self.kind().can_ask_a_human()
    }

    /// Read one secret from stdin for a command that explicitly requested
    /// `--token -`. Only the local CLI implements this input channel.
    fn read_stdin_secret(&mut self) -> Result<String, CommandError> {
        Err(CommandError::Other("--token - requires CLI stdin".into()))
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
    + AttachCommandFrontend
    + AgentsCommandFrontend
    + ConfigCommandFrontend
    + ExecPromptCommandFrontend
    + ExecWorkflowCommandFrontend
    + DaemonCommandFrontend
    + DaemonApiCommandFrontend
    + LinkFrontend
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
        + AttachCommandFrontend
        + AgentsCommandFrontend
        + ConfigCommandFrontend
        + ExecPromptCommandFrontend
        + ExecWorkflowCommandFrontend
        + DaemonCommandFrontend
        + DaemonApiCommandFrontend
        + LinkFrontend
        + SquadCommandFrontend
        + SquadAttachFrontend
        + RemoteCommandFrontend
        + NewCommandFrontend
        + SpecsCommandFrontend
        + CleanCommandFrontend
        + 'static
{
}

/// One per `*Command` struct in `src/command/commands/`. Constructed by
/// [`Dispatch::build_command`] and consumed by [`Dispatch::run_command`].
///
/// There are deliberately no `Auth` or `Download` arms: neither command is in
/// the catalogue, so neither is reachable, and WI 0114 F-35 deletes both.
pub enum BuiltCommand {
    Attach(AttachCommand),
    Init(InitCommand),
    Ready(ReadyCommand),
    Chat(ChatCommand),
    Specs(SpecsCommand),
    Agents(AgentsCommand),
    Config(ConfigCommand),
    WorkerLink(WorkerLinkCommand),
    Queue(crate::command::worker::pick::QueueCommand),
    QueuePick(crate::command::worker::pick::QueuePickCommand),
    ExecPrompt(ExecPromptCommand),
    ExecWorkflow(ExecWorkflowCommand),
    Daemon(DaemonCommand),
    DaemonApi(DaemonApiCommand),
    Link(LinkCommand),
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
    /// Handed to the built command through `BuildContext::cancel`.
    cancel: tokio_util::sync::CancellationToken,
    /// Handed to the built command through `BuildContext::output_dir`.
    output_dir: Option<std::path::PathBuf>,
    picked_lease: Option<(
        crate::command::worker::queue_source::SharedQueueSource,
        crate::data::worker::queue::PickRun,
    )>,
}

impl<F: CommandFrontend> Dispatch<F> {
    pub fn new(mut frontend: F, session: Arc<RwLock<Session>>, engines: Engines) -> Self {
        // The credential monitor is attached to `engines.agent_engine` when
        // the bundle is assembled (F-38); a disabled `authRefresh` leaves it
        // absent, making lease registration a no-op and preserving legacy
        // env-var delivery.
        if let Ok(session) = session.try_read() {
            frontend.set_host_output_policy(
                crate::command::host_output::resolve_host_output_policy(
                    &session.effective_config(),
                ),
            );
        }
        Self {
            catalogue: CommandCatalogue::get(),
            frontend,
            session,
            engines,
            squad_gateway: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            output_dir: None,
            picked_lease: None,
        }
    }

    /// Let an external cancel stop the dispatched command: `token` reaches
    /// the command as `BuildContext::cancel`. The CLI, TUI and squad never
    /// call this, so their commands see a token that never fires.
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    pub fn with_picked_lease(
        mut self,
        source: crate::command::worker::queue_source::SharedQueueSource,
        run: crate::data::worker::queue::PickRun,
    ) -> Self {
        self.picked_lease = Some((source, run));
        self
    }

    /// The command's `/awman/output` host directory (WI 0137 §3). Only the
    /// API queue worker sets it; the directory is created by the command
    /// that mounts it.
    pub fn with_output_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.output_dir = Some(dir);
        self
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
    pub fn with_task_gateway(mut self, gateway: Arc<dyn TaskGateway>) -> Self {
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

    /// Apply this invocation's global `--set key=value` flags to the session
    /// clone one command is built with.
    ///
    /// The CLI's `--set` is part of argv, so `main` already applied it to the
    /// process session at startup. The TUI command box and API `args` carry a
    /// `--set` per command: it changes that one command's session, never the
    /// tab's or the server's.
    fn apply_set_overrides(
        &self,
        path: &[&str],
        session: &mut Session,
    ) -> Result<(), CommandError> {
        if self.frontend.kind() == FrontendKind::Cli {
            return Ok(());
        }
        let set = self.frontend.flag_strings(path, catalogue::SET_FLAG)?;
        if set.is_empty() {
            return Ok(());
        }
        let overrides = crate::data::config::overrides::ConfigOverrides::parse_set_args(&set)
            .map_err(|e| CommandError::InvalidFlagValue {
                command: path.iter().map(|p| (*p).to_string()).collect(),
                flag: catalogue::SET_FLAG.into(),
                reason: e.to_string(),
            })?;
        session.add_overrides(overrides).map_err(CommandError::from)
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
        let mut session = self
            .session
            .try_read()
            .map_err(|_| CommandError::Other("session is write-locked".into()))?
            .clone();
        self.apply_set_overrides(&canonical, &mut session)?;
        let ctx = BuildContext {
            remote_target: self.frontend.flag_string(&canonical, "remote")?,
            set_args: crate::data::config::overrides::forwardable_set_args(
                &self.frontend.flag_strings(&canonical, "set")?,
            ),
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
            cancel: self.cancel.clone(),
            output_dir: self.output_dir.clone(),
        };
        (spec.build)(&ctx)
    }

    /// Validate credential flags before remote admission, using the same
    /// parser and policy ceiling as command construction. Returns names only.
    pub fn prerequisite_admission(
        &self,
        path: &[&str],
    ) -> Result<Option<serde_json::Value>, CommandError> {
        use crate::command::commands::workflow_prerequisites as prerequisites;
        let canonical: Vec<&str> = self.catalogue.canonical_path(path).into_iter().collect();
        let spec = self
            .catalogue
            .lookup(&canonical)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        if !spec
            .flags
            .iter()
            .any(|f| f.long == prerequisites::FLAG_TOKEN)
        {
            return Ok(None);
        }
        let flags = ResolvedFlags::resolve(&self.frontend, &canonical, spec)?;
        let args = ResolvedArgs::resolve(&self.frontend, &canonical, spec.arguments)?;
        let mut session = self
            .session
            .try_read()
            .map_err(|_| CommandError::Other("session is write-locked".into()))?
            .clone();
        let with_check = spec
            .flags
            .iter()
            .any(|f| f.long == prerequisites::FLAG_CHECK);
        let mut context_flags = session.flags().clone();
        context_flags.squad_prerequisites = !with_check;
        session.set_flags(context_flags);
        let ctx = BuildContext {
            remote_target: self.frontend.flag_string(&canonical, "remote")?,
            set_args: crate::data::config::overrides::forwardable_set_args(
                &self.frontend.flag_strings(&canonical, "set")?,
            ),
            flags: &flags,
            args: &args,
            engines: &self.engines,
            session,
            managed_session: self.session.clone(),
            gateway: self.squad_gateway.clone(),
            caller: CallerContext::new(&canonical, self.frontend.kind()),
            cancel: self.cancel.clone(),
            output_dir: self.output_dir.clone(),
        };
        let parsed = prerequisites::parse_prerequisite_flags(&ctx, with_check)?;
        prerequisites::check_flag_widening(
            self.frontend.kind(),
            &parsed,
            &prerequisites::policy_for(&ctx.session, &Default::default(), None),
        )?;
        let mut summary = prerequisites::flags_applied_policy(&ctx.session, &parsed);
        if summary["unresolved"].is_null() {
            let origin = if with_check && !ctx.flags.bool("dynamic") {
                crate::data::workflow_prerequisites::PrerequisiteOrigin::Static
            } else {
                crate::data::workflow_prerequisites::PrerequisiteOrigin::AssumedAll
            };
            summary["unresolved"] = serde_json::to_value(
                prerequisites::headless_for(self.frontend.kind()).prerequisite_unresolved(origin),
            )
            .expect("policy serializes");
        }
        Ok(Some(summary))
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
        // The TUI draws this before the command future starts. Use the same
        // fresh files and per-command overrides that run_command will use.
        let mut preview = session.clone();
        if preview.reload_config().is_err()
            || self.apply_set_overrides(&canonical, &mut preview).is_err()
        {
            return LaunchDisplay::unknown();
        }
        let agent_flag = spec.find_flag("agent").and_then(|_| flags.string("agent"));
        let agent_display_name = crate::command::commands::LaunchPolicy::for_session(&preview)
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
        let env = self.session.read().await.env().clone();
        let resolver = DaemonResolver::from_env(&env)?;
        self.squad_gateway = resolver.gateway_for(spec.gateway_need).await?;
        Ok(())
    }

    /// Build the requested command and drive it to completion.
    ///
    /// Two admissions run before the command is built, both driven by the
    /// catalogue rather than by a frontend's own list of command names
    /// (WI 0113 F-04): the runtime-tier guard, and squad gateway resolution.
    /// Running is then one call through the [`Command`] trait.
    pub async fn run_command(mut self, path: &[&str]) -> Result<CommandOutcome, CommandError> {
        // Sessions outlive commands in the TUI and API. Read every config
        // file again before admission or construction so this invocation uses
        // edits made since the session was opened or its previous command.
        {
            let mut session = self.session.write().await;
            if let Err(error) = session.reload_config() {
                session.state_mut().fail_command(error.to_string());
                return Err(error.into());
            }
        }
        self.admit(path).await?;
        let session = Arc::clone(&self.session);
        let built = if let Some((source, run)) = self.picked_lease.take() {
            BuiltCommand::QueuePick(
                crate::command::worker::pick::QueuePickCommand::new(
                    session.read().await.clone(),
                    self.engines.clone(),
                    None,
                    None,
                    false,
                )
                .prepared(source, run, self.cancel.clone()),
            )
        } else {
            self.build_command(path)?
        };

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

impl<F: DispatchFrontend + CliRelayFrontend> Dispatch<F> {
    /// Execute a CLI invocation using the backend chosen by the hand-off.
    /// `argv` is the invoking argv, including argv[0]. Only catalogue entries
    /// explicitly admitting a CLI relay can discover or start a daemon.
    pub async fn run_relayed(
        mut self,
        path: &[&str],
        argv: Vec<String>,
    ) -> Result<CliCompletion, crate::command::engine_handoff::HandoffError> {
        use crate::command::engine_handoff::{BackendSelection, EngineHandoff};
        use crate::command::integrations::IntegrationRegistry;

        if !self.catalogue.is_relay_allowed(path) {
            return Ok(CliCompletion::local(self.run_command(path).await));
        }
        {
            let mut session = self.session.write().await;
            if let Err(error) = session.reload_config() {
                session.state_mut().fail_command(error.to_string());
                return Ok(CliCompletion::local(Err(error.into())));
            }
        }
        let (env, config) = {
            let session = self.session.read().await;
            (session.env().clone(), session.effective_config())
        };
        let handoff = EngineHandoff::new(
            env.clone(),
            config.clone(),
            IntegrationRegistry::compiled_in(),
            Arc::new(CliHandoffPrompter),
        )
        .resolve()
        .await;
        let handoff = match handoff {
            Ok(handoff) => handoff,
            Err(error) if config.tui_backend() == crate::data::config::tui::TuiBackend::Auto => {
                self.frontend
                    .relay_notice(&error.message.replace(['\r', '\n'], " "));
                return Ok(CliCompletion::local(self.run_command(path).await));
            }
            Err(error) => return Err(error),
        };
        if let Some(notice) = &handoff.notice {
            self.frontend
                .relay_notice(&notice.replace(['\r', '\n'], " "));
        }
        let BackendSelection::Daemon { endpoint, .. } = handoff.selection else {
            return Ok(CliCompletion::local(self.run_command(path).await));
        };
        let facts = self.frontend.relay_facts();
        let hello = crate::data::frontend_protocol::Hello {
            protocol: crate::data::frontend_protocol::FRONTEND_SUBPROTOCOL.into(),
            build: env!("CARGO_PKG_VERSION").into(),
            kind: crate::data::frontend_protocol::FrontendKindWire::Cli,
            role: crate::data::frontend_protocol::FrontendRole::Primary,
            annotations: handoff.annotations,
            cwd: env.cwd().to_path_buf(),
            argv: argv.clone(),
            env: env.to_hello_env(),
            term_size: facts.term_size,
            stdin_is_tty: facts.stdin_is_tty,
            stdout_is_tty: facts.stdout_is_tty,
            view: None,
            resume: None,
            launch_context: None,
        };
        // No command has been submitted yet. A failure here can safely take
        // auto's builtin branch. After Submit, a failure must be reported.
        let connected = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            CliRelayConnection::open(&endpoint.address, hello),
        )
        .await
        .map_err(|_| CliConnectError::from("the engine did not answer the CLI handshake"))
        .and_then(|result| result);
        let connection = match connected {
            Ok(connection) => connection,
            Err(error) => {
                let notice = crate::command::engine_handoff::fallback_notice(&error.message);
                if config.tui_backend() == crate::data::config::tui::TuiBackend::Auto {
                    self.frontend
                        .relay_notice(&notice.replace(['\r', '\n'], " "));
                    return Ok(CliCompletion::local(self.run_command(path).await));
                }
                return Err(crate::command::engine_handoff::HandoffError {
                    message: notice,
                    exit_code: error.exit_code,
                });
            }
        };
        // Reconstruct the canonical argv from the frontend's already parsed
        // invocation. Global flags before the subcommand, aliases and values
        // containing spaces are handled by the catalogue, never by Layer 3.
        let args = self
            .relay_argv(path)
            .map_err(|error| cli_relay_failure(error.to_string()))?;
        connection
            .run(self.frontend, args, facts.term_size)
            .await
            .map(CliCompletion::Relayed)
            .map_err(cli_relay_failure)
    }
}

impl<F: CommandFrontend> Dispatch<F> {
    fn relay_argv(&self, path: &[&str]) -> Result<Vec<String>, CommandError> {
        use crate::command::dispatch::projections::raw_args::ParsedFlag;
        let canonical = self.catalogue.canonical_path(path);
        let refs: Vec<&str> = canonical.to_vec();
        let spec = self
            .catalogue
            .lookup(&refs)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        let mut argv: Vec<String> = canonical.iter().map(|s| (*s).to_string()).collect();
        let resolved = self.resolve_flags(path)?;
        // Use the same typed extraction as command construction, preserving
        // repeated values and not forwarding frontend default values.
        let mut values = Vec::new();
        for flag in spec.flags.iter().chain(self.catalogue.global_flags()) {
            use catalogue::FlagKind;
            let value = match flag.kind {
                FlagKind::Bool if flag.long == "headless" => {
                    resolved.bool(flag.long).then_some(ParsedFlag::Bool(true))
                }
                FlagKind::Bool => self
                    .frontend
                    .flag_bool(&refs, flag.long)?
                    .filter(|v| *v)
                    .map(ParsedFlag::Bool),
                FlagKind::String | FlagKind::OptionalString | FlagKind::Enum(_) => self
                    .frontend
                    .flag_string(&refs, flag.long)?
                    .map(ParsedFlag::Str),
                FlagKind::VecString => {
                    let list = self.frontend.flag_strings(&refs, flag.long)?;
                    (!list.is_empty()).then_some(ParsedFlag::Strings(list))
                }
                FlagKind::Path | FlagKind::OptionalPath => self
                    .frontend
                    .flag_path(&refs, flag.long)?
                    .map(ParsedFlag::Path),
                FlagKind::U16 => self
                    .frontend
                    .flag_u16(&refs, flag.long)?
                    .map(ParsedFlag::U16),
                FlagKind::UsizeAtLeastOne => self
                    .frontend
                    .flag_usize(&refs, flag.long)?
                    .map(ParsedFlag::Usize),
            };
            if let Some(value) = value {
                let items = match value {
                    ParsedFlag::Bool(_) => vec![],
                    ParsedFlag::Strings(items) => items,
                    ParsedFlag::Str(value) => vec![value],
                    ParsedFlag::Path(value) => vec![value.to_string_lossy().into_owned()],
                    ParsedFlag::U16(value) => vec![value.to_string()],
                    ParsedFlag::Usize(value) => vec![value.to_string()],
                };
                if items.is_empty() {
                    values.push(format!("--{}", flag.long));
                } else {
                    for value in items {
                        values.push(format!("--{}={value}", flag.long));
                    }
                }
            }
        }
        argv.extend(values);
        // `--` preserves positional values that begin with a dash.
        argv.push("--".into());
        for argument in spec.arguments {
            argv.extend(self.frontend.arguments(&refs, argument.name)?);
        }
        Ok(argv)
    }
}

struct CliHandoffPrompter;

#[async_trait::async_trait]
impl crate::command::engine_handoff::HandoffPrompter for CliHandoffPrompter {
    async fn confirm_daemon_restart(&self, _running_jobs: usize) -> bool {
        // The hand-off's typed failure carries the restart advice and code 2.
        false
    }
}

fn cli_relay_failure(message: String) -> crate::command::engine_handoff::HandoffError {
    crate::command::engine_handoff::HandoffError {
        message,
        exit_code: 1,
    }
}

struct CliRelayConnection {
    sender: crate::engine::remote::frontend_client::FrontendSender,
    receiver: crate::engine::remote::frontend_client::FrontendReceiver,
    tab_id: crate::data::ids::TabId,
}

struct CliConnectError {
    message: String,
    exit_code: u8,
}

impl From<String> for CliConnectError {
    fn from(message: String) -> Self {
        Self {
            message,
            exit_code: 1,
        }
    }
}

impl From<&str> for CliConnectError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

enum CliRenderEvent {
    Bytes(crate::data::frontend_protocol::OutputStream, Vec<u8>),
    Output(
        crate::data::frontend_protocol::OutputStream,
        crate::data::message::UserMessage,
    ),
    Notice(String),
    Dialog {
        id: crate::data::ids::DialogId,
        request: Box<crate::data::dialog::DialogRequest>,
        interrupted: tokio_util::sync::CancellationToken,
    },
    Finish(tokio::sync::oneshot::Sender<()>),
}

fn cli_interrupt_repeat(last: &mut Option<std::time::Instant>) -> bool {
    let now = std::time::Instant::now();
    let repeat =
        last.is_some_and(|last| now.duration_since(last) < std::time::Duration::from_secs(2));
    *last = Some(now);
    repeat
}

impl CliRelayConnection {
    async fn open(
        address: &std::path::Path,
        hello: crate::data::frontend_protocol::Hello,
    ) -> Result<Self, CliConnectError> {
        use crate::data::frontend_protocol::{EngineFrame, FrontendFrame, SnapshotAssembler};
        use crate::engine::remote::{frontend_client::FrontendStream, local::LocalClient};
        let client = LocalClient::new(address.to_path_buf());
        let response = client
            .send(
                "POST",
                "/v1/engine/instances",
                Some(&serde_json::json!({
                    "kind": "cli", "annotations": hello.annotations,
                })),
            )
            .await
            .map_err(|e| e.to_string())?;
        if response.status >= 400 {
            return Err(response
                .body
                .get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("the engine refused a CLI instance")
                .to_string()
                .into());
        }
        let id = response
            .body
            .get("id")
            .and_then(|s| s.as_str())
            .ok_or("the engine answered without an instance id")?
            .parse::<crate::data::ids::InstanceId>()
            .map_err(|e| e.to_string())?;
        let (mut sender, mut receiver) = FrontendStream::connect_local(address, &id)
            .await
            .map_err(|e| e.to_string())?
            .split();
        sender
            .send(&FrontendFrame::Hello(hello))
            .await
            .map_err(|e| e.to_string())?;
        let mut welcomed = false;
        let mut assembler = SnapshotAssembler::default();
        loop {
            let (_, frame) = receiver
                .next()
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the engine closed the CLI handshake")?;
            let snapshot = match frame {
                EngineFrame::Welcome {
                    protocol, build, ..
                } => {
                    if protocol != crate::data::frontend_protocol::FRONTEND_SUBPROTOCOL
                        || build != env!("CARGO_PKG_VERSION")
                    {
                        return Err(CliConnectError {
                            message: "the daemon version changed; restart it with `awman daemon stop`, or use `--set tui.backend=builtin`".into(),
                            exit_code: crate::command::engine_handoff::EXIT_STALE_DAEMON,
                        });
                    }
                    welcomed = true;
                    None
                }
                EngineFrame::Snapshot(snapshot) if welcomed => Some(snapshot),
                EngineFrame::SnapshotChunk { chunk, of, bytes } if welcomed => assembler
                    .push(chunk, of, &bytes)
                    .map_err(|e| e.to_string())?,
                _ => return Err("the engine did not send a CLI welcome and snapshot".into()),
            };
            if let Some(snapshot) = snapshot {
                let tab_id = snapshot
                    .tabs
                    .first()
                    .ok_or("the engine opened no CLI tab")?
                    .tab_id
                    .clone();
                return Ok(Self {
                    sender,
                    receiver,
                    tab_id,
                });
            }
        }
    }

    async fn run<F: CliRelayFrontend>(
        mut self,
        mut frontend: F,
        args: Vec<String>,
        pty_size: crate::data::frontend_protocol::TermSize,
    ) -> Result<crate::data::frontend_protocol::CommandResultWire, String> {
        use crate::data::frontend_protocol::{EngineFrame, FrontendFrame, SubmitInput};
        use crate::data::message::UserMessage;
        use tokio_util::sync::CancellationToken;
        // Engine traits have synchronous input methods. Keep rendering on a
        // separate thread so a prompt never prevents Cancel or DialogClosed
        // from reaching the socket. Each prompt has its own interruption token.
        let (render_tx, render_rx) = std::sync::mpsc::channel();
        let (answer_tx, mut answer_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("cli-relay-render".into())
            .spawn(move || {
                while let Ok(event) = render_rx.recv() {
                    match event {
                        CliRenderEvent::Bytes(stream, bytes) => {
                            frontend.relay_bytes(stream, &bytes)
                        }
                        CliRenderEvent::Output(stream, message) => {
                            frontend.relay_output(stream, message)
                        }
                        CliRenderEvent::Notice(text) => frontend.relay_notice(&text),
                        CliRenderEvent::Dialog {
                            id,
                            request,
                            interrupted,
                        } => {
                            let response = frontend.relay_dialog(*request, &interrupted);
                            let _ = answer_tx.send((id, response));
                        }
                        CliRenderEvent::Finish(done) => {
                            frontend.replay_queued();
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let lifetime = CancellationToken::new();
        let _interrupt_on_drop = lifetime.clone().drop_guard();
        let mut dialogs =
            std::collections::HashMap::<crate::data::ids::DialogId, CancellationToken>::new();
        self.sender
            .send(&FrontendFrame::Submit {
                tab_id: self.tab_id.clone(),
                input: SubmitInput::Argv { args },
                pty_size,
            })
            .await
            .map_err(|e| e.to_string())?;
        let mut last_interrupt = None;
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|e| e.to_string())?;
                    let repeat = cli_interrupt_repeat(&mut last_interrupt);
                    self.sender.send(&FrontendFrame::Cancel {tab_id: self.tab_id.clone(), repeat}).await.map_err(|e| e.to_string())?;
                    for (dialog_id, token) in dialogs.drain() {
                        token.cancel();
                        self.sender.send(&FrontendFrame::DialogResponse {dialog_id, response: crate::data::dialog::DialogResponse::Interrupted}).await.map_err(|e| e.to_string())?;
                    }
                }
                answer = answer_rx.recv() => {
                    let (dialog_id, response) = answer.ok_or("the CLI renderer stopped")?;
                    if dialogs.remove(&dialog_id).is_some() {
                        if response == crate::data::dialog::DialogResponse::Interrupted {
                            let repeat = cli_interrupt_repeat(&mut last_interrupt);
                            self.sender.send(&FrontendFrame::Cancel {tab_id: self.tab_id.clone(), repeat}).await.map_err(|e| e.to_string())?;
                        }
                        self.sender.send(&FrontendFrame::DialogResponse {dialog_id, response}).await.map_err(|e| e.to_string())?;
                    }
                }
                frame = self.receiver.next() => {
                    let (_, frame) = frame.map_err(|e| e.to_string())?.ok_or("the daemon disconnected before the command finished")?;
                    match frame {
                        EngineFrame::CommandOutput {tab_id, stream, level, text} if tab_id == self.tab_id => {let _ = render_tx.send(CliRenderEvent::Output(stream, UserMessage {level, text}));}
                        EngineFrame::CommandBytes {tab_id, stream, bytes} if tab_id == self.tab_id => {let _ = render_tx.send(CliRenderEvent::Bytes(stream, bytes));}
                        EngineFrame::Notice {text, ..} => {let _ = render_tx.send(CliRenderEvent::Notice(text));}
                        EngineFrame::DialogRequest {dialog_id, tab_id, request} if tab_id == self.tab_id => {
                            let interrupted = lifetime.child_token();
                            if request.awaits_answer() {dialogs.insert(dialog_id.clone(), interrupted.clone());}
                            let _ = render_tx.send(CliRenderEvent::Dialog {id: dialog_id, request: Box::new(request), interrupted});
                        }
                        EngineFrame::DialogClosed {dialog_id, ..} => {
                            if let Some(token) = dialogs.remove(&dialog_id) {token.cancel();}
                        }
                        EngineFrame::CommandFinished {tab_id, result} if tab_id == self.tab_id => {
                            lifetime.cancel();
                            let (done, flushed) = tokio::sync::oneshot::channel();
                            render_tx.send(CliRenderEvent::Finish(done)).map_err(|_| "the CLI renderer stopped")?;
                            flushed.await.map_err(|_| "the CLI renderer stopped")?;
                            let _ = self.sender.send(&FrontendFrame::Quit).await;
                            self.sender.close().await;
                            return Ok(result);
                        }
                        EngineFrame::Detach(reason) => return Err(format!("the engine detached the CLI: {}", reason.as_str())),
                        _ => {}
                    }
                }
            }
        }
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
    /// The panic log is located from the session's own environment.
    fn unexpected_end_message(env: &crate::data::config::env::EnvSnapshot) -> String {
        let log = crate::data::fs::PanicLog::from_env(env);
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
            let message = Self::unexpected_end_message(guard.env());
            let state = guard.state_mut();
            state.fail_command(message);
            state.set_current_workflow(None);
        }
    }
}

/// Move `$frontend` into the `Box<dyn *CommandFrontend>` each command's
/// [`Command`] impl takes, run it, and wrap the typed outcome in the matching
/// [`CommandOutcome`] variant. One line per command: the mapping is the only
/// thing that differs between arms. `ExecWorkflow` is the one exception: its
/// frontend may wrap itself first, through
/// [`ExecWorkflowCommandFrontend::into_exec_workflow_frontend`].
macro_rules! run_built_command {
    ($built:expr, $frontend:expr, { $($variant:ident => $frontend_trait:path),* $(,)? }) => {
        match $built {
            BuiltCommand::Queue(command) => command.list().await.map(CommandOutcome::Queue),
            BuiltCommand::QueuePick(command) => Box::pin(command.run_with_frontend(
                crate::command::worker::pick::DispatchQueueFrontend::new($frontend)
            )).await.map(CommandOutcome::QueuePick),
            BuiltCommand::ExecWorkflow(command) => command
                .run_with_frontend($frontend.into_exec_workflow_frontend())
                .await
                .map(CommandOutcome::ExecWorkflow),
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
    /// object. What it no longer carries is any per-command logic — only the
    /// variant-to-frontend-trait mapping below.
    pub async fn run_with_frontend<F: DispatchFrontend>(
        self,
        frontend: F,
    ) -> Result<CommandOutcome, CommandError> {
        run_built_command!(self, frontend, {
            Init => InitCommandFrontend,
            Ready => ReadyCommandFrontend,
            Chat => ChatCommandFrontend,
            Attach => AttachCommandFrontend,
            Specs => SpecsCommandFrontend,
            Agents => AgentsCommandFrontend,
            Config => ConfigCommandFrontend,
            WorkerLink => CommandFrontend,
            ExecPrompt => ExecPromptCommandFrontend,
            Daemon => DaemonCommandFrontend,
            DaemonApi => DaemonApiCommandFrontend,
            Link => LinkFrontend,
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

/// Typed `--on-dirty-merge-target`: `None` when not passed, so config and the
/// host's headless default apply.
pub(crate) fn parse_on_dirty_merge_target(
    raw: Option<String>,
    command: &[&str],
) -> Result<Option<crate::data::config::worktree::OnDirtyMergeTarget>, CommandError> {
    raw.map(|value| {
        value
            .parse()
            .map_err(|reason| CommandError::InvalidFlagValue {
                command: command.iter().map(|part| (*part).to_string()).collect(),
                flag: "on-dirty-merge-target".into(),
                reason,
            })
    })
    .transpose()
}

/// Typed `--on-merge-failure`: `None` when not passed.
pub(crate) fn parse_on_merge_failure(
    raw: Option<String>,
    command: &[&str],
) -> Result<Option<crate::data::config::worktree::OnMergeFailure>, CommandError> {
    raw.map(|value| {
        value
            .parse()
            .map_err(|reason| CommandError::InvalidFlagValue {
                command: command.iter().map(|part| (*part).to_string()).collect(),
                flag: "on-merge-failure".into(),
                reason,
            })
    })
    .transpose()
}

/// Typed `--on-existing-worktree`: `None` when not passed or `ask`, so the
/// host asks (or answers from its headless profile).
pub(crate) fn parse_on_existing_worktree(
    raw: Option<String>,
    command: &[&str],
) -> Result<
    Option<crate::command::commands::worktree_lifecycle::ExistingWorktreeDecision>,
    CommandError,
> {
    use crate::command::commands::worktree_lifecycle::ExistingWorktreeDecision;
    let Some(value) = raw else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "ask" => Ok(None),
        "resume" => Ok(Some(ExistingWorktreeDecision::Resume)),
        "recreate" => Ok(Some(ExistingWorktreeDecision::Recreate)),
        "fail" => Ok(Some(ExistingWorktreeDecision::Fail)),
        other => Err(CommandError::InvalidFlagValue {
            command: command.iter().map(|part| (*part).to_string()).collect(),
            flag: "on-existing-worktree".into(),
            reason: format!("unknown value '{other}' (expected ask, resume, recreate or fail)"),
        }),
    }
}

/// Typed `--agent-flag` values: `None` when the flag was not passed (so config
/// `agentFlags` applies), otherwise the list in order. Empty entries are
/// rejected with the same rule config uses.
pub(crate) fn parse_agent_flags(
    raw: &[String],
    command: &[&str],
) -> Result<Option<Vec<String>>, CommandError> {
    if raw.is_empty() {
        return Ok(None);
    }
    crate::data::config::agents::validate_agent_flag_values("--agent-flag", raw).map_err(|e| {
        CommandError::InvalidFlagValue {
            command: command.iter().map(|part| (*part).to_string()).collect(),
            flag: "agent-flag".into(),
            reason: e.to_string(),
        }
    })?;
    Ok(Some(raw.to_vec()))
}
