//! Raw-args projection: catalogue-driven parsing of a pre-tokenized argv.
//!
//! Frontends that receive already-tokenized argument vectors — chiefly the API
//! frontend, which takes an `args: [String]` array straight off an HTTP request
//! body — hand those strings to [`CommandCatalogue::parse_raw_args`] instead of
//! hand-rolling their own flag parser. Every decision (which flags exist, what
//! type each flag/positional carries, which positional is a greedy trailing
//! argument) is derived from the canonical [`CommandSpec`] data, so API-mode
//! parsing can never drift from the clap (CLI) or command-box (TUI) projections.
//!
//! This is also the TUI command box's parser. `parsed_input::parse` splits the
//! submitted string and resolves the command path, then hands the remaining
//! tokens here (WI 0114 F-26); it used to carry a second flag loop of its own,
//! which accepted bad enum values and non-numeric numbers the API rejected.
//! Values are coerced to their declared types (u16/usize/path/enum) with a
//! structured [`CommandError`] on mismatch — mirroring how clap rejects the
//! same input in CLI mode.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::command::dispatch::catalogue::{
    ArgumentKind, CommandCatalogue, CommandSpec, FlagDefault, FlagKind, FlagSpec, FrontendKind,
    FrontendVisibility,
};
use crate::command::error::CommandError;

/// A single parsed flag value, typed per the catalogue's declared [`FlagKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedFlag {
    Bool(bool),
    Str(String),
    Strings(Vec<String>),
    Path(PathBuf),
    U16(u16),
    Usize(usize),
}

/// Typed result of [`CommandCatalogue::parse_raw_args`]. Frontends read values
/// out of it through the accessor methods, which map 1:1 onto the
/// `CommandFrontend` trait surface. This is the one parsed representation the
/// API and TUI dispatch from; nothing downstream re-validates its values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    flags: BTreeMap<String, ParsedFlag>,
    /// Positional arguments, keyed by their catalogue name. Stored as a vector
    /// so a greedy trailing argument keeps every token; single-valued
    /// positionals hold a one-element vector.
    arguments: BTreeMap<String, Vec<String>>,
    /// Names of positional arguments whose declared kind is a path. These are
    /// also reachable via [`ParsedArgs::flag_path`] so a command that reads a
    /// path positional through `flag_path` (e.g. `exec workflow`) still works.
    path_arguments: BTreeSet<String>,
}

impl ParsedArgs {
    /// A parsed invocation a frontend raises itself: `bool_flags` set true and
    /// at most one positional. Callers check both against the command's spec.
    pub(crate) fn for_action(bool_flags: &[&str], argument: Option<(&str, &str)>) -> Self {
        Self {
            flags: bool_flags
                .iter()
                .map(|name| ((*name).to_string(), ParsedFlag::Bool(true)))
                .collect(),
            arguments: argument
                .map(|(name, value)| (name.to_string(), vec![value.to_string()]))
                .into_iter()
                .collect(),
            path_arguments: BTreeSet::new(),
        }
    }

    /// Whether the invocation set `flag`, whatever its type.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.contains_key(flag)
    }

    pub fn flag_bool(&self, flag: &str) -> Option<bool> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn flag_string(&self, flag: &str) -> Option<String> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Str(s)) => Some(s.clone()),
            _ => None,
        }
    }

    pub fn flag_strings(&self, flag: &str) -> Vec<String> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Strings(v)) => v.clone(),
            _ => Vec::new(),
        }
    }

    pub fn flag_path(&self, flag: &str) -> Option<PathBuf> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Path(p)) => Some(p.clone()),
            // A path-typed positional is reachable via flag_path too, matching
            // the pre-refactor API behavior and Dispatch's flag_path-then-
            // argument fallback for `exec workflow` / `remote exec workflow`.
            _ if self.path_arguments.contains(flag) => {
                self.arguments.get(flag).map(|v| PathBuf::from(v.join(" ")))
            }
            _ => None,
        }
    }

    pub fn flag_enum(&self, flag: &str) -> Option<String> {
        // Enum values are stored as strings (as in the clap projection).
        self.flag_string(flag)
    }

    pub fn flag_u16(&self, flag: &str) -> Option<u16> {
        match self.flags.get(flag) {
            Some(ParsedFlag::U16(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn flag_usize(&self, flag: &str) -> Option<usize> {
        match self.flags.get(flag) {
            Some(ParsedFlag::Usize(n)) => Some(*n),
            _ => None,
        }
    }

    /// A single positional argument. A greedy trailing argument collapses to
    /// its tokens joined by single spaces (preserving the historical
    /// `exec prompt` behavior spec-driven rather than special-cased).
    pub fn argument(&self, name: &str) -> Option<String> {
        self.arguments.get(name).map(|v| v.join(" "))
    }

    pub fn arguments(&self, name: &str) -> Vec<String> {
        self.arguments.get(name).cloned().unwrap_or_default()
    }

    /// The argv tokens (after the command path) that
    /// [`CommandCatalogue::parse_raw_args`] parses back into `self` for
    /// `spec`. A presenter whose engine runs elsewhere sends these
    /// (`SubmitInput::Argv`), so the engine re-parses through the same
    /// catalogue parser instead of trusting a parse it did not make (D3).
    ///
    /// Flags use the inline `--name=value` form, so a value starting with `-`
    /// stays a value; positionals follow a `--` in the spec's order.
    pub fn to_argv(&self, spec: &CommandSpec) -> Vec<String> {
        let mut argv = Vec::new();
        for (name, value) in &self.flags {
            match value {
                ParsedFlag::Bool(true) => argv.push(format!("--{name}")),
                ParsedFlag::Bool(false) => argv.push(format!("--{name}=false")),
                ParsedFlag::Str(text) => argv.push(format!("--{name}={text}")),
                ParsedFlag::Strings(items) => {
                    argv.extend(items.iter().map(|item| format!("--{name}={item}")))
                }
                ParsedFlag::Path(path) => argv.push(format!("--{name}={}", path.display())),
                ParsedFlag::U16(n) => argv.push(format!("--{name}={n}")),
                ParsedFlag::Usize(n) => argv.push(format!("--{name}={n}")),
            }
        }
        let positionals: Vec<String> = spec
            .arguments
            .iter()
            .flat_map(|argument| self.arguments(argument.name))
            .collect();
        if !positionals.is_empty() {
            argv.push("--".into());
            argv.extend(positionals);
        }
        argv
    }
}

// ─── Per-frontend flag-default policy (Finding D) ────────────────────────────

/// One declarative per-frontend flag default. `forced` distinguishes a policy
/// that overrides any request-supplied value from one that only fills a default
/// when the caller omitted the flag.
pub(crate) struct FrontendFlagDefault {
    pub(crate) flag: &'static str,
    pub(crate) value: FlagDefault,
    /// `true`: override even an explicit request value (technical requirement).
    /// `false`: apply only when the flag was not supplied (overridable default).
    pub(crate) forced: bool,
}

/// API-profile flag defaults.
///
/// `headless` is an overridable DEFAULT (WI 0141): it is
/// `daemon.features.worker.push.interaction`'s own default, `headless`.
/// It is no longer forced. Interaction is a per-feature setting resolved in
/// Layer 2 (`command::interaction::resolve_interaction`); push admission
/// always writes the resolved choice into the stored args as an explicit
/// `--headless` or `--interactive`, so this default only fills in for a
/// caller that parses daemon args without going through admission. An
/// explicit `--interactive` suppresses it (the two flags conflict). Whether
/// the agent then gets a PTY is decided by the `InteractionDecision` handed
/// to the frontend, not by sniffing args.
///
/// `yolo` is an overridable DEFAULT: it stays `true` for backward compatibility
/// (every historical API caller ran with auto-approval), but a request payload
/// that passes `--yolo`/`--plan` etc. can now change it. The policy lives here,
/// next to the profile, instead of being an unadvertised side effect buried in
/// the API frontend. See work item 0097, Finding D.
const API_FLAG_DEFAULTS: &[FrontendFlagDefault] = &[
    FrontendFlagDefault {
        flag: "headless",
        value: FlagDefault::Bool(true),
        forced: false,
    },
    FrontendFlagDefault {
        flag: "yolo",
        value: FlagDefault::Bool(true),
        forced: false,
    },
];

/// The flag defaults a frontend profile applies, as data.
///
/// `pub(crate)` so the API frontend can *report* its own profile rather than
/// hand-writing `{"yolo": true, "non_interactive": true}` into a response
/// body — the literal F-50 found in `api/routes.rs`.
pub(crate) fn frontend_flag_defaults(frontend: FrontendKind) -> &'static [FrontendFlagDefault] {
    match frontend {
        // An engine instance serves a person at a local frontend: no
        // unattended profile (parity with the TUI).
        FrontendKind::Daemon { .. } if frontend.is_engine_local() => &[],
        // The squad daemon runs unattended for the same reasons the API
        // server does, and applies the same profile.
        FrontendKind::Daemon { .. } => API_FLAG_DEFAULTS,
        // CLI and TUI resolve flag defaults through clap / the command layer;
        // no profile overrides apply, guaranteeing this mechanism cannot alter
        // their behavior.
        FrontendKind::Cli | FrontendKind::Tui | FrontendKind::Relayed { .. } => &[],
    }
}

fn flag_default_to_parsed(value: &FlagDefault) -> Option<ParsedFlag> {
    match value {
        FlagDefault::Bool(b) => Some(ParsedFlag::Bool(*b)),
        FlagDefault::Str(s) => Some(ParsedFlag::Str((*s).to_string())),
        FlagDefault::U16(n) => Some(ParsedFlag::U16(*n)),
        FlagDefault::None | FlagDefault::EmptyVec => None,
    }
}

/// Whether the caller asked for `name`, by the rule of
/// [`ResolvedFlags::supplied`](crate::command::dispatch::resolved::ResolvedFlags::supplied):
/// an explicit `--flag=false` asks for nothing, so it conflicts with nothing.
fn flag_is_active(flags: &BTreeMap<String, ParsedFlag>, name: &str) -> bool {
    flags
        .get(name)
        .is_some_and(|value| !matches!(value, ParsedFlag::Bool(false)))
}

/// Apply the frontend profile's flag defaults to an already-parsed argv. Only
/// flags the command actually declares are touched, so commands without the
/// flag are left untouched.
fn apply_frontend_defaults(parsed: &mut ParsedArgs, spec: &CommandSpec, frontend: FrontendKind) {
    for entry in frontend_flag_defaults(frontend) {
        let Some(flag_spec) = spec.find_flag(entry.flag) else {
            continue;
        };
        let Some(value) = flag_default_to_parsed(&entry.value) else {
            continue;
        };
        if entry.forced {
            parsed.flags.insert(entry.flag.to_string(), value);
            continue;
        }
        // Overridable default: fill only when absent. An explicitly supplied
        // value wins, and so does a mutually-exclusive flag the caller passed —
        // otherwise the fills-when-absent default would silently reintroduce a
        // conflict (e.g. `--plan` must genuinely suppress the `yolo` default
        // rather than leaving both set for dispatch to reject later).
        if parsed.flags.contains_key(entry.flag) {
            continue;
        }
        // Conflicts are symmetric but may be declared on one side only
        // (`--interactive` declares its conflict with the shared
        // `--headless` spec), so check both directions.
        let conflict_present = flag_spec
            .conflicts_with
            .iter()
            .any(|other| flag_is_active(&parsed.flags, other))
            || spec.flags.iter().any(|other| {
                other.conflicts_with.contains(&entry.flag)
                    && flag_is_active(&parsed.flags, other.long)
            });
        if conflict_present {
            continue;
        }
        parsed.flags.insert(entry.flag.to_string(), value);
    }
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

impl CommandCatalogue {
    /// Parse a pre-tokenized argv against the command at `path`.
    ///
    /// `path` is the resolved command path (e.g. `["exec", "prompt"]`); `args`
    /// is the flag/positional token vector with the command path already
    /// stripped. Flags, positionals, types, and the greedy trailing positional
    /// are all derived from the [`CommandSpec`] — there are no per-command
    /// special cases.
    pub fn parse_raw_args(
        &self,
        path: &[&str],
        args: &[String],
    ) -> Result<ParsedArgs, CommandError> {
        let spec = self
            .lookup_with_aliases(path)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        let canonical = self.canonical_path(path);
        parse_against_spec(spec, &canonical, args)
    }

    /// Like [`parse_raw_args`](Self::parse_raw_args) but additionally applies
    /// the declarative per-frontend flag defaults for `frontend` (Finding D).
    pub fn parse_raw_args_with_profile(
        &self,
        path: &[&str],
        args: &[String],
        frontend: FrontendKind,
    ) -> Result<ParsedArgs, CommandError> {
        let spec = self
            .lookup_with_aliases(path)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        let canonical = self.canonical_path(path);
        let mut parsed = parse_against_spec(spec, &canonical, args)?;
        // Visibility is also an admission boundary. In particular, API and
        // squad workers must not accept local-only host-mount selectors.
        // Check parsed flags so greedy prompt tokens remain prompt text.
        for flag in spec.flags {
            if !parsed.flags.contains_key(flag.long) {
                continue;
            }
            let allowed = match flag.frontends {
                FrontendVisibility::All => true,
                // A relayed frontend sees the flags of the frontend relaying it.
                FrontendVisibility::CliOnly => frontend.presenting() == Some(FrontendKind::Cli),
                FrontendVisibility::TuiOnly => {
                    frontend.presenting() == Some(FrontendKind::Tui) || frontend.is_engine_local()
                }
                FrontendVisibility::CliAndTui => {
                    frontend.presenting().is_some() || frontend.is_engine_local()
                }
                FrontendVisibility::Hidden => false,
            };
            // A forced technical setting is applied even without an argv
            // token, and the client cannot override its value. Accepting its
            // spelling (e.g. a daemon's headless mode) grants nothing.
            let forced_by_profile = frontend_flag_defaults(frontend)
                .iter()
                .any(|entry| entry.flag == flag.long && entry.forced);
            if !allowed && !forced_by_profile {
                return Err(CommandError::InvalidFlagValue {
                    command: canonical.iter().map(|part| (*part).to_string()).collect(),
                    flag: flag.long.to_string(),
                    reason: format!("not available via the {} frontend", frontend.label()),
                });
            }
        }
        apply_frontend_defaults(&mut parsed, spec, frontend);
        Ok(parsed)
    }

    /// The flags a frontend profile forces or defaults for `path`, as
    /// `(flag name, value)` pairs — the profile's own account of itself.
    ///
    /// A frontend that reports its profile to a client reads it here rather
    /// than restating it: `api/routes.rs` used to answer `POST /v1/worker/commands`
    /// with a hand-written `{"yolo": true, "non_interactive": true}` that no
    /// test tied to `API_FLAG_DEFAULTS` (WI 0114 F-50). Flags the command
    /// does not declare are left out, so the report matches what was applied.
    pub fn frontend_profile(
        &self,
        frontend: FrontendKind,
        path: &[&str],
    ) -> Vec<(&'static str, FlagDefault)> {
        let Some(spec) = self.lookup_with_aliases(path) else {
            return Vec::new();
        };
        frontend_flag_defaults(frontend)
            .iter()
            .filter(|entry| spec.find_flag(entry.flag).is_some())
            .map(|entry| (entry.flag, entry.value))
            .collect()
    }
}

pub(crate) fn parse_against_spec(
    spec: &CommandSpec,
    path: &[&str],
    args: &[String],
) -> Result<ParsedArgs, CommandError> {
    let mut flags: BTreeMap<String, ParsedFlag> = BTreeMap::new();
    let mut positionals: Vec<String> = Vec::new();

    // Whether the command's last positional is a greedy trailing var-arg, and
    // how many fixed positionals precede it. Once positional collection reaches
    // the trailing argument, remaining tokens (including hyphen-prefixed ones)
    // are captured verbatim — matching clap's `trailing_var_arg` semantics.
    let trailing = spec
        .arguments
        .last()
        .is_some_and(|a| matches!(a.kind, ArgumentKind::TrailingVarArgs));
    let fixed_arg_count = spec
        .arguments
        .iter()
        .filter(|a| {
            !matches!(
                a.kind,
                ArgumentKind::TrailingVarArgs | ArgumentKind::VarArgs
            )
        })
        .count();

    let mut greedy = false;
    let mut after_double_dash = false;

    let mut cursor = RawArgCursor::new(args, path);
    while !cursor.done() {
        let arg = cursor.peek().expect("not done").clone();

        if greedy || after_double_dash {
            positionals.push(arg);
            cursor.advance();
            continue;
        }

        if arg == "--" {
            after_double_dash = true;
            cursor.advance();
            continue;
        }

        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let Some(flag_spec) = spec
                .find_flag(name)
                .or_else(|| CommandCatalogue::get().find_global_flag(name))
            else {
                return Err(replaced_or_unknown_flag(
                    path,
                    name,
                    inline,
                    cursor.next_token(),
                ));
            };
            cursor.apply_flag(flag_spec, inline, &mut flags)?;
        } else if let Some(short) = arg.strip_prefix('-') {
            // Only single-character short flags are supported (clap parity).
            if short.chars().count() != 1 {
                return Err(CommandError::unknown_flag(path, arg.clone()));
            }
            let ch = short.chars().next().unwrap();
            let flag_spec = spec
                .flags
                .iter()
                .find(|f| f.short == Some(ch))
                .ok_or_else(|| CommandError::unknown_flag(path, format!("-{ch}")))?;
            cursor.apply_flag(flag_spec, None, &mut flags)?;
        } else {
            positionals.push(arg);
            // Begin greedy capture once we have started filling a trailing
            // var-arg (i.e. once positionals exceed the fixed leading ones).
            if trailing && positionals.len() > fixed_arg_count {
                greedy = true;
            }
            cursor.advance();
        }
    }

    // The clap projection enforces `FlagSpec::conflicts_with` automatically.
    // Raw/API parsing has no clap layer, so enforce the same declarative rule
    // before dispatch can observe a contradictory flag set.
    for flag in spec.flags {
        if !flag_is_active(&flags, flag.long) {
            continue;
        }
        if let Some(conflicting) = flag
            .conflicts_with
            .iter()
            .find(|conflicting| flag_is_active(&flags, conflicting))
        {
            return Err(CommandError::InvalidFlagValue {
                command: path.iter().map(|segment| (*segment).to_string()).collect(),
                flag: flag.long.to_string(),
                reason: format!("--{} conflicts with --{}", flag.long, conflicting),
            });
        }
    }

    // Map collected positionals onto the declared arguments in order.
    let mut arguments: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut path_arguments: BTreeSet<String> = BTreeSet::new();
    let mut pos_idx = 0;
    for arg_spec in spec.arguments {
        match arg_spec.kind {
            ArgumentKind::TrailingVarArgs | ArgumentKind::VarArgs => {
                let collected: Vec<String> = positionals[pos_idx..].to_vec();
                if !collected.is_empty() {
                    arguments.insert(arg_spec.name.to_string(), collected);
                }
                pos_idx = positionals.len();
            }
            ArgumentKind::Path | ArgumentKind::OptionalPath => {
                if let Some(v) = positionals.get(pos_idx) {
                    arguments.insert(arg_spec.name.to_string(), vec![v.clone()]);
                    path_arguments.insert(arg_spec.name.to_string());
                    pos_idx += 1;
                }
            }
            ArgumentKind::String | ArgumentKind::OptionalString => {
                if let Some(v) = positionals.get(pos_idx) {
                    arguments.insert(arg_spec.name.to_string(), vec![v.clone()]);
                    pos_idx += 1;
                }
            }
        }
    }

    // Positionals the command never declared are a usage error, exactly as
    // clap treats them on the CLI. Silently dropping them let a command like
    // `new skill --pull <slug> <name>` — which declares no positional at all —
    // look accepted through the API and TUI while the CLI rejected it.
    if let Some(extra) = positionals.get(pos_idx) {
        return Err(CommandError::unexpected_argument(path, extra.clone()));
    }

    Ok(ParsedArgs {
        flags,
        arguments,
        path_arguments,
    })
}

/// A position in an argument vector, and the command path errors are
/// reported against.
///
/// The three were passed separately to `apply_flag` and `read_value`, which
/// then returned the new index for the caller to assign back — an invitation
/// to forget (WI 0114 F-51). The cursor advances itself.
pub(super) struct RawArgCursor<'a> {
    args: &'a [String],
    /// Index of the next unconsumed token.
    i: usize,
    path: &'a [&'a str],
}

impl<'a> RawArgCursor<'a> {
    fn new(args: &'a [String], path: &'a [&'a str]) -> Self {
        Self { args, i: 0, path }
    }

    fn peek(&self) -> Option<&'a String> {
        self.args.get(self.i)
    }

    /// The token after the current one.
    fn next_token(&self) -> Option<&'a String> {
        self.args.get(self.i + 1)
    }

    fn advance(&mut self) {
        self.i += 1;
    }

    fn done(&self) -> bool {
        self.i >= self.args.len()
    }

    /// Apply a single flag to the flag map, consuming a value when the flag's kind
    /// requires one. Advances past everything it consumed.
    fn apply_flag(
        &mut self,
        flag_spec: &FlagSpec,
        inline: Option<String>,
        flags: &mut BTreeMap<String, ParsedFlag>,
    ) -> Result<(), CommandError> {
        let path = self.path;
        let key = flag_spec.long.to_string();
        match flag_spec.kind {
            FlagKind::Bool => {
                // clap `SetTrue`: presence implies true and no following value is
                // consumed. An inline `--flag=false` is honored for completeness.
                let value = !matches!(inline.as_deref(), Some("false"));
                flags.insert(key, ParsedFlag::Bool(value));
                self.advance();
                Ok(())
            }
            FlagKind::String | FlagKind::OptionalString => {
                let val = self.read_value(inline, flag_spec.long)?;
                flags.insert(key, ParsedFlag::Str(val));
                Ok(())
            }
            FlagKind::Enum(allowed) => {
                let val = self.read_value(inline, flag_spec.long)?;
                if !allowed.contains(&val.as_str()) {
                    return Err(CommandError::InvalidFlagValue {
                        command: path.iter().map(|s| s.to_string()).collect(),
                        flag: flag_spec.long.to_string(),
                        reason: format!("'{val}' is not one of {allowed:?}"),
                    });
                }
                flags.insert(key, ParsedFlag::Str(val));
                Ok(())
            }
            FlagKind::VecString => {
                let val = self.read_value(inline, flag_spec.long)?;
                match flags.get_mut(&key) {
                    Some(ParsedFlag::Strings(items)) => items.push(val),
                    _ => {
                        flags.insert(key, ParsedFlag::Strings(vec![val]));
                    }
                }
                Ok(())
            }
            FlagKind::Path | FlagKind::OptionalPath => {
                let val = self.read_value(inline, flag_spec.long)?;
                flags.insert(key, ParsedFlag::Path(PathBuf::from(val)));
                Ok(())
            }
            FlagKind::U16 => {
                let val = self.read_value(inline, flag_spec.long)?;
                let n = val
                    .parse::<u16>()
                    .map_err(|_| CommandError::InvalidFlagValue {
                        command: path.iter().map(|s| s.to_string()).collect(),
                        flag: flag_spec.long.to_string(),
                        reason: format!("'{val}' is not a valid integer (0..=65535)"),
                    })?;
                flags.insert(key, ParsedFlag::U16(n));
                Ok(())
            }
            FlagKind::UsizeAtLeastOne => {
                let val = self.read_value(inline, flag_spec.long)?;
                let n = val
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| CommandError::InvalidFlagValue {
                        command: path.iter().map(|s| s.to_string()).collect(),
                        flag: flag_spec.long.to_string(),
                        reason: format!("'{val}' is not a positive integer (>= 1)"),
                    })?;
                flags.insert(key, ParsedFlag::Usize(n));
                Ok(())
            }
        }
    }

    /// Read a value for a value-taking flag: the inline `--flag=value` form
    /// if present, otherwise the following token. A following token that looks
    /// like a flag (or a missing token) is a structured error, matching clap's
    /// refusal to consume a `-`-prefixed token as a value by default.
    fn read_value(&mut self, inline: Option<String>, flag: &str) -> Result<String, CommandError> {
        if let Some(v) = inline {
            self.advance();
            return Ok(v);
        }
        match self.args.get(self.i + 1) {
            Some(v) if !v.starts_with('-') => {
                let v = v.clone();
                self.advance();
                self.advance();
                Ok(v)
            }
            _ => Err(CommandError::InvalidFlagValue {
                command: self.path.iter().map(|s| s.to_string()).collect(),
                flag: flag.to_string(),
                reason: "flag requires a value".to_string(),
            }),
        }
    }
}

/// The error for an undeclared long flag: a [replaced flag]'s migration
/// message when it is one, otherwise [`CommandError::UnknownFlag`]. The value
/// is the inline `--flag=value`, or the next token unless it is a flag.
///
/// [replaced flag]: crate::command::dispatch::catalogue::ReplacedFlagSpec
fn replaced_or_unknown_flag(
    path: &[&str],
    name: &str,
    inline: Option<String>,
    next: Option<&String>,
) -> CommandError {
    let Some(replaced) = CommandCatalogue::get().replaced_flag(name) else {
        return CommandError::unknown_flag(path, name);
    };
    let value = inline.or_else(|| next.filter(|token| !token.starts_with('-')).cloned());
    CommandError::InvalidFlagValue {
        command: path.iter().map(|segment| (*segment).to_string()).collect(),
        flag: replaced.name.to_string(),
        reason: (replaced.message)(value.as_deref()),
    }
}
