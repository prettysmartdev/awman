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
/// `non-interactive` is FORCED: HTTP workers have no TTY, and on Apple's
/// `container` CLI a PTY request with piped stdin fails with `ENOTTY`, so this
/// is a technical requirement, not a preference.
///
/// `yolo` is an overridable DEFAULT: it stays `true` for backward compatibility
/// (every historical API caller ran with auto-approval), but a request payload
/// that passes `--yolo`/`--plan` etc. can now change it. The policy lives here,
/// next to the profile, instead of being an unadvertised side effect buried in
/// the API frontend. See work item 0097, Finding D.
const API_FLAG_DEFAULTS: &[FrontendFlagDefault] = &[
    FrontendFlagDefault {
        flag: "non-interactive",
        value: FlagDefault::Bool(true),
        forced: true,
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
        // The squad daemon runs unattended for the same reasons the API
        // server does, and applies the same profile.
        FrontendKind::Api | FrontendKind::SquadDaemon => API_FLAG_DEFAULTS,
        // CLI and TUI resolve flag defaults through clap / the command layer;
        // no profile overrides apply, guaranteeing this mechanism cannot alter
        // their behavior.
        FrontendKind::Cli | FrontendKind::Tui => &[],
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
        let conflict_present = flag_spec
            .conflicts_with
            .iter()
            .any(|other| flag_is_active(&parsed.flags, other));
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
        apply_frontend_defaults(&mut parsed, spec, frontend);
        Ok(parsed)
    }

    /// The flags a frontend profile forces or defaults for `path`, as
    /// `(flag name, value)` pairs — the profile's own account of itself.
    ///
    /// A frontend that reports its profile to a client reads it here rather
    /// than restating it: `api/routes.rs` used to answer `POST /v1/commands`
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

fn parse_against_spec(
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
        .filter(|a| !matches!(a.kind, ArgumentKind::TrailingVarArgs))
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
            let flag_spec = spec
                .find_flag(name)
                .ok_or_else(|| CommandError::unknown_flag(path, name))?;
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
            ArgumentKind::TrailingVarArgs => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> &'static CommandCatalogue {
        CommandCatalogue::get()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exec_prompt_joins_trailing_positionals() {
        let p = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["hello", "brave", "world"]))
            .unwrap();
        assert_eq!(p.argument("prompt").as_deref(), Some("hello brave world"));
        assert_eq!(p.arguments("prompt"), vec!["hello", "brave", "world"]);
    }

    #[test]
    fn exec_workflow_path_positional_reachable_via_flag_path_and_argument() {
        let p = cat()
            .parse_raw_args(&["exec", "workflow"], &argv(&["build.toml"]))
            .unwrap();
        assert_eq!(p.argument("workflow").as_deref(), Some("build.toml"));
        assert_eq!(p.flag_path("workflow"), Some(PathBuf::from("build.toml")));
    }

    /// The API / TUI command box parse `--agent-flag=<value>` exactly as clap
    /// does, hyphen-leading values included.
    #[test]
    fn agent_flag_inline_values_may_start_with_a_hyphen() {
        let p = cat()
            .parse_raw_args(
                &["chat"],
                &argv(&["--agent-flag=--no-daemon", "--agent-flag=-v"]),
            )
            .unwrap();
        assert_eq!(p.flag_strings("agent-flag"), vec!["--no-daemon", "-v"]);
    }

    /// Clap rejects positionals a command never declared; the raw-args
    /// projection must agree (WI-0097 parity) rather than silently dropping
    /// them. `exec workflow` declares exactly one positional.
    #[test]
    fn extra_positional_beyond_declared_arguments_is_rejected() {
        let err = cat()
            .parse_raw_args(&["exec", "workflow"], &argv(&["build.toml", "stray"]))
            .expect_err("an undeclared extra positional must be a usage error");
        match err {
            CommandError::UnexpectedArgument { command, argument } => {
                assert_eq!(command, vec!["exec", "workflow"]);
                assert_eq!(argument, "stray", "the error must name the stray token");
            }
            other => panic!("expected UnexpectedArgument, got {other:?}"),
        }
    }

    /// `new skill` declares no positional at all, so a stray skill name next to
    /// `--pull` must be rejected before any git work can start (WI-0103).
    #[test]
    fn new_skill_rejects_a_positional_name_alongside_pull() {
        let err = cat()
            .parse_raw_args(
                &["new", "skill"],
                &argv(&["--pull", "owner/library", "accidental-name"]),
            )
            .expect_err("new skill takes no positional argument");
        assert!(
            matches!(
                &err,
                CommandError::UnexpectedArgument { argument, .. } if argument == "accidental-name"
            ),
            "expected UnexpectedArgument naming the stray name, got {err:?}"
        );
    }

    #[test]
    fn flag_equals_and_space_forms_are_equivalent() {
        let a = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--agent", "claude"]))
            .unwrap();
        let b = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--agent=claude"]))
            .unwrap();
        assert_eq!(a.flag_string("agent").as_deref(), Some("claude"));
        assert_eq!(b.flag_string("agent").as_deref(), Some("claude"));
    }

    #[test]
    fn bool_flag_is_set_true_by_presence() {
        let p = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--plan"]))
            .unwrap();
        assert_eq!(p.flag_bool("plan"), Some(true));
    }

    #[test]
    fn repeated_vec_flag_accumulates() {
        let p = cat()
            .parse_raw_args(
                &["exec", "prompt"],
                &argv(&["--overlay", "/a", "--overlay", "/b"]),
            )
            .unwrap();
        assert_eq!(p.flag_strings("overlay"), vec!["/a", "/b"]);
    }

    #[test]
    fn double_dash_forces_positionals() {
        let p = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--", "--not-a-flag"]))
            .unwrap();
        assert_eq!(p.argument("prompt").as_deref(), Some("--not-a-flag"));
    }

    #[test]
    fn unknown_flag_is_structured_error() {
        let err = cat()
            .parse_raw_args(&["exec", "prompt"], &argv(&["--bogus"]))
            .unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    #[test]
    fn u16_type_mismatch_is_structured_error() {
        let err = cat()
            .parse_raw_args(&["api", "start"], &argv(&["--port", "not-a-number"]))
            .unwrap_err();
        assert!(matches!(err, CommandError::InvalidFlagValue { .. }));
    }

    #[test]
    fn u16_flag_coerces_value() {
        let p = cat()
            .parse_raw_args(&["api", "start"], &argv(&["--port", "9876"]))
            .unwrap();
        assert_eq!(p.flag_u16("port"), Some(9876));
    }

    #[test]
    fn bad_enum_value_is_structured_error() {
        let err = cat()
            .parse_raw_args(
                &["remote", "session", "start"],
                &argv(&["--type", "banana"]),
            )
            .unwrap_err();
        assert!(matches!(err, CommandError::InvalidFlagValue { .. }));
    }

    #[test]
    fn api_profile_forces_non_interactive_and_defaults_yolo() {
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["hi"]), FrontendKind::Api)
            .unwrap();
        assert_eq!(p.flag_bool("non-interactive"), Some(true));
        assert_eq!(p.flag_bool("yolo"), Some(true));
    }

    #[test]
    fn api_profile_yolo_default_is_overridable_by_plan() {
        // A request that opts into --plan (mutually exclusive with yolo in the
        // catalogue) must genuinely override the overridable yolo default: the
        // fills-when-absent default is suppressed because a conflicting flag is
        // present, so yolo stays unset and dispatch sees a coherent request.
        // non-interactive stays forced regardless.
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["--plan"]), FrontendKind::Api)
            .unwrap();
        assert_eq!(p.flag_bool("plan"), Some(true));
        assert_eq!(p.flag_bool("non-interactive"), Some(true));
        // yolo default suppressed by the conflicting --plan (real override).
        assert_eq!(p.flag_bool("yolo"), None);
    }

    #[test]
    fn cli_and_tui_profiles_apply_no_overrides() {
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["hi"]), FrontendKind::Cli)
            .unwrap();
        // No profile override: non-interactive/yolo are absent unless supplied.
        assert_eq!(p.flag_bool("non-interactive"), None);
        assert_eq!(p.flag_bool("yolo"), None);
    }

    #[test]
    fn unknown_command_path_is_structured_error() {
        let err = cat().parse_raw_args(&["nope"], &argv(&[])).unwrap_err();
        assert!(matches!(err, CommandError::UnknownCommand { .. }));
    }

    #[test]
    fn config_set_maps_two_positionals() {
        let p = cat()
            .parse_raw_args(&["config", "set"], &argv(&["agent", "claude"]))
            .unwrap();
        assert_eq!(p.argument("field").as_deref(), Some("agent"));
        assert_eq!(p.argument("value").as_deref(), Some("claude"));
    }

    #[test]
    fn only_parse_stage_errors_can_be_replayed() {
        let error = cat().parse_raw_args(&["nope"], &argv(&[])).unwrap_err();
        let replay = error.clone_parse_stage().expect("a parse error replays");
        assert_eq!(replay.to_string(), error.to_string());
        assert!(CommandError::Aborted.clone_parse_stage().is_none());
    }

    #[test]
    fn explicit_yolo_value_is_kept_under_api_profile() {
        // yolo is an overridable default, so if the request already set it the
        // profile must not clobber it.
        let p = cat()
            .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&["--yolo"]), FrontendKind::Api)
            .unwrap();
        assert_eq!(p.flag_bool("yolo"), Some(true));
    }
}

/// Parsing cases every frontend that dispatches from [`ParsedArgs`] must read
/// back identically. The API and TUI frontends' own tests run this table, so
/// the only allowed differences are the profile rows declared here.
#[cfg(test)]
pub(crate) mod shared_cases {
    use super::*;
    use crate::command::dispatch::CommandFrontend;

    pub(crate) enum Read {
        /// Compared the way `resolved::read_flag` reads it: absent is false.
        Bool(&'static str, bool),
        Str(&'static str, &'static str),
        Strs(&'static str, &'static [&'static str]),
        Path(&'static str, &'static str),
        U16(&'static str, u16),
        Usize(&'static str, usize),
        Arg(&'static str, &'static str),
        Args(&'static str, &'static [&'static str]),
    }

    pub(crate) struct Case {
        pub name: &'static str,
        /// The command as typed, possibly through an alias.
        pub path: &'static [&'static str],
        pub canonical: &'static [&'static str],
        pub args: &'static [&'static str],
        pub reads: &'static [Read],
        /// Reads that differ by caller profile.
        pub tui_reads: &'static [Read],
        pub api_reads: &'static [Read],
    }

    const PROMPT: &[&str] = &["exec", "prompt"];
    const WORKFLOW: &[&str] = &["exec", "workflow"];

    pub(crate) const CASES: &[Case] = &[
        Case {
            name: "omitted flags: false for the TUI, API profile defaults and forces",
            path: PROMPT,
            canonical: PROMPT,
            args: &["hi"],
            reads: &[Read::Bool("plan", false)],
            tui_reads: &[
                Read::Bool("yolo", false),
                Read::Bool("non-interactive", false),
            ],
            api_reads: &[
                Read::Bool("yolo", true),
                Read::Bool("non-interactive", true),
            ],
        },
        Case {
            name: "explicit false beats a default but not a forced flag",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--yolo=false", "--non-interactive=false", "hi"],
            reads: &[Read::Bool("yolo", false)],
            tui_reads: &[Read::Bool("non-interactive", false)],
            api_reads: &[Read::Bool("non-interactive", true)],
        },
        Case {
            name: "a conflicting flag suppresses the API's yolo default",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--plan", "hi"],
            reads: &[Read::Bool("plan", true), Read::Bool("yolo", false)],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "an explicitly false flag does not suppress another flag's default",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--plan=false", "hi"],
            reads: &[Read::Bool("plan", false)],
            tui_reads: &[Read::Bool("yolo", false)],
            api_reads: &[Read::Bool("yolo", true)],
        },
        Case {
            name: "an explicitly false flag keeps its own value beside a false conflict",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--plan=false", "--yolo=false", "hi"],
            reads: &[Read::Bool("plan", false), Read::Bool("yolo", false)],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "a false --plan does not conflict with --yolo",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--plan=false", "--yolo", "hi"],
            reads: &[Read::Bool("plan", false), Read::Bool("yolo", true)],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "a false --yolo does not conflict with --plan",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--yolo=false", "--plan", "hi"],
            reads: &[Read::Bool("plan", true), Read::Bool("yolo", false)],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "a command alias and a short flag resolve to canonical names",
            path: &["exec", "wf"],
            canonical: WORKFLOW,
            args: &["a.toml", "-n"],
            reads: &[
                Read::Bool("non-interactive", true),
                Read::Arg("workflow", "a.toml"),
            ],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "numeric flags at their bounds arrive typed",
            path: WORKFLOW,
            canonical: WORKFLOW,
            args: &["a.toml", "--max-concurrent", "1"],
            reads: &[Read::Usize("max-concurrent", 1)],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "the largest port arrives typed",
            path: &["api", "start"],
            canonical: &["api", "start"],
            args: &["--port=65535"],
            reads: &[Read::U16("port", 65535)],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "a path positional reads as both a path and an argument",
            path: WORKFLOW,
            canonical: WORKFLOW,
            args: &["dir with space/a.toml"],
            reads: &[
                Read::Path("workflow", "dir with space/a.toml"),
                Read::Arg("workflow", "dir with space/a.toml"),
            ],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "trailing arguments keep hyphen-leading tokens",
            path: PROMPT,
            canonical: PROMPT,
            args: &["hello", "--not-a-flag", "-x"],
            reads: &[
                Read::Arg("prompt", "hello --not-a-flag -x"),
                Read::Args("prompt", &["hello", "--not-a-flag", "-x"]),
            ],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "a double dash makes the rest positional",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--", "--plan"],
            reads: &[Read::Bool("plan", false), Read::Arg("prompt", "--plan")],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "repeated flags accumulate in order",
            path: PROMPT,
            canonical: PROMPT,
            args: &["--overlay", "/b", "--overlay=/a"],
            reads: &[Read::Strs("overlay", &["/b", "/a"])],
            tui_reads: &[],
            api_reads: &[],
        },
        Case {
            name: "hyphen-leading agent args pass through their inline form",
            path: &["chat"],
            canonical: &["chat"],
            args: &[
                "--agent",
                "claude",
                "--agent-flag=--no-daemon",
                "--agent-flag=-v",
            ],
            reads: &[
                Read::Str("agent", "claude"),
                Read::Strs("agent-flag", &["--no-daemon", "-v"]),
            ],
            tui_reads: &[],
            api_reads: &[],
        },
    ];

    /// Rejected input and the classification it must keep in every frontend.
    pub(crate) const ERROR_CASES: &[(&[&str], &[&str], &str)] = &[
        (PROMPT, &["--bogus"], "UnknownFlag"),
        (PROMPT, &["-ab"], "UnknownFlag"),
        (&["nope"], &[], "UnknownCommand"),
        (WORKFLOW, &["a.toml", "stray"], "UnexpectedArgument"),
        (&["api", "start"], &["--port=65536"], "InvalidFlagValue"),
        (&["api", "start"], &["--port=abc"], "InvalidFlagValue"),
        (WORKFLOW, &["--max-concurrent=0"], "InvalidFlagValue"),
        (&["chat"], &["--launch-mode=banana"], "InvalidFlagValue"),
        (&["chat"], &["--agent-flag", "-v"], "InvalidFlagValue"),
        (PROMPT, &["--plan", "--yolo"], "InvalidFlagValue"),
    ];

    pub(crate) fn classification(error: &CommandError) -> &'static str {
        match error {
            CommandError::UnknownCommand { .. } => "UnknownCommand",
            CommandError::UnknownFlag { .. } => "UnknownFlag",
            CommandError::InvalidFlagValue { .. } => "InvalidFlagValue",
            CommandError::UnexpectedArgument { .. } => "UnexpectedArgument",
            other => panic!("not a parse-stage classification: {other:?}"),
        }
    }

    /// Assert `reads` against any frontend, through the dispatch accessors.
    pub(crate) fn assert_reads<F: CommandFrontend>(frontend: &F, case: &Case, reads: &[Read]) {
        let path = case.canonical;
        let name = case.name;
        for read in reads {
            match *read {
                Read::Bool(flag, want) => assert_eq!(
                    frontend.flag_bool(path, flag).unwrap().unwrap_or(false),
                    want,
                    "{name}: --{flag}"
                ),
                Read::Str(flag, want) => assert_eq!(
                    frontend.flag_string(path, flag).unwrap().as_deref(),
                    Some(want),
                    "{name}: --{flag}"
                ),
                Read::Strs(flag, want) => assert_eq!(
                    frontend.flag_strings(path, flag).unwrap(),
                    want,
                    "{name}: --{flag}"
                ),
                Read::Path(flag, want) => assert_eq!(
                    frontend.flag_path(path, flag).unwrap(),
                    Some(PathBuf::from(want)),
                    "{name}: --{flag}"
                ),
                Read::U16(flag, want) => assert_eq!(
                    frontend.flag_u16(path, flag).unwrap(),
                    Some(want),
                    "{name}: --{flag}"
                ),
                Read::Usize(flag, want) => assert_eq!(
                    frontend.flag_usize(path, flag).unwrap(),
                    Some(want),
                    "{name}: --{flag}"
                ),
                Read::Arg(arg, want) => assert_eq!(
                    frontend.argument(path, arg).unwrap().as_deref(),
                    Some(want),
                    "{name}: {arg}"
                ),
                Read::Args(arg, want) => assert_eq!(
                    frontend.arguments(path, arg).unwrap(),
                    want,
                    "{name}: {arg}"
                ),
            }
        }
    }
}
