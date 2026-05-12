//! `CommandCatalogue` — the canonical, single-source-of-truth enumeration of
//! every amux command, subcommand, argument, and flag.
//!
//! Frontends never hard-code command names or flag names; they ask the
//! catalogue (or its projections) for what's available. The catalogue MUST
//! enumerate every command currently defined in `oldsrc/cli.rs` exactly.

use std::sync::OnceLock;

/// Visibility of a command/flag across frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendVisibility {
    /// Visible to every frontend (CLI, TUI, headless).
    All,
    /// CLI-only (e.g. headless server start).
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
}

#[derive(Debug, Clone, Copy)]
pub struct ArgumentSpec {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: ArgumentKind,
    pub optional: bool,
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

    pub fn root(&self) -> &'static CommandSpec {
        self.root
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

    /// Same as `lookup`, but first applies any registered path alias rewrites.
    pub fn lookup_with_aliases(&self, path: &[&str]) -> Option<&'static CommandSpec> {
        let canonical = self.canonical_path(path);
        self.lookup(&canonical)
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
    name: "amux",
    aliases: &[],
    help: "amux — containerized code agent manager",
    long_help: None,
    arguments: &[],
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
        &INIT, &READY, &CHAT, &SPECS, &STATUS, &CONFIG, &EXEC, &HEADLESS, &REMOTE, &NEW,
    ],
};

const PATH_ALIASES: &[(&[&str], &[&str])] = &[];

// ── init ─────────────────────────────────────────────────────────────────────

const AGENT_VALUES: &[&str] = &[
    "claude", "codex", "opencode", "maki", "gemini", "copilot", "crush", "cline",
];

const INIT: CommandSpec = CommandSpec {
    name: "init",
    aliases: &[],
    help: "Initialize the current Git repo for use with amux.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "agent",
            short: None,
            help: "Code agent to install in the Dockerfile.dev container.",
            kind: FlagKind::Enum(AGENT_VALUES),
            default: FlagDefault::Str("claude"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "aspec",
            short: None,
            help: "Download aspec templates to the current project.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
};

// ── ready ────────────────────────────────────────────────────────────────────

const READY: CommandSpec = CommandSpec {
    name: "ready",
    aliases: &[],
    help: "Check Docker daemon, verify Dockerfile.dev, build image, and report status.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "refresh",
            short: None,
            help: "Run the Dockerfile agent audit (skipped by default).",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "build",
            short: None,
            help: "Force rebuild the dev container image from Dockerfile.dev.",
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
            help: "Pass --no-cache to docker build.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the agent in non-interactive (print) mode instead of interactive mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "allow-docker",
            short: None,
            help: "Mount the host Docker daemon socket into the agent container.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "json",
            short: None,
            help: "Suppress human output and print structured JSON. Implies --non-interactive.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &["non-interactive"],
            optional: true,
        },
    ],
    subcommands: &[],
};

// ── chat ─────────────────────────────────────────────────────────────────────

const CHAT: CommandSpec = CommandSpec {
    name: "chat",
    aliases: &[],
    help: "Start a freeform chat session with the configured agent in a container.",
    long_help: None,
    arguments: &[],
    flags: &AGENT_RUN_FLAGS_NO_WORKTREE,
    subcommands: &[],
};

// ── specs ───────────────────────────────────────────────────────────────────

const SPECS: CommandSpec = CommandSpec {
    name: "specs",
    aliases: &[],
    help: "Manage work item specs (amend).",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&SPECS_AMEND],
};

const SPECS_AMEND: CommandSpec = CommandSpec {
    name: "amend",
    aliases: &[],
    help: "Review and amend a completed work item to match the final implementation.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "work_item",
        help: "Work item number (e.g. 0025).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "allow-docker",
            short: None,
            help: "Mount the host Docker daemon socket into the agent container.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
};

// ── status ───────────────────────────────────────────────────────────────────

const STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show the status of all running code-agent containers.",
    long_help: None,
    arguments: &[],
    flags: &[FlagSpec {
        long: "watch",
        short: None,
        help: "Continuously refresh the output every 3 seconds.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    subcommands: &[],
};

// ── config ───────────────────────────────────────────────────────────────────

const CONFIG: CommandSpec = CommandSpec {
    name: "config",
    aliases: &[],
    help: "View and edit global and repo configuration.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&CONFIG_SHOW, &CONFIG_GET, &CONFIG_SET],
};

const CONFIG_SHOW: CommandSpec = CommandSpec {
    name: "show",
    aliases: &[],
    help: "Display all config fields at both global and repo level.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[],
};

const CONFIG_GET: CommandSpec = CommandSpec {
    name: "get",
    aliases: &[],
    help: "Show a single field's global value, repo value, and effective value.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "field",
        help: "Config field name (e.g. terminal_scrollback_lines).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[],
    subcommands: &[],
};

const CONFIG_SET: CommandSpec = CommandSpec {
    name: "set",
    aliases: &[],
    help: "Set a config field value (repo scope by default).",
    long_help: None,
    arguments: &[
        ArgumentSpec {
            name: "field",
            help: "Config field name.",
            kind: ArgumentKind::String,
            optional: false,
        },
        ArgumentSpec {
            name: "value",
            help: "New value for the field.",
            kind: ArgumentKind::String,
            optional: false,
        },
    ],
    flags: &[FlagSpec {
        long: "global",
        short: None,
        help: "Write to global config instead of repo config.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    subcommands: &[],
};

// ── exec ────────────────────────────────────────────────────────────────────

const EXEC: CommandSpec = CommandSpec {
    name: "exec",
    aliases: &[],
    help: "Run a one-shot command: inject a prompt or run a workflow without a work item.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&EXEC_PROMPT, &EXEC_WORKFLOW],
};

const EXEC_PROMPT: CommandSpec = CommandSpec {
    name: "prompt",
    aliases: &[],
    help: "Send a one-shot prompt to the agent.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "prompt",
        help: "The prompt text to send to the agent.",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &AGENT_RUN_FLAGS_NO_WORKTREE,
    subcommands: &[],
};

const EXEC_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &["wf"],
    help: "Run a workflow file without requiring a work item number.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "workflow",
        help: "Path to the workflow file.",
        kind: ArgumentKind::Path,
        optional: false,
    }],
    flags: &EXEC_WORKFLOW_FLAGS,
    subcommands: &[],
};

// ── headless ────────────────────────────────────────────────────────────────

const HEADLESS: CommandSpec = CommandSpec {
    name: "headless",
    aliases: &[],
    help: "Run amux as a headless HTTP server for remote/automated access.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[
        &HEADLESS_START,
        &HEADLESS_KILL,
        &HEADLESS_LOGS,
        &HEADLESS_STATUS,
    ],
};

const HEADLESS_START: CommandSpec = CommandSpec {
    name: "start",
    aliases: &[],
    help: "Start the headless HTTP server.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "port",
            short: None,
            help: "Port to listen on.",
            kind: FlagKind::U16,
            default: FlagDefault::U16(9876),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "workdirs",
            short: None,
            help: "Allowlisted working directories (repeatable).",
            kind: FlagKind::VecString,
            default: FlagDefault::EmptyVec,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "background",
            short: None,
            help: "Daemonize via the OS process manager.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "refresh-key",
            short: None,
            help: "Regenerate the API key.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "dangerously-skip-auth",
            short: None,
            help: "Disable authentication for this execution even if a key hash exists on disk.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
};

const HEADLESS_KILL: CommandSpec = CommandSpec {
    name: "kill",
    aliases: &[],
    help: "Stop the background headless server.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[],
};

const HEADLESS_LOGS: CommandSpec = CommandSpec {
    name: "logs",
    aliases: &[],
    help: "Stream the background server log file to stdout.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[],
};

const HEADLESS_STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show headless server status.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[],
};

// ── remote ──────────────────────────────────────────────────────────────────

const REMOTE: CommandSpec = CommandSpec {
    name: "remote",
    aliases: &[],
    help: "Connect to a remote headless amux instance and execute commands.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&REMOTE_RUN, &REMOTE_SESSION],
};

const REMOTE_RUN: CommandSpec = CommandSpec {
    name: "run",
    aliases: &[],
    help: "Execute a command on the remote headless amux host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "command",
        help: "The amux subcommand and arguments to execute on the remote host.",
        kind: ArgumentKind::TrailingVarArgs,
        optional: false,
    }],
    flags: REMOTE_RUN_FLAGS,
    subcommands: &[],
};

const REMOTE_RUN_FLAGS: &[FlagSpec] = &[
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote headless amux host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "session",
        short: None,
        help: "Session ID to run the command in.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "follow",
        short: Some('f'),
        help: "Stream logs from the remote host until the command completes.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote headless amux host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const REMOTE_SESSION: CommandSpec = CommandSpec {
    name: "session",
    aliases: &[],
    help: "Manage sessions on the remote headless amux host.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&REMOTE_SESSION_START, &REMOTE_SESSION_KILL],
};

const REMOTE_SESSION_FLAGS: &[FlagSpec] = &[
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote headless amux host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote headless amux host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const REMOTE_SESSION_START: CommandSpec = CommandSpec {
    name: "start",
    aliases: &[],
    help: "Start a new session on the remote host for the given directory.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "dir",
        help: "Working directory to use for the new session.",
        kind: ArgumentKind::OptionalString,
        optional: true,
    }],
    flags: REMOTE_SESSION_FLAGS,
    subcommands: &[],
};

const REMOTE_SESSION_KILL: CommandSpec = CommandSpec {
    name: "kill",
    aliases: &[],
    help: "Kill a session on the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "session_id",
        help: "Session ID to kill.",
        kind: ArgumentKind::OptionalString,
        optional: true,
    }],
    flags: REMOTE_SESSION_FLAGS,
    subcommands: &[],
};

// ── new ─────────────────────────────────────────────────────────────────────

const NEW: CommandSpec = CommandSpec {
    name: "new",
    aliases: &[],
    help: "Create a new amux artefact (spec, workflow, or skill).",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&NEW_SPEC, &NEW_WORKFLOW, &NEW_SKILL],
};

const NEW_SPEC: CommandSpec = CommandSpec {
    name: "spec",
    aliases: &[],
    help: "Create a new work item spec.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "interview",
            short: None,
            help: "Use interview mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the interview agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
};

const WORKFLOW_FORMAT_VALUES: &[&str] = &["toml", "yaml", "md"];

const NEW_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &[],
    help: "Interactively create a new workflow file.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "interview",
            short: None,
            help: "Let a code agent complete the workflow from a summary you provide.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the interview agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "global",
            short: None,
            help: "Write to ~/.amux/workflows/<name> instead of the current repo.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "format",
            short: None,
            help: "Output file format.",
            kind: FlagKind::Enum(WORKFLOW_FORMAT_VALUES),
            default: FlagDefault::Str("toml"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
};

const NEW_SKILL: CommandSpec = CommandSpec {
    name: "skill",
    aliases: &[],
    help: "Interactively create a new skill file.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "interview",
            short: None,
            help: "Let a code agent complete the skill body from a summary you provide.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the interview agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "global",
            short: None,
            help: "Write to ~/.amux/skills/<name>/ instead of the current repo.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
};

// ─── Reusable agent-run flag arrays ─────────────────────────────────────────

/// Agent-run flag set used by `chat` and `exec prompt` (no worktree, no
/// workflow). All optional. Mode flags `yolo` / `auto` / `plan` are mutually
/// exclusive.
const AGENT_RUN_FLAGS_NO_WORKTREE: [FlagSpec; 9] = [
    FlagSpec {
        long: "non-interactive",
        short: Some('n'),
        help: "Run the agent in non-interactive (print) mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "plan",
        short: None,
        help: "Run the agent in plan mode (read-only).",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["yolo"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "allow-docker",
        short: None,
        help: "Mount the host Docker daemon socket into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "mount-ssh",
        short: None,
        help: "Mount host ~/.ssh read-only into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "yolo",
        short: None,
        help: "Enable fully autonomous mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["plan"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "auto",
        short: None,
        help: "Enable auto permission mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "agent",
        short: None,
        help: "Agent to use (overrides .amux/config.json).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "model",
        short: None,
        help: "Override the model used by the launched agent.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "overlay",
        short: None,
        help: "Mount a host directory into the agent container. Repeatable.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const EXEC_WORKFLOW_FLAGS: [FlagSpec; 11] = [
    FlagSpec {
        long: "work-item",
        short: None,
        help: "Optional work item number.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "non-interactive",
        short: Some('n'),
        help: "Run the agent in non-interactive (print) mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "plan",
        short: None,
        help: "Run the agent in plan mode (read-only).",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["yolo"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "allow-docker",
        short: None,
        help: "Mount the host Docker daemon socket into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "worktree",
        short: None,
        help: "Run in an isolated Git worktree under ~/.amux/worktrees/.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "mount-ssh",
        short: None,
        help: "Mount host ~/.ssh read-only into the agent container.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "yolo",
        short: None,
        help: "Enable fully autonomous mode. Implies --worktree.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["plan"],
        implies: &["worktree"],
        optional: true,
    },
    FlagSpec {
        long: "auto",
        short: None,
        help: "Enable auto permission mode. Implies --worktree.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &["worktree"],
        optional: true,
    },
    FlagSpec {
        long: "agent",
        short: None,
        help: "Agent to use.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "model",
        short: None,
        help: "Override the model used by the launched agent.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "overlay",
        short: None,
        help: "Mount a host directory into the agent container. Repeatable.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_top_level_returns_spec() {
        let cat = CommandCatalogue::get();
        let spec = cat.lookup(&["init"]).expect("init must be present");
        assert_eq!(spec.name, "init");
    }

    #[test]
    fn lookup_nested_returns_spec() {
        let cat = CommandCatalogue::get();
        let spec = cat
            .lookup(&["exec", "workflow"])
            .expect("exec workflow must be present");
        assert_eq!(spec.name, "workflow");
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let cat = CommandCatalogue::get();
        assert!(cat.lookup(&["bogus"]).is_none());
        assert!(cat.lookup(&["init", "bogus"]).is_none());
    }

    #[test]
    fn string_alias_wf_resolves_to_workflow() {
        let cat = CommandCatalogue::get();
        let spec = cat.lookup(&["exec", "wf"]).unwrap();
        assert_eq!(spec.name, "workflow");
    }

    #[test]
    fn ready_json_implies_non_interactive() {
        let cat = CommandCatalogue::get();
        let ready = cat.lookup(&["ready"]).unwrap();
        let json_flag = ready.find_flag("json").unwrap();
        assert!(json_flag.implies.contains(&"non-interactive"));
    }

    #[test]
    fn exec_workflow_yolo_implies_worktree() {
        let cat = CommandCatalogue::get();
        let exec_workflow = cat.lookup(&["exec", "workflow"]).unwrap();
        let yolo = exec_workflow.find_flag("yolo").unwrap();
        assert!(yolo.implies.contains(&"worktree"));
    }

    #[test]
    fn exec_workflow_auto_implies_worktree() {
        let cat = CommandCatalogue::get();
        let exec_workflow = cat.lookup(&["exec", "workflow"]).unwrap();
        let auto = exec_workflow.find_flag("auto").unwrap();
        assert!(auto.implies.contains(&"worktree"));
    }

    #[test]
    fn plan_and_yolo_are_mutually_exclusive_on_chat() {
        let cat = CommandCatalogue::get();
        let chat = cat.lookup(&["chat"]).unwrap();
        let plan = chat.find_flag("plan").unwrap();
        assert!(plan.conflicts_with("yolo"));
        let yolo = chat.find_flag("yolo").unwrap();
        assert!(yolo.conflicts_with("plan"));
    }

    #[test]
    fn every_top_level_command_is_present() {
        let cat = CommandCatalogue::get();
        for name in [
            "init", "ready", "chat", "specs", "status", "config", "exec", "headless", "remote",
            "new",
        ] {
            assert!(cat.lookup(&[name]).is_some(), "missing top-level '{name}'");
        }
    }

    #[test]
    fn remote_run_has_trailing_var_args_argument() {
        let cat = CommandCatalogue::get();
        let run = cat.lookup(&["remote", "run"]).unwrap();
        assert_eq!(run.arguments.len(), 1);
        assert!(matches!(
            run.arguments[0].kind,
            ArgumentKind::TrailingVarArgs
        ));
    }

    // ─── Data-table tests ─────────────────────────────────────────────────────

    /// Compact check for a single flag: path, flag name, whether it is a Bool,
    /// and whether it is optional.  The `bool_expected` field avoids PartialEq
    /// on `FlagKind` (which contains `&'static [&'static str]` slices).
    struct FlagCheck {
        path: &'static [&'static str],
        flag: &'static str,
        is_bool: bool,
        is_optional: bool,
    }

    const FLAG_TABLE: &[FlagCheck] = &[
        FlagCheck {
            path: &["init"],
            flag: "agent",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["init"],
            flag: "aspec",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "refresh",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "build",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "no-cache",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "non-interactive",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "allow-docker",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["ready"],
            flag: "json",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "non-interactive",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "plan",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "yolo",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "auto",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "allow-docker",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "mount-ssh",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "agent",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "model",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["chat"],
            flag: "overlay",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "yolo",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "auto",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "worktree",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "work-item",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "workflow"],
            flag: "plan",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "prompt"],
            flag: "yolo",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["exec", "prompt"],
            flag: "overlay",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["status"],
            flag: "watch",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["config", "set"],
            flag: "global",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["headless", "start"],
            flag: "port",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["headless", "start"],
            flag: "workdirs",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["headless", "start"],
            flag: "background",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["headless", "start"],
            flag: "refresh-key",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["headless", "start"],
            flag: "dangerously-skip-auth",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "run"],
            flag: "follow",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "run"],
            flag: "api-key",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "run"],
            flag: "remote-addr",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "session", "start"],
            flag: "api-key",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["remote", "session", "kill"],
            flag: "remote-addr",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "workflow"],
            flag: "format",
            is_bool: false,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "workflow"],
            flag: "interview",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "workflow"],
            flag: "global",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "skill"],
            flag: "interview",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "skill"],
            flag: "global",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["new", "spec"],
            flag: "interview",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["specs", "amend"],
            flag: "non-interactive",
            is_bool: true,
            is_optional: true,
        },
        FlagCheck {
            path: &["specs", "amend"],
            flag: "allow-docker",
            is_bool: true,
            is_optional: true,
        },
    ];

    #[test]
    fn all_documented_flags_present_with_correct_kind_and_optional() {
        let cat = CommandCatalogue::get();
        for case in FLAG_TABLE {
            let spec = cat
                .lookup(case.path)
                .unwrap_or_else(|| panic!("command {:?} not found in catalogue", case.path));
            let flag = spec
                .find_flag(case.flag)
                .unwrap_or_else(|| panic!("flag '{}' not found on {:?}", case.flag, case.path));
            assert_eq!(
                flag.optional, case.is_optional,
                "optional mismatch for '{}' on {:?}",
                case.flag, case.path
            );
            assert_eq!(
                matches!(flag.kind, FlagKind::Bool),
                case.is_bool,
                "is_bool mismatch for '{}' on {:?}",
                case.flag,
                case.path
            );
        }
    }

    #[test]
    fn all_expected_subcommands_are_present() {
        let cat = CommandCatalogue::get();
        let cases: &[(&[&str], &str)] = &[
            (&["specs"], "amend"),
            (&["config"], "show"),
            (&["config"], "get"),
            (&["config"], "set"),
            (&["exec"], "prompt"),
            (&["exec"], "workflow"),
            (&["headless"], "start"),
            (&["headless"], "kill"),
            (&["headless"], "logs"),
            (&["headless"], "status"),
            (&["remote"], "run"),
            (&["remote"], "session"),
            (&["remote", "session"], "start"),
            (&["remote", "session"], "kill"),
            (&["new"], "spec"),
            (&["new"], "workflow"),
            (&["new"], "skill"),
        ];
        for (parent_path, subcmd_name) in cases {
            let parent = cat
                .lookup(parent_path)
                .unwrap_or_else(|| panic!("parent {:?} not found", parent_path));
            assert!(
                parent.find_subcommand(subcmd_name).is_some(),
                "subcommand '{}' not found under {:?}",
                subcmd_name,
                parent_path
            );
        }
    }

    #[test]
    fn flag_spec_conflicts_with_accessor_is_symmetric_on_chat() {
        let cat = CommandCatalogue::get();
        let chat = cat.lookup(&["chat"]).unwrap();
        let plan = chat.find_flag("plan").unwrap();
        let yolo = chat.find_flag("yolo").unwrap();
        assert!(plan.conflicts_with("yolo"), "plan must conflict with yolo");
        assert!(yolo.conflicts_with("plan"), "yolo must conflict with plan");
        assert!(
            !plan.conflicts_with("non-interactive"),
            "plan must NOT conflict with non-interactive"
        );
    }

    #[test]
    fn headless_start_flags_are_cli_only() {
        let cat = CommandCatalogue::get();
        let start = cat.lookup(&["headless", "start"]).unwrap();
        for flag in start.flags {
            assert!(
                matches!(flag.frontends, FrontendVisibility::CliOnly),
                "headless start flag '{}' must be CliOnly, got {:?}",
                flag.long,
                flag.frontends
            );
        }
    }

    #[test]
    fn exec_workflow_arguments_include_workflow_path() {
        let cat = CommandCatalogue::get();
        let wf = cat.lookup(&["exec", "workflow"]).unwrap();
        assert_eq!(wf.arguments.len(), 1);
        assert_eq!(wf.arguments[0].name, "workflow");
        assert!(matches!(wf.arguments[0].kind, ArgumentKind::Path));
    }

    #[test]
    fn config_get_and_set_have_required_field_argument() {
        let cat = CommandCatalogue::get();
        let get = cat.lookup(&["config", "get"]).unwrap();
        assert_eq!(get.arguments.len(), 1);
        assert_eq!(get.arguments[0].name, "field");
        assert!(!get.arguments[0].optional);

        let set = cat.lookup(&["config", "set"]).unwrap();
        assert_eq!(set.arguments.len(), 2);
        let names: Vec<&str> = set.arguments.iter().map(|a| a.name).collect();
        assert!(names.contains(&"field") && names.contains(&"value"));
    }
}
