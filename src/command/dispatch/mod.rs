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

use crate::command::commands::agents::{AgentsCommand, AgentsCommandFrontend};
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
use crate::command::commands::squad::attach::{SquadAttachCommand, SquadAttachFrontend};
use crate::command::commands::squad::commands::{SquadCommand, SquadCommandFrontend};
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::commands::squad::supervisor::SquadGatewayResolver;
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
    + AgentsCommandFrontend
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
        + AgentsCommandFrontend
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
    Agents(AgentsCommand),
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
    /// Handed to the built command through `BuildContext::cancel`.
    cancel: tokio_util::sync::CancellationToken,
    /// Handed to the built command through `BuildContext::output_dir`.
    output_dir: Option<std::path::PathBuf>,
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
            cancel: tokio_util::sync::CancellationToken::new(),
            output_dir: None,
        }
    }

    /// Let an external cancel stop the dispatched command: `token` reaches
    /// the command as `BuildContext::cancel`. The CLI, TUI and squad never
    /// call this, so their commands see a token that never fires.
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
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
/// thing that differs between arms. `ExecWorkflow` is the one exception: its
/// frontend may wrap itself first, through
/// [`ExecWorkflowCommandFrontend::into_exec_workflow_frontend`].
macro_rules! run_built_command {
    ($built:expr, $frontend:expr, { $($variant:ident => $frontend_trait:path),* $(,)? }) => {
        match $built {
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
            Specs => SpecsCommandFrontend,
            Agents => AgentsCommandFrontend,
            Config => ConfigCommandFrontend,
            ExecPrompt => ExecPromptCommandFrontend,
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

/// Typed `--on-dirty-merge-target`: `None` when not passed, so config and the
/// host's headless default apply.
pub(crate) fn parse_on_dirty_merge_target(
    raw: Option<String>,
    command: &[&str],
) -> Result<Option<crate::data::config::repo::OnDirtyMergeTarget>, CommandError> {
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
) -> Result<Option<crate::data::config::repo::OnMergeFailure>, CommandError> {
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
    crate::data::config::repo::validate_agent_flag_values("--agent-flag", raw).map_err(|e| {
        CommandError::InvalidFlagValue {
            command: command.iter().map(|part| (*part).to_string()).collect(),
            flag: "agent-flag".into(),
            reason: e.to_string(),
        }
    })?;
    Ok(Some(raw.to_vec()))
}

#[cfg(all(test, unix))]
mod builtin_launch_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_existing_worktree_parses_all_choices_and_ask_as_unset() {
        use crate::command::commands::worktree_lifecycle::ExistingWorktreeDecision as D;
        let command = &["exec", "workflow"];
        assert_eq!(parse_on_existing_worktree(None, command).unwrap(), None);
        assert_eq!(
            parse_on_existing_worktree(Some("ask".into()), command).unwrap(),
            None
        );
        assert_eq!(
            parse_on_existing_worktree(Some(" RESUME ".into()), command).unwrap(),
            Some(D::Resume)
        );
        assert_eq!(
            parse_on_existing_worktree(Some("Recreate".into()), command).unwrap(),
            Some(D::Recreate)
        );
        assert_eq!(
            parse_on_existing_worktree(Some("fail".into()), command).unwrap(),
            Some(D::Fail)
        );
    }

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
        use crate::data::session::InvocationStatus;

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
            InvocationStatus::Error(message) => assert!(
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
        use crate::data::session::InvocationStatus;

        let session = make_session();
        {
            let mut guard = session.try_write().unwrap();
            let state = guard.state_mut();
            state.begin_command("agents".to_string(), Vec::new());
            state.finish_command(0);
        }

        let mut armed = UnfinishedCommandGuard::arm(Arc::clone(&session));
        armed.disarm();
        drop(armed);

        let guard = session.try_read().unwrap();
        let command = guard.state().current_command.as_ref().unwrap();
        assert!(matches!(command.status, InvocationStatus::Done));
        assert_eq!(command.exit_code, Some(0));
    }

    #[test]
    fn build_agents_status_command_with_no_flags() {
        let dispatch = Dispatch::new(
            FakeCommandFrontend::new(),
            make_session(),
            Engines::for_tests(std::path::Path::new("/tmp")),
        );
        let built = dispatch.build_command(&["agents", "status"]).unwrap();
        match built {
            BuiltCommand::Agents(AgentsCommand::Status(_)) => {}
            _ => panic!("expected agents status"),
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

    /// A built config command keeps the once-per-repo state of the host's
    /// bundle, so a notice is not repeated for each command the host runs.
    #[test]
    fn build_config_shares_the_bundles_notices_handle() {
        let engines = Engines::for_tests(std::path::Path::new("/tmp"));
        let notices = engines.config_notices.clone();
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), engines);
        let BuiltCommand::Config(command) = dispatch.build_command(&["config", "show"]).unwrap()
        else {
            panic!("expected Config");
        };
        assert!(Arc::ptr_eq(command.notices(), &notices));
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
        assert_eq!(parsed.args.flag_bool("yolo"), Some(true));
        assert_eq!(
            parsed.args.argument("workflow").as_deref(),
            Some("my-workflow.toml")
        );
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
        let result =
            Dispatch::<FakeCommandFrontend>::parse_command_box_input("agents status --bogus");
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
        assert_eq!(
            parsed.args.argument("workflow").as_deref(),
            Some("my-workflow.toml")
        );
        assert_eq!(parsed.args.flag_bool("follow"), Some(true));
    }

    #[test]
    fn parse_command_box_input_short_flag_non_interactive() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input("ready -n").unwrap();
        assert_eq!(parsed.path, vec!["ready"]);
        assert_eq!(parsed.args.flag_bool("non-interactive"), Some(true));
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

    #[test]
    fn parse_agent_flags_absent_is_none_so_config_applies() {
        assert_eq!(parse_agent_flags(&[], &["chat"]).unwrap(), None);
    }

    #[test]
    fn parse_agent_flags_keeps_values_in_order() {
        let raw = vec![
            "--no-daemon".to_string(),
            "--config".to_string(),
            "a=b".to_string(),
        ];
        assert_eq!(parse_agent_flags(&raw, &["chat"]).unwrap(), Some(raw));
    }

    #[test]
    fn parse_agent_flags_rejects_an_empty_entry() {
        let raw = vec!["--no-daemon".to_string(), String::new()];
        match parse_agent_flags(&raw, &["exec", "prompt"]).unwrap_err() {
            CommandError::InvalidFlagValue { command, flag, .. } => {
                assert_eq!(command, vec!["exec", "prompt"]);
                assert_eq!(flag, "agent-flag");
            }
            other => panic!("expected InvalidFlagValue, got {other:?}"),
        }
    }
}
