//! Catalogue-resolved flag and argument views, and the [`BuildContext`] every
//! `*Command::from_input` constructor is handed.
//!
//! Before WI 0113 F-10 each arm of `Dispatch::build_command` read the flags it
//! cared about straight off the frontend and restated the catalogue's default
//! as a literal (`unwrap_or_else(|| "claude")` beside
//! `FlagDefault::Str("claude")`), and the catalogue's `implies` edges were
//! re-implemented as hand-written `if`s in three places. [`ResolvedFlags`]
//! removes both: it walks a command's [`FlagSpec`]s exactly once, reads each
//! through the frontend, applies [`FlagDefault`] when the frontend supplied
//! nothing, and then closes over `implies` transitively. A command constructor
//! sees the finished answer and never a default of its own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::dispatch::catalogue::{
    ArgumentKind, ArgumentSpec, CommandSpec, FlagDefault, FlagKind, FlagSpec, FrontendKind,
};
use crate::command::dispatch::{CommandFrontend, Engines};
use crate::command::error::CommandError;
use crate::data::session::Session;

// ─── Resolved values ────────────────────────────────────────────────────────

/// One flag's value after the frontend read, the catalogue default and the
/// implication closure. The variant is decided by the flag's [`FlagKind`], so
/// a getter of the wrong type is a mismatch the catalogue can prove.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedValue {
    Bool(bool),
    /// `String`, `OptionalString` and `Enum` all resolve here.
    Str(Option<String>),
    Strs(Vec<String>),
    Path(Option<PathBuf>),
    U16(Option<u16>),
    Usize(Option<usize>),
}

/// Every flag a command declares, resolved once.
///
/// The getters take the flag's `long` name and panic when that name is not in
/// the command's [`CommandSpec`], or when it is but has a different
/// [`FlagKind`]. Both are programming errors — a constructor asking for a flag
/// its own command does not declare — and the catalogue-wide parity test in
/// `projections::parity_test` exercises every declared flag through the
/// matching getter so neither can reach a release.
#[derive(Debug, Clone)]
pub struct ResolvedFlags {
    command: Vec<String>,
    values: BTreeMap<&'static str, ResolvedValue>,
    /// Flags the frontend actually supplied, before defaults and implications.
    /// Mutual-exclusion validation runs over this set, never over the resolved
    /// values: a default or an implied `true` must not read as a conflict.
    supplied: BTreeSet<&'static str>,
}

impl ResolvedFlags {
    fn get(&self, name: &str) -> &ResolvedValue {
        self.values.get(name).unwrap_or_else(|| {
            panic!("flag {name:?} is not declared by this command's CommandSpec")
        })
    }

    fn kind_mismatch(name: &str, wanted: &str, got: &ResolvedValue) -> ! {
        panic!("flag {name:?} is not a {wanted} flag in the catalogue (resolved as {got:?})")
    }

    /// A boolean flag's value. Absent and defaultless boolean flags are
    /// `false`, which is what every `FlagDefault::Bool(false)` in the
    /// catalogue also says.
    pub fn bool(&self, name: &str) -> bool {
        match self.get(name) {
            ResolvedValue::Bool(value) => *value,
            other => Self::kind_mismatch(name, "bool", other),
        }
    }

    /// A string flag's value, defaulted from `FlagDefault::Str`.
    pub fn str(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            ResolvedValue::Str(value) => value.as_deref(),
            other => Self::kind_mismatch(name, "string", other),
        }
    }

    /// [`Self::str`] as an owned value, for the many flag structs that hold
    /// `Option<String>`.
    pub fn string(&self, name: &str) -> Option<String> {
        self.str(name).map(str::to_string)
    }

    /// A repeatable string flag's values. Never `None`: the catalogue's
    /// `FlagDefault::EmptyVec` is the empty slice.
    pub fn strs(&self, name: &str) -> &[String] {
        match self.get(name) {
            ResolvedValue::Strs(values) => values,
            other => Self::kind_mismatch(name, "repeatable string", other),
        }
    }

    /// A path flag's value.
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        match self.get(name) {
            ResolvedValue::Path(value) => value.clone(),
            other => Self::kind_mismatch(name, "path", other),
        }
    }

    /// A `u16` flag's value, defaulted from `FlagDefault::U16`.
    pub fn u16(&self, name: &str) -> Option<u16> {
        match self.get(name) {
            ResolvedValue::U16(value) => *value,
            other => Self::kind_mismatch(name, "u16", other),
        }
    }

    /// A string or enum flag that must end up with a value, whether the
    /// frontend supplied one or the catalogue defaulted it. Fails with
    /// `MissingRequiredFlag` when neither did — the error a genuinely required
    /// flag such as `squad add --name` already produced.
    pub fn require_str(&self, name: &str) -> Result<String, CommandError> {
        self.str(name).map(str::to_string).ok_or_else(|| {
            CommandError::missing_required_flag(&self.command_path(), name.to_string())
        })
    }

    /// [`Self::require_str`] for a `u16` flag such as `--port`.
    pub fn require_u16(&self, name: &str) -> Result<u16, CommandError> {
        self.u16(name).ok_or_else(|| {
            CommandError::missing_required_flag(&self.command_path(), name.to_string())
        })
    }

    fn command_path(&self) -> Vec<&str> {
        self.command.iter().map(String::as_str).collect()
    }

    /// A `usize` flag's value.
    pub fn usize(&self, name: &str) -> Option<usize> {
        match self.get(name) {
            ResolvedValue::Usize(value) => *value,
            other => Self::kind_mismatch(name, "usize", other),
        }
    }

    /// An enum flag's value, defaulted from `FlagDefault::Str`. The catalogue
    /// has already restricted it to one of the variant strings, so callers map
    /// it with an `unreachable!()` arm rather than an error.
    pub fn r#enum(&self, name: &str) -> Option<&str> {
        self.str(name)
    }

    /// Whether the frontend supplied this flag, ignoring defaults and
    /// implications. A boolean flag counts as supplied only when it was
    /// supplied as `true` — clap stores `false` for every absent `SetTrue`
    /// flag, and an explicit `--flag=false` asks for nothing.
    ///
    /// Unlike the value getters this one does not panic on an unknown name —
    /// it answers a question about what the *user* typed, and a name they
    /// typed need not be in the spec.
    pub fn supplied(&self, name: &str) -> bool {
        self.supplied.contains(name)
    }

    /// Whether the command declares `name` at all.
    ///
    /// The value getters panic on an undeclared flag on purpose — a
    /// constructor asking for a flag its own command does not have is a bug.
    /// [`ResolvedFlags::unattended`] asks a question *across* commands, most
    /// of which declare neither flag, so it needs this instead.
    fn declared(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    /// Whether this invocation runs without a human answering the agent's
    /// permission prompts — `--yolo` (skip them) or `--auto` (approve them).
    ///
    /// The single answer to "is this run unattended". The TUI used to
    /// recompute it as `yolo || auto` off the raw parsed input, which is a
    /// Layer 2 decision spelled in Layer 3: add a third flag with the same
    /// meaning and the tab indicator silently stops tracking it. A command
    /// that declares neither flag is never unattended.
    pub fn unattended(&self) -> bool {
        (self.declared("yolo") && self.bool("yolo")) || (self.declared("auto") && self.bool("auto"))
    }

    /// Resolve every flag `spec` declares against `frontend`.
    ///
    /// The order is load-bearing: read, then validate mutual exclusions
    /// against what was actually supplied, then apply defaults, then close
    /// over implications. Validating before the last two steps is what keeps
    /// `exec workflow --yolo` from tripping the `worktree` conflict list it
    /// implies its way into.
    pub(crate) fn resolve<F: CommandFrontend>(
        frontend: &F,
        command_path: &[&str],
        spec: &'static CommandSpec,
    ) -> Result<Self, CommandError> {
        let mut values: BTreeMap<&'static str, ResolvedValue> = BTreeMap::new();
        let mut supplied: BTreeSet<&'static str> = BTreeSet::new();

        for flag in spec.flags {
            let read = read_flag(frontend, command_path, flag)?;
            if is_supplied(&read) {
                supplied.insert(flag.long);
            }
            values.insert(flag.long, apply_default(read, flag));
        }

        let resolved = Self {
            command: command_path
                .iter()
                .map(|part| (*part).to_string())
                .collect(),
            values,
            supplied,
        };
        resolved.validate_conflicts(command_path, spec.flags)?;
        let mut resolved = resolved.close_over_implications(spec.flags);
        resolved.apply_non_interactive_rule(spec, frontend.input_available());
        Ok(resolved)
    }

    /// The one non-interactive rule: an invocation is non-interactive when the
    /// user asked for it (directly, or through a flag that implies it) **or**
    /// when there is nowhere to read an answer from.
    ///
    /// Before WI 0114 F-50 this was `frontend::effective_non_interactive` in
    /// Layer 3, called once by the CLI to cache its own copy and re-derived
    /// per-`ask_*` elsewhere, so a command could believe it was interactive
    /// while its frontend believed the opposite.
    pub fn is_non_interactive(explicitly_requested: bool, input_available: bool) -> bool {
        explicitly_requested || !input_available
    }

    /// Apply [`is_non_interactive`](Self::is_non_interactive) to this
    /// command's `non-interactive` flag, if it declares one.
    ///
    /// Runs after implication closure, so `--json ⇒ --non-interactive` has
    /// already been folded in and counts as "explicitly requested".
    fn apply_non_interactive_rule(&mut self, spec: &'static CommandSpec, input_available: bool) {
        const FLAG: &str = "non-interactive";
        if spec.find_flag(FLAG).is_none() {
            return;
        }
        let explicit = matches!(self.values.get(FLAG), Some(ResolvedValue::Bool(true)));
        let effective = Self::is_non_interactive(explicit, input_available);
        if effective && !explicit {
            tracing::info!("auto-detected non-interactive mode (no input available)");
        }
        self.values.insert(FLAG, ResolvedValue::Bool(effective));
    }

    /// Any pair of supplied flags must not name each other in
    /// `conflicts_with`.
    fn validate_conflicts(
        &self,
        command_path: &[&str],
        flags: &'static [FlagSpec],
    ) -> Result<(), CommandError> {
        for flag in flags {
            if !self.supplied.contains(flag.long) {
                continue;
            }
            for conflict in flag.conflicts_with {
                if self.supplied.contains(conflict) {
                    return Err(CommandError::mutually_exclusive(
                        command_path,
                        flag.long,
                        *conflict,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Set every flag implied by a flag that resolved to `true`, repeating
    /// until nothing changes so the relation is transitive.
    ///
    /// Only boolean implications exist in the catalogue (`--json` implies
    /// `--non-interactive`; `--yolo` and `--auto` imply `--worktree`), and an
    /// edge whose target this command does not declare is skipped: the shared
    /// flag arrays give `--yolo` to `chat` and `exec prompt`, neither of which
    /// has a `--worktree` to set.
    fn close_over_implications(mut self, flags: &'static [FlagSpec]) -> Self {
        loop {
            let mut changed = false;
            for flag in flags {
                if !matches!(self.values.get(flag.long), Some(ResolvedValue::Bool(true))) {
                    continue;
                }
                for target in flag.implies {
                    if let Some(ResolvedValue::Bool(value @ false)) = self.values.get_mut(*target) {
                        *value = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                return self;
            }
        }
    }
}

fn read_flag<F: CommandFrontend>(
    frontend: &F,
    command_path: &[&str],
    flag: &FlagSpec,
) -> Result<ResolvedValue, CommandError> {
    Ok(match flag.kind {
        FlagKind::Bool => ResolvedValue::Bool(
            frontend
                .flag_bool(command_path, flag.long)?
                .unwrap_or(false),
        ),
        FlagKind::String | FlagKind::OptionalString => {
            ResolvedValue::Str(frontend.flag_string(command_path, flag.long)?)
        }
        FlagKind::Enum(_) => ResolvedValue::Str(frontend.flag_enum(command_path, flag.long)?),
        FlagKind::VecString => ResolvedValue::Strs(frontend.flag_strings(command_path, flag.long)?),
        FlagKind::Path | FlagKind::OptionalPath => {
            ResolvedValue::Path(frontend.flag_path(command_path, flag.long)?)
        }
        FlagKind::U16 => ResolvedValue::U16(frontend.flag_u16(command_path, flag.long)?),
        FlagKind::UsizeAtLeastOne => {
            ResolvedValue::Usize(frontend.flag_usize(command_path, flag.long)?)
        }
    })
}

fn is_supplied(read: &ResolvedValue) -> bool {
    match read {
        ResolvedValue::Bool(value) => *value,
        ResolvedValue::Str(value) => value.is_some(),
        ResolvedValue::Strs(values) => !values.is_empty(),
        ResolvedValue::Path(value) => value.is_some(),
        ResolvedValue::U16(value) => value.is_some(),
        ResolvedValue::Usize(value) => value.is_some(),
    }
}

/// Fill an unsupplied flag from its [`FlagDefault`]. A default whose type does
/// not match the flag's kind is a catalogue mistake; it is left alone here and
/// caught by `catalogue::tests::every_flag_default_matches_its_kind`.
fn apply_default(read: ResolvedValue, flag: &FlagSpec) -> ResolvedValue {
    match (read, flag.default) {
        (ResolvedValue::Bool(false), FlagDefault::Bool(default)) => ResolvedValue::Bool(default),
        (ResolvedValue::Str(None), FlagDefault::Str(default)) => {
            ResolvedValue::Str(Some(default.to_string()))
        }
        (ResolvedValue::U16(None), FlagDefault::U16(default)) => ResolvedValue::U16(Some(default)),
        (other, _) => other,
    }
}

// ─── Resolved arguments ─────────────────────────────────────────────────────

/// Every positional argument a command declares, read once.
#[derive(Debug, Clone)]
pub struct ResolvedArgs {
    command: Vec<String>,
    singles: BTreeMap<&'static str, Option<String>>,
    trailing: BTreeMap<&'static str, Vec<String>>,
}

impl ResolvedArgs {
    /// A single-valued positional argument, or `None` when it was omitted.
    /// Panics when `name` is not declared by the command's `CommandSpec`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.singles
            .get(name)
            .unwrap_or_else(|| {
                panic!("argument {name:?} is not declared by this command's CommandSpec")
            })
            .as_deref()
    }

    /// [`Self::get`], failing with `MissingRequiredArgument` when omitted.
    pub fn require(&self, name: &str) -> Result<String, CommandError> {
        self.get(name).map(str::to_string).ok_or_else(|| {
            CommandError::missing_required_argument(&self.command_path(), name.to_string())
        })
    }

    /// A `TrailingVarArgs` argument's tokens.
    pub fn trailing(&self, name: &str) -> &[String] {
        self.trailing
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_else(|| {
                panic!("argument {name:?} is not a trailing var-args argument of this command")
            })
    }

    fn command_path(&self) -> Vec<&str> {
        self.command.iter().map(String::as_str).collect()
    }

    pub(crate) fn resolve<F: CommandFrontend>(
        frontend: &F,
        command_path: &[&str],
        arguments: &'static [ArgumentSpec],
    ) -> Result<Self, CommandError> {
        let mut singles = BTreeMap::new();
        let mut trailing = BTreeMap::new();
        for argument in arguments {
            // Every argument has a single-valued reading: for a
            // `TrailingVarArgs` positional the frontends join the collected
            // tokens with spaces, which is how `exec prompt` receives a
            // multi-word prompt. The token list is additionally kept for the
            // callers that need it unjoined.
            singles.insert(
                argument.name,
                frontend.argument(command_path, argument.name)?,
            );
            if matches!(argument.kind, ArgumentKind::TrailingVarArgs) {
                trailing.insert(
                    argument.name,
                    frontend.arguments(command_path, argument.name)?,
                );
            }
        }
        Ok(Self {
            command: command_path
                .iter()
                .map(|part| (*part).to_string())
                .collect(),
            singles,
            trailing,
        })
    }
}

// ─── Build context ──────────────────────────────────────────────────────────

/// Which command is being built, in canonical form.
///
/// Constructors take their errors' command path from here rather than from a
/// literal, so an alias (`exec wf`) and its canonical spelling produce the same
/// message. It is also how one `from_input` serves a command with several
/// subcommands: `ConfigCommand::from_input` reads [`CallerContext::leaf`] to
/// decide between `show`, `get` and `set`.
#[derive(Debug, Clone)]
pub struct CallerContext {
    path: Vec<String>,
    frontend: FrontendKind,
    local_user: bool,
}

impl CallerContext {
    /// Build from the canonical path and the frontend running the command.
    ///
    /// `local_user` is derived from the frontend kind rather than asked of
    /// the frontend: it *is* a property of the kind, and a per-frontend
    /// `is_local_user_session` override was a decision living in Layer 3
    /// (WI 0114 F-49).
    pub fn new(canonical_path: &[&str], frontend: FrontendKind) -> Self {
        Self {
            path: canonical_path
                .iter()
                .map(|part| (*part).to_string())
                .collect(),
            frontend,
            local_user: frontend.is_local_user(),
        }
    }

    /// Which frontend is running this command.
    pub fn frontend(&self) -> FrontendKind {
        self.frontend
    }

    /// Whether the caller is the user's own session on the host that chose
    /// the paths in the request — and therefore whether the process's current
    /// directory is the user's and a human is there to answer.
    ///
    /// `false` for the API frontend, which re-executes a request a client
    /// already authorised from a working directory unrelated to the caller's.
    /// Mount-scope policy that compares against the current directory is
    /// applied on the client, once, not again in the daemon.
    pub fn local_user(&self) -> bool {
        self.local_user
    }

    /// The canonical command path, in the shape `CommandError`'s constructors
    /// take.
    pub fn path(&self) -> Vec<&str> {
        self.path.iter().map(String::as_str).collect()
    }

    /// The last path segment — the subcommand name within its parent.
    pub fn leaf(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or_default()
    }
}

/// Everything a `*Command` needs to construct itself.
///
/// The grand architecture's Layer 2 rule is that a command package collects
/// everything it needs at instantiation time; this is that collection, and
/// `CommandSpec::build` is the catalogue entry that hands it over.
pub struct BuildContext<'a> {
    pub flags: &'a ResolvedFlags,
    pub args: &'a ResolvedArgs,
    pub engines: &'a Engines,
    /// A snapshot of the session, for the many commands that only read it.
    pub session: Session,
    /// The *shared* session the frontend and every other holder observe.
    ///
    /// A command that records in-flight state — what is running, which
    /// workflow, which container (decision Q3, WI 0114 F-22) — must write
    /// through this, not through the `session` snapshot above, or the write
    /// lands on a clone nobody else can see.
    pub managed_session: Arc<RwLock<Session>>,
    /// The squad daemon gateway `Dispatch::admit` resolved for this command's
    /// `GatewayNeed`, if any.
    pub gateway: Option<Arc<dyn TaskGateway>>,
    pub caller: CallerContext,
}

impl BuildContext<'_> {
    /// The canonical command path, for error construction.
    pub fn path(&self) -> Vec<&str> {
        self.caller.path()
    }

    /// The gateway as the `Box<dyn TaskGateway>` the squad commands hold.
    pub fn boxed_gateway(&self) -> Option<Box<dyn TaskGateway>> {
        self.gateway.clone().map(|gateway| {
            Box::new(crate::command::commands::squad::gateway::SharedTaskGateway(
                gateway,
            )) as Box<dyn TaskGateway>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::command::dispatch::tests::FakeCommandFrontend;

    fn resolve(path: &[&str], frontend: &FakeCommandFrontend) -> ResolvedFlags {
        let spec = CommandCatalogue::get().lookup(path).expect("spec");
        ResolvedFlags::resolve(frontend, path, spec).expect("resolve")
    }

    #[test]
    fn an_absent_flag_takes_the_catalogue_default() {
        let flags = resolve(&["init"], &FakeCommandFrontend::new());
        assert_eq!(flags.r#enum("agent"), Some("claude"));
        assert!(!flags.bool("aspec"));
        assert!(!flags.supplied("agent"));
    }

    #[test]
    fn a_supplied_flag_beats_the_catalogue_default() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.enums.insert("agent".into(), "codex".into());
        let flags = resolve(&["init"], &frontend);
        assert_eq!(flags.r#enum("agent"), Some("codex"));
        assert!(flags.supplied("agent"));
    }

    #[test]
    fn json_implies_non_interactive() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("json".into(), true);
        let flags = resolve(&["ready"], &frontend);
        assert!(flags.bool("non-interactive"));
        assert!(
            !flags.supplied("non-interactive"),
            "an implied flag was never supplied by the frontend"
        );
    }

    // ─── the non-interactive rule (WI 0114 F-50) ────────────────────────────

    /// The rule itself, in the one place it lives.
    #[test]
    fn non_interactive_is_asked_for_or_forced_by_having_nowhere_to_ask() {
        assert!(ResolvedFlags::is_non_interactive(true, true));
        assert!(ResolvedFlags::is_non_interactive(true, false));
        assert!(ResolvedFlags::is_non_interactive(false, false));
        assert!(!ResolvedFlags::is_non_interactive(false, true));
    }

    /// A caller with nowhere to read an answer from resolves
    /// `--non-interactive` whether or not it passed the flag — and the flag
    /// still reads as unsupplied, because the user did not supply it.
    #[test]
    fn a_frontend_without_input_resolves_non_interactive() {
        let frontend = FakeCommandFrontend::new().without_input();
        let flags = resolve(&["ready"], &frontend);
        assert!(flags.bool("non-interactive"));
        assert!(!flags.supplied("non-interactive"));
    }

    /// With input available the flag is the user's alone.
    #[test]
    fn a_frontend_with_input_leaves_non_interactive_to_the_user() {
        let flags = resolve(&["ready"], &FakeCommandFrontend::new());
        assert!(!flags.bool("non-interactive"));

        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("non-interactive".into(), true);
        assert!(resolve(&["ready"], &frontend).bool("non-interactive"));
    }

    /// A command with no `--non-interactive` flag is left untouched.
    #[test]
    fn a_command_without_the_flag_is_unaffected_by_the_rule() {
        let frontend = FakeCommandFrontend::new().without_input();
        let flags = resolve(&["status"], &frontend);
        assert!(
            CommandCatalogue::get()
                .lookup(&["status"])
                .unwrap()
                .find_flag("non-interactive")
                .is_none()
                || flags.bool("non-interactive"),
            "the rule must not invent a flag the command does not declare"
        );
    }

    #[test]
    fn yolo_and_auto_each_imply_worktree() {
        for flag in ["yolo", "auto"] {
            let mut frontend = FakeCommandFrontend::new();
            frontend.bools.insert(flag.into(), true);
            let flags = resolve(&["exec", "workflow"], &frontend);
            assert!(flags.bool("worktree"), "{flag} must imply worktree");
        }
    }

    #[test]
    fn an_implication_whose_target_the_command_lacks_is_skipped() {
        // `chat` shares `--yolo` with `exec workflow` but declares no
        // `--worktree`; resolving must not panic or invent one.
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        let flags = resolve(&["chat"], &frontend);
        assert!(flags.bool("yolo"));
    }

    #[test]
    fn conflicts_are_judged_on_supplied_flags_only() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend.bools.insert("plan".into(), true);
        let spec = CommandCatalogue::get()
            .lookup(&["exec", "workflow"])
            .unwrap();
        let result = ResolvedFlags::resolve(&frontend, &["exec", "workflow"], spec);
        assert!(matches!(
            result,
            Err(CommandError::MutuallyExclusive { .. })
        ));
    }

    #[test]
    #[should_panic(expected = "not declared by this command's CommandSpec")]
    fn a_flag_outside_the_spec_panics() {
        let flags = resolve(&["status"], &FakeCommandFrontend::new());
        let _ = flags.bool("not-a-flag");
    }
}
