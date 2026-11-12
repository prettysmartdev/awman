//! `CommandCatalogue` — the canonical, single-source-of-truth enumeration of
//! every awman command, subcommand, argument, and flag.
//!
//! Frontends never hard-code command names or flag names; they ask the
//! catalogue (or its projections) for what's available. The catalogue MUST
//! enumerate every command currently exposed by the command layer.

use std::sync::OnceLock;

/// Visibility of a command/flag across frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendVisibility {
    /// Visible to every frontend (CLI, TUI, API).
    All,
    /// CLI-only (e.g. API server start).
    CliOnly,
    /// TUI-only (e.g. tab annotations).
    TuiOnly,
    /// CLI + TUI (e.g. interactive Q&A toggles).
    CliAndTui,
    /// Hidden (no frontend exposes it).
    Hidden,
}

/// The kind of value a flag accepts.
#[derive(Debug, Clone, Copy)]
pub enum FlagKind {
    /// `--foo` (presence-only).
    Bool,
    /// `--foo NAME` required string.
    String,
    /// `--foo NAME` optional string.
    OptionalString,
    /// `--foo NAME` from a fixed set of values.
    Enum(&'static [&'static str]),
    /// Repeatable string flag (`--foo a --foo b`).
    VecString,
    /// `--foo PATH` optional path.
    Path,
    /// `--foo PATH` optional path.
    OptionalPath,
    /// `--foo N` u16 number.
    U16,
    /// `--foo N` usize number, must be >= 1.
    UsizeAtLeastOne,
}

/// Default value for a flag.
#[derive(Debug, Clone, Copy)]
pub enum FlagDefault {
    None,
    Bool(bool),
    Str(&'static str),
    U16(u16),
    EmptyVec,
}

/// Spec for a single named flag.
#[derive(Debug, Clone, Copy)]
pub struct FlagSpec {
    pub long: &'static str,
    pub short: Option<char>,
    pub help: &'static str,
    pub kind: FlagKind,
    pub default: FlagDefault,
    pub frontends: FrontendVisibility,
    /// Other flags this flag is mutually exclusive with.
    pub conflicts_with: &'static [&'static str],
    /// Other flags this flag implies (sets to true / forwards value).
    pub implies: &'static [&'static str],
    /// `false` = required; `true` = optional.
    pub optional: bool,
}

impl FlagSpec {
    pub fn conflicts_with(&self, other: &str) -> bool {
        self.conflicts_with.contains(&other)
    }
}

/// Spec for a flag that once existed but has since been removed. Frontends
/// scan raw argv for these *before* clap parses, so a user who passes a
/// retired flag sees a migration hint instead of clap's generic
/// "unexpected argument" error. Keeping the retired-flag knowledge here — next
/// to the live [`FlagSpec`]s — means future removals never touch `main.rs`.
#[derive(Debug, Clone, Copy)]
pub struct RemovedFlagSpec {
    /// The retired long flag, leading dashes included (e.g. `--mount-ssh`).
    /// Matches both the bare form and the `--flag=value` form.
    pub name: &'static str,
    /// Migration guidance appended after "`<name>` has been removed.".
    pub hint: &'static str,
}

/// The kind of an argument (positional value).
#[derive(Debug, Clone, Copy)]
pub enum ArgumentKind {
    String,
    OptionalString,
    Path,
    OptionalPath,
    /// `<COMMAND>...` style: collect every remaining token verbatim,
    /// including hyphen-prefixed values, into a single argument.
    TrailingVarArgs,
    /// Multiple positionals; flags can appear between or after values.
    VarArgs,
}

#[derive(Debug, Clone, Copy)]
pub struct ArgumentSpec {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: ArgumentKind,
    pub optional: bool,
}

/// Which frontend kinds are allowed to invoke a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendKind {
    Cli,
    Tui,
    /// A CLI or TUI whose commands run in the daemon's `engine` (WI 0142
    /// D1): the person is at the relaying frontend, so it answers like that
    /// frontend (a local user, a human to ask, its flag visibility). Which
    /// commands may be relayed is each spec's
    /// [`relay_allowed`](CommandSpec::relay_allowed).
    Relayed {
        origin: RelayOrigin,
    },
    Daemon {
        feature: crate::data::daemon_state::FeatureName,
        listener: crate::data::daemon_state::CommandListener,
    },
}

/// Which frontend relays a [`FrontendKind::Relayed`] command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayOrigin {
    Tui,
    Cli,
}

impl FrontendKind {
    /// A daemon-backed TUI (its command box runs in the engine).
    pub const RELAYED_TUI: Self = Self::Relayed {
        origin: RelayOrigin::Tui,
    };
    /// A daemon-backed CLI invocation (`relay_allowed` commands only).
    pub const RELAYED_CLI: Self = Self::Relayed {
        origin: RelayOrigin::Cli,
    };

    /// The relaying frontend of a [`Self::Relayed`] kind.
    pub fn origin(self) -> Option<RelayOrigin> {
        match self {
            FrontendKind::Relayed { origin } => Some(origin),
            FrontendKind::Cli | FrontendKind::Tui | FrontendKind::Daemon { .. } => None,
        }
    }

    /// The frontend a relayed kind answers like (`Cli`/`Tui` themselves
    /// otherwise; `None` for the daemon's own frontends).
    pub fn presenting(self) -> Option<FrontendKind> {
        match self {
            FrontendKind::Cli
            | FrontendKind::Relayed {
                origin: RelayOrigin::Cli,
            } => Some(FrontendKind::Cli),
            FrontendKind::Tui
            | FrontendKind::Relayed {
                origin: RelayOrigin::Tui,
            } => Some(FrontendKind::Tui),
            FrontendKind::Daemon { .. } => None,
        }
    }

    pub const WORKER_LOCAL: Self = Self::Daemon {
        feature: crate::data::daemon_state::FeatureName::Worker,
        listener: crate::data::daemon_state::CommandListener::Local,
    };
    pub const SQUAD_LOCAL: Self = Self::Daemon {
        feature: crate::data::daemon_state::FeatureName::Squad,
        listener: crate::data::daemon_state::CommandListener::Local,
    };
    /// Commands an `engine` instance runs for its frontends (WI 0142 Part A
    /// §1). The instance runs with the launching frontend's environment, cwd
    /// and argv, and a person is at its frontends, so it answers like the
    /// TUI: a local user with a human to ask, the same command set and flag
    /// visibility (parity, GA Tenet 2). Only the local listener serves it.
    pub const ENGINE_LOCAL: Self = Self::Daemon {
        feature: crate::data::daemon_state::FeatureName::Engine,
        listener: crate::data::daemon_state::CommandListener::Local,
    };

    /// Whether this is an engine instance serving a local frontend
    /// ([`Self::ENGINE_LOCAL`]).
    pub fn is_engine_local(self) -> bool {
        self == Self::ENGINE_LOCAL
    }
    /// Whether this frontend is the user's own session on the host that chose
    /// the paths in a request.
    ///
    /// The CLI runs in the user's shell and the TUI in their terminal, so in
    /// both the process's current directory is theirs and a human is there to
    /// answer. Neither HTTP frontend is: each re-executes a request a client
    /// already authorised, from a working directory unrelated to the
    /// caller's.
    pub fn is_local_user(self) -> bool {
        match self {
            FrontendKind::Cli | FrontendKind::Tui | FrontendKind::Relayed { .. } => true,
            FrontendKind::Daemon { .. } if self.is_engine_local() => true,
            FrontendKind::Daemon { .. } => false,
        }
    }

    /// Whether a human can be asked a question through this frontend.
    ///
    /// The mirror of [`is_local_user`](Self::is_local_user) for the HTTP
    /// frontends, and the constant half of the headless rule: the CLI's
    /// answer additionally depends on whether stdin is a terminal, which only
    /// the CLI can see, so it supplies that through
    /// [`CommandFrontend::input_available`](crate::command::dispatch::CommandFrontend::input_available).
    pub fn can_ask_a_human(self) -> bool {
        match self {
            FrontendKind::Cli | FrontendKind::Tui | FrontendKind::Relayed { .. } => true,
            FrontendKind::Daemon { .. } if self.is_engine_local() => true,
            FrontendKind::Daemon { .. } => false,
        }
    }

    /// The name this frontend is reported by in user-facing errors.
    pub fn label(self) -> &'static str {
        match self {
            FrontendKind::Cli => "cli",
            FrontendKind::Tui => "tui",
            FrontendKind::Relayed {
                origin: RelayOrigin::Tui,
            } => "tui:daemon",
            FrontendKind::Relayed {
                origin: RelayOrigin::Cli,
            } => "cli:daemon",
            FrontendKind::Daemon { feature, listener } => match (feature, listener) {
                (
                    crate::data::daemon_state::FeatureName::Worker,
                    crate::data::daemon_state::CommandListener::Local,
                ) => "daemon:worker:local",
                (
                    crate::data::daemon_state::FeatureName::Worker,
                    crate::data::daemon_state::CommandListener::Network,
                ) => "daemon:worker:network",
                (
                    crate::data::daemon_state::FeatureName::Squad,
                    crate::data::daemon_state::CommandListener::Local,
                ) => "daemon:squad:local",
                (
                    crate::data::daemon_state::FeatureName::Squad,
                    crate::data::daemon_state::CommandListener::Network,
                ) => "daemon:squad:network",
                (
                    crate::data::daemon_state::FeatureName::Engine,
                    crate::data::daemon_state::CommandListener::Local,
                ) => "daemon:engine:local",
                (
                    crate::data::daemon_state::FeatureName::Engine,
                    crate::data::daemon_state::CommandListener::Network,
                ) => "daemon:engine:network",
            },
        }
    }
}

/// Whether a command needs a squad daemon gateway before it can be built,
/// and how hard dispatch should try to get one.
///
/// This is the catalogue's answer to a question two frontends used to answer
/// for themselves with hard-coded name lists (WI 0113 F-04). Dispatch reads
/// it; no frontend does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayNeed {
    /// The command never speaks to a squad daemon.
    None,
    /// The command cannot run without one: start a daemon if none is running,
    /// and refuse when this process holds no key for it.
    Running,
    /// The command reports on a daemon if one is running and answers "not
    /// running" otherwise. Starts nothing and mints no key.
    IfRunning,
}

/// Spec for one command (or subcommand) in the catalogue.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    /// Aliases (string only, e.g. `"wf"` for `exec workflow`).
    pub aliases: &'static [&'static str],
    pub help: &'static str,
    pub long_help: Option<&'static str>,
    pub arguments: &'static [ArgumentSpec],
    pub flags: &'static [FlagSpec],
    pub subcommands: &'static [&'static CommandSpec],
    /// Whether this command can be invoked via the API frontend.
    ///
    /// Interactive/PTY commands are deliberately excluded as long-term
    /// policy: an HTTP request cannot safely own their terminal lifecycle.
    /// `squad attach` is therefore `false` even though its non-presentation
    /// flow is implemented in Layer 2 and shared by CLI and TUI.
    pub api_allowed: bool,

    /// How `Dispatch` constructs this command (WI 0113 F-10).
    ///
    /// The catalogue owns the constructor the same way it owns the flags and
    /// their defaults: `Dispatch::build_command` resolves the flags, looks the
    /// spec up and calls this. A spec that is not itself runnable — the root,
    /// a grouping parent such as `exec`, or a command still awaiting its
    /// Layer 2 implementation — registers
    /// [`build::unsupported`](crate::command::dispatch::build::unsupported).
    pub build: crate::command::dispatch::build::CommandBuilder,

    /// Whether dispatch must hold a squad gateway before this command is
    /// built. `None` for everything outside the squad subtree.
    pub gateway_need: GatewayNeed,
    /// Whether this command needs a container-class agent runtime. The squad
    /// subtree does: a sandbox-tier runtime cannot mount task directories or
    /// run workflow setup/teardown steps, so every squad entry point must
    /// fail fast with the shared refusal rather than start work it cannot
    /// finish.
    pub requires_container_tier: bool,

    /// Whether this command needs a successfully-detected agent runtime.
    ///
    /// `config` is the recovery path when `GlobalConfig::runtime` names a
    /// runtime that cannot be constructed on this host (e.g.
    /// `apple-containers` on Linux): it only reads and writes config files, so
    /// it must stay reachable to let the user switch the runtime back. Every
    /// other command conservatively requires one.
    ///
    /// An attribute rather than a path test in
    /// [`CommandCatalogue::requires_runtime`], which used to read
    /// `!matches!(path.first(), Some(&"config"))` — a command-name fact spelled
    /// outside the catalogue, so renaming `config` would have silently made
    /// the recovery path unreachable (F-25 step 4).
    pub requires_runtime: bool,

    /// Whether a bare invocation of this command opens the TUI rather than
    /// running as a one-shot CLI command.
    ///
    /// The frontend still supplies the two facts about *this invocation* that
    /// the catalogue cannot know — whether stdin is a terminal, and whether
    /// the user asked for headless output. This is only "does this
    /// command have a TUI form at all", which is a catalogue fact: renaming
    /// `squad` must not leave a stale literal in a frontend (midpoint
    /// finding 24).
    pub opens_tui_when_bare: bool,

    /// An internal entry point (`view`): parsed and runnable like any other
    /// command, but absent from `--help`, the generated reference and TUI
    /// hints (WI 0142 D10).
    pub internal: bool,

    /// Whether a daemon-backed CLI relays this command to the engine
    /// (`FrontendKind::Relayed { origin: Cli }`, WI 0142 D10). Every spec
    /// declares it; `false` means the command runs locally even when the
    /// engine is the backend, and so never triggers the engine hand-off.
    /// Engine-free commands (`config …`, `--help`, `--version`) are always
    /// `false`.
    pub relay_allowed: bool,
}

impl CommandSpec {
    pub fn find_subcommand(&self, name: &str) -> Option<&'static CommandSpec> {
        for sub in self.subcommands {
            if sub.name == name || sub.aliases.contains(&name) {
                return Some(*sub);
            }
        }
        None
    }

    pub fn find_flag(&self, name: &str) -> Option<&'static FlagSpec> {
        self.flags.iter().find(|f| f.long == name)
    }
}

// ─── Top-level catalogue ─────────────────────────────────────────────────────

pub struct CommandCatalogue {
    root: &'static CommandSpec,
    /// Path aliases: pairs of (alias_path, canonical_path). When the user
    /// invokes `alias_path`, dispatch resolves `canonical_path` instead.
    path_aliases: &'static [(&'static [&'static str], &'static [&'static str])],
}

static CATALOGUE: OnceLock<CommandCatalogue> = OnceLock::new();

impl CommandCatalogue {
    /// Borrow the lazily-built singleton.
    pub fn get() -> &'static CommandCatalogue {
        CATALOGUE.get_or_init(|| CommandCatalogue {
            root: &ROOT,
            path_aliases: PATH_ALIASES,
        })
    }

    /// Whether a bare invocation of `path` opens the TUI rather than running
    /// as a one-shot CLI command — the
    /// [`CommandSpec::opens_tui_when_bare`] attribute of the spec this path
    /// resolves to.
    ///
    /// The caller still supplies the two *facts about this invocation* that
    /// the catalogue cannot know: whether stdin is a terminal, and whether the
    /// user asked for headless output.
    pub fn opens_tui_when_bare(&self, path: &[&str]) -> bool {
        self.lookup_with_aliases(path)
            .map(|spec| spec.opens_tui_when_bare)
            .unwrap_or(false)
    }

    pub fn root(&self) -> &'static CommandSpec {
        self.root
    }

    /// The flags every command accepts (`--set`). See [`GLOBAL_FLAGS`].
    pub fn global_flags(&self) -> &'static [FlagSpec] {
        GLOBAL_FLAGS
    }

    /// A global flag by long name.
    pub fn find_global_flag(&self, name: &str) -> Option<&'static FlagSpec> {
        GLOBAL_FLAGS.iter().find(|f| f.long == name)
    }

    pub fn path_aliases(&self) -> &'static [(&'static [&'static str], &'static [&'static str])] {
        self.path_aliases
    }

    /// Walk a path of names, returning the matching `CommandSpec` if any.
    pub fn lookup(&self, path: &[&str]) -> Option<&'static CommandSpec> {
        let mut current = self.root;
        for segment in path {
            current = current.find_subcommand(segment)?;
        }
        Some(current)
    }

    /// Command paths a user who typed `path` may have meant, nearest first.
    ///
    /// Only the *last* segment is corrected, against the subcommands of
    /// whatever prefix did resolve: `awman exec wrkflow` suggests
    /// `exec workflow`, not the top-level commands nearest to `wrkflow`. Each
    /// suggestion is returned as a full, space-joined path so a frontend can
    /// print it verbatim.
    ///
    /// Returns nothing for an empty path, and nothing when no candidate is
    /// within three edits — an unrecognisable command is better reported as
    /// unknown than corrected to something unrelated.
    ///
    /// Both frontends used to own a copy of this, over the top-level command
    /// list only and with two different thresholds (WI 0114 F-43).
    pub fn suggest(&self, path: &[&str]) -> Vec<String> {
        const MAX_EDITS: usize = 3;

        let Some((last, prefix)) = path.split_last() else {
            return Vec::new();
        };
        // Correct the deepest segment that resolves; an unknown segment
        // partway through the path leaves nothing sensible to search under.
        let Some(parent) = self.lookup(prefix) else {
            return Vec::new();
        };
        let candidates: Vec<&str> = parent.subcommands.iter().map(|sub| sub.name).collect();
        crate::data::text::nearest(last, &candidates, MAX_EDITS)
            .into_iter()
            .map(|name| {
                prefix
                    .iter()
                    .copied()
                    .chain(std::iter::once(name))
                    .collect::<Vec<&str>>()
                    .join(" ")
            })
            .collect()
    }

    /// Returns `true` if the given command path is allowed for the given
    /// frontend kind. Session management routes are always allowed; only
    /// command execution routes are restricted.
    pub fn is_allowed_for_frontend(&self, frontend: FrontendKind, path: &[&str]) -> bool {
        match frontend {
            FrontendKind::Cli | FrontendKind::Tui => true,
            // The TUI's command box runs every command the builtin TUI runs;
            // a relayed CLI only what its spec declares relayable.
            FrontendKind::Relayed {
                origin: RelayOrigin::Tui,
            } => true,
            FrontendKind::Relayed {
                origin: RelayOrigin::Cli,
            } => self.is_relay_allowed(path),
            FrontendKind::Daemon {
                feature: crate::data::daemon_state::FeatureName::Worker,
                ..
            } => self.is_api_allowed(path),
            FrontendKind::Daemon {
                feature: crate::data::daemon_state::FeatureName::Squad,
                ..
            } => path.first() == Some(&"squad") && self.is_api_allowed(path),
            FrontendKind::Daemon {
                feature: crate::data::daemon_state::FeatureName::Engine,
                ..
            } => frontend.is_engine_local(),
        }
    }

    /// Whether a daemon-backed CLI relays `path` to the engine (and so
    /// whether the CLI runs the engine hand-off for it at all). Unknown paths
    /// and the bare root are local-only.
    pub fn is_relay_allowed(&self, path: &[&str]) -> bool {
        !path.is_empty()
            && self
                .lookup_with_aliases(path)
                .is_some_and(|spec| spec.relay_allowed)
    }

    /// Whether `path` is an internal entry point (hidden from help, docs
    /// and hints).
    pub fn is_internal(&self, path: &[&str]) -> bool {
        self.lookup_with_aliases(path)
            .is_some_and(|spec| spec.internal)
    }

    fn is_api_allowed(&self, path: &[&str]) -> bool {
        let canonical = self.canonical_path(path);
        match self.lookup(&canonical) {
            Some(spec) => spec.api_allowed,
            None => false,
        }
    }

    /// Same as `lookup`, but first applies any registered path alias rewrites.
    pub fn lookup_with_aliases(&self, path: &[&str]) -> Option<&'static CommandSpec> {
        let canonical = self.canonical_path(path);
        self.lookup(&canonical)
    }

    /// Whether a command path needs a successfully-detected agent runtime to
    /// run — the [`CommandSpec::requires_runtime`] attribute of the spec this
    /// path resolves to.
    ///
    /// A path with no spec (an unknown command, which dispatch will reject
    /// anyway) and the bare-TUI invocation both conservatively require one.
    pub fn requires_runtime(&self, path: &[&str]) -> bool {
        self.lookup_with_aliases(path)
            .map(|spec| spec.requires_runtime)
            .unwrap_or(true)
    }

    /// Scan a raw argv for any [removed flag](RemovedFlagSpec). Returns the
    /// composed migration message ("`<flag>` has been removed. <hint>") for the
    /// first removed flag found, or `None` when argv contains none. Frontends
    /// call this before clap parsing so a retired flag surfaces the hint
    /// instead of clap's generic "unexpected argument" error. Both the bare
    /// `--flag` and the `--flag=value` forms are matched.
    pub fn removed_flag_hint<I, S>(&self, args: I) -> Option<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for arg in args {
            let arg = arg.as_ref();
            for spec in REMOVED_FLAGS {
                if arg == spec.name || arg.starts_with(&format!("{}=", spec.name)) {
                    return Some(format!("{} has been removed. {}", spec.name, spec.hint));
                }
            }
        }
        None
    }

    /// Validate that a command path is reachable by the given frontend,
    /// returning `Err(CommandError::NotAvailableForFrontend)` when blocked.
    pub fn validate_for_frontend(
        &self,
        frontend: FrontendKind,
        path: &[&str],
    ) -> Result<(), crate::command::error::CommandError> {
        if self.is_allowed_for_frontend(frontend, path) {
            Ok(())
        } else {
            let command = path.join(" ");
            let frontend_name = frontend.label();
            Err(
                crate::command::error::CommandError::NotAvailableForFrontend {
                    command,
                    frontend: frontend_name.to_string(),
                },
            )
        }
    }

    /// Return all command paths where `api_allowed == true` as
    /// (parent_name, subcommand_name) pairs. Only immediate (leaf)
    /// api-allowed specs are returned; the root is never included.
    pub fn api_allowed_commands(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        self.collect_api_allowed_rec(self.root, &mut out);
        out
    }

    fn collect_api_allowed_rec(
        &self,
        node: &'static CommandSpec,
        out: &mut Vec<(&'static str, &'static str)>,
    ) {
        for sub in node.subcommands {
            if sub.api_allowed {
                out.push((node.name, sub.name));
            }
            self.collect_api_allowed_rec(sub, out);
        }
    }

    /// Apply path-alias rewrites to a user-supplied path. Returns the
    /// canonical path or the input path unchanged.
    pub fn canonical_path(&self, path: &[&str]) -> Vec<&'static str> {
        // First check registered aliases.
        for (alias, canonical) in self.path_aliases {
            if alias.len() == path.len() && alias.iter().zip(path).all(|(a, b)| *a == *b) {
                return canonical.to_vec();
            }
        }
        // Otherwise the path is canonical; we still need 'static strings.
        // Look up each segment against the catalogue and use the catalogue's
        // 'static reference for the matched subcommand name.
        let mut current = self.root;
        let mut out: Vec<&'static str> = Vec::with_capacity(path.len());
        for segment in path {
            match current.find_subcommand(segment) {
                Some(sub) => {
                    out.push(sub.name);
                    current = sub;
                }
                None => {
                    // Unknown segment — append it verbatim so the caller can
                    // surface an UnknownCommand error that names the bad token.
                    out.push(Box::leak(segment.to_string().into_boxed_str()));
                    return out;
                }
            }
        }
        out
    }
}

// ─── Static catalogue data ───────────────────────────────────────────────────

const ROOT: CommandSpec = CommandSpec {
    name: "awman",
    aliases: &[],
    help: "awman — containerized code agent manager",
    long_help: None,
    arguments: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    flags: &[
        FlagSpec {
            long: "build",
            short: None,
            help: "Force rebuild of images on startup",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "no-cache",
            short: None,
            help: "Disable Docker layer cache during builds",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "refresh",
            short: None,
            help: "Refresh agent environment (run audit)",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[
        &ATTACH, &INIT, &READY, &CHAT, &SPECS, &AGENTS, &CONFIG, &EXEC, &DAEMON, &SQUAD, &REMOTE,
        &NEW, &CLEAN, &VIEW, &LINK, &QUEUE,
    ],
};

/// Flags every command accepts, wherever they appear before `--`.
///
/// Declared once here rather than on every spec: clap marks them
/// `global(true)`, the raw-args parser (TUI command box, API `args`) accepts
/// them on any command, and none of them reaches a command's
/// [`ResolvedFlags`](crate::command::dispatch::ResolvedFlags): `main` applies
/// `--set` to the session at startup, and dispatch applies a command box's or
/// request's `--set` to that one command's session.
const GLOBAL_FLAGS: &[FlagSpec] = &[
    FlagSpec {
        long: SET_FLAG,
        short: None,
        help: "Override a config key for this invocation: --set key=value (repeatable). Beats \
           config files and AWMAN_* variables. `awman config show` lists the keys.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "remote",
        short: None,
        help: "Named remote target from remote.targets.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

/// The long name of the global `--set` flag.
pub const SET_FLAG: &str = "set";

const PATH_ALIASES: &[(&[&str], &[&str])] = &[];

/// The `--type` flag's accepted values: every [`SessionKind`] spelled the way
/// it serialises.
///
/// `FlagKind::Enum` takes a `&'static [&'static str]` and a `CommandSpec` is a
/// `const`, so this cannot be built from `SessionKind::ALL` at compile time —
/// but it must not drift from it, which
/// `session_kind_flag_values_match_the_enum` enforces (WI 0114 F-48).
const SESSION_KIND_FLAG_VALUES: &[&str] = &["local", "remote"];

/// Flags that have been removed. Scanned by [`CommandCatalogue::removed_flag_hint`]
/// before clap parsing so retired flags yield a migration hint. Add an entry
/// here when a flag is dropped; `main.rs` needs no changes.
const REMOVED_FLAGS: &[RemovedFlagSpec] = &[
    // WI-0082: `--mount-ssh` was removed in favour of `--overlay ssh()`.
    RemovedFlagSpec {
        name: "--mount-ssh",
        hint: "Pass `--overlay ssh()` instead (or set `overlays = [\"ssh()\"]` \
               in a per-step workflow entry). See `docs/08-overlays.md`.",
    },
];

// ─── Module layout (WI 0114 F-51) ────────────────────────────────────────────
//
// The specs are data, grouped by the subtree they describe. `ROOT` above is
// what assembles them into the tree, so this file remains the one place the
// catalogue's shape is visible.
mod agents;
mod attach;
mod core;
mod daemon;
mod exec;
mod new;
mod remote;
mod shared_flags;
pub use shared_flags::SessionWorktreeTarget;
mod squad;
// WI 0142: the hidden `view` command spec.
mod link;
mod queue;
mod view;

use agents::*;
use attach::*;
use core::*;
use daemon::*;
use exec::*;
use link::*;
use new::*;
use queue::*;
use remote::*;
use shared_flags::*;
use squad::*;
use view::*;
