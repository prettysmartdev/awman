//! Flag arrays more than one command declares, and the `const fn` machinery
//! that derives one array from another.
//!
//! Deriving rather than copying is what keeps `exec prompt` and `remote exec
//! prompt` from drifting from `chat`: a flag added to the base set appears in
//! all of them, and the compiler checks the arity.
//!
//! Split out of `dispatch/catalogue.rs` by WI 0114 F-51. The specs are data;
//! `ROOT` in `mod.rs` is what assembles them into the tree.

use super::*;

// ─── Programmatic derivation of remote exec flag sets ────────────────────────
//
// Per the work item: `remote exec workflow` accepts the same flags as the
// local `exec workflow`, minus flags that make no sense remotely (`--workdir`
// is implicit and `--worktree` is a server-side concern). Plus remote-transport
// flags (`--remote-addr`, `--session`, `--api-key`, `--follow`).
//
// The flag list is built at compile time by const fn so that any future
// addition to AGENT_RUN_FLAGS_NO_WORKTREE / EXEC_WORKFLOW_FLAGS is picked up
// automatically — no manual list maintenance.

pub(super) const REMOTE_EXEC_EXCLUDED_FLAG_NAMES: &[&str] = &["workdir", "worktree"];

pub(super) const REMOTE_TRANSPORT_FLAGS: [FlagSpec; 4] = [
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote awman API host.",
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
        help: "Session ID to use on the remote host.",
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
        help: "API key for the remote awman API host.",
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
        help: "Stream logs via SSE until the command completes.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

pub(super) const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

pub(super) const fn const_str_in_list(needle: &str, haystack: &[&str]) -> bool {
    let mut i = 0;
    while i < haystack.len() {
        if const_str_eq(haystack[i], needle) {
            return true;
        }
        i += 1;
    }
    false
}

pub(super) const fn count_kept(base: &[FlagSpec], excluded: &[&str]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < base.len() {
        if !const_str_in_list(base[i].long, excluded) {
            count += 1;
        }
        i += 1;
    }
    count
}

pub(super) const REMOTE_EXEC_WORKFLOW_KEPT: usize =
    count_kept(&EXEC_WORKFLOW_FLAGS, REMOTE_EXEC_EXCLUDED_FLAG_NAMES);
pub(super) const REMOTE_EXEC_WORKFLOW_TOTAL: usize =
    REMOTE_TRANSPORT_FLAGS.len() + REMOTE_EXEC_WORKFLOW_KEPT;

pub(super) const REMOTE_EXEC_PROMPT_KEPT: usize =
    count_kept(&EXEC_PROMPT_FLAGS, REMOTE_EXEC_EXCLUDED_FLAG_NAMES);
pub(super) const REMOTE_EXEC_PROMPT_TOTAL: usize =
    REMOTE_TRANSPORT_FLAGS.len() + REMOTE_EXEC_PROMPT_KEPT;

pub(super) const fn build_remote_flags<const N: usize>(
    base: &[FlagSpec],
    excluded: &[&str],
) -> [FlagSpec; N] {
    let mut out: [FlagSpec; N] = [REMOTE_TRANSPORT_FLAGS[0]; N];
    let mut idx = 0;
    let mut i = 0;
    while i < REMOTE_TRANSPORT_FLAGS.len() {
        out[idx] = REMOTE_TRANSPORT_FLAGS[i];
        idx += 1;
        i += 1;
    }
    let mut j = 0;
    while j < base.len() {
        if !const_str_in_list(base[j].long, excluded) {
            out[idx] = base[j];
            idx += 1;
        }
        j += 1;
    }
    out
}

pub(super) const REMOTE_EXEC_WORKFLOW_FLAGS: [FlagSpec; REMOTE_EXEC_WORKFLOW_TOTAL] =
    build_remote_flags::<REMOTE_EXEC_WORKFLOW_TOTAL>(
        &EXEC_WORKFLOW_FLAGS,
        REMOTE_EXEC_EXCLUDED_FLAG_NAMES,
    );

pub(super) const REMOTE_EXEC_PROMPT_FLAGS: [FlagSpec; REMOTE_EXEC_PROMPT_TOTAL] =
    build_remote_flags::<REMOTE_EXEC_PROMPT_TOTAL>(
        &EXEC_PROMPT_FLAGS,
        REMOTE_EXEC_EXCLUDED_FLAG_NAMES,
    );

// ─── Reusable agent-run flag arrays ─────────────────────────────────────────

/// Agent-run flag set used by `chat` and `exec prompt` (no worktree, no
/// workflow). All optional. Mode flags `yolo` / `auto` / `plan` are mutually
/// exclusive.
pub(super) const AGENT_RUN_FLAGS_NO_WORKTREE: [FlagSpec; 9] = [
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
        long: "launch-mode",
        short: None,
        help: "Launch the agent over stdio or ACP.",
        kind: FlagKind::Enum(&["stdio", "acp"]),
        default: FlagDefault::None,
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
        help: "Agent to use (overrides .awman/config.json).",
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

/// Agent-run flags for `exec prompt` — extends `AGENT_RUN_FLAGS_NO_WORKTREE`
/// with `--issue`. Scoped to `exec prompt` only; `chat` retains the base set.
/// The one flag `exec prompt` adds to the base agent-run set.
pub(super) const EXEC_PROMPT_EXTRA_FLAGS: [FlagSpec; 1] = [FlagSpec {
    long: "issue",
    short: None,
    help: "GitHub issue number, URL, or owner/repo#N to use as the prompt.",
    kind: FlagKind::OptionalString,
    default: FlagDefault::None,
    frontends: FrontendVisibility::All,
    conflicts_with: &[],
    implies: &[],
    optional: true,
}];

/// Agent-run flags for `exec prompt` — the base set plus `--issue`.
///
/// Derived rather than copied (WI 0114 F-51): the nine base flags were written
/// out a second time here, so a change to one of them silently applied to
/// `chat` and not to `exec prompt`. The arity is checked by the compiler.
pub(super) const EXEC_PROMPT_FLAGS: [FlagSpec;
    AGENT_RUN_FLAGS_NO_WORKTREE.len() + EXEC_PROMPT_EXTRA_FLAGS.len()] =
    concat_flags(&[&AGENT_RUN_FLAGS_NO_WORKTREE, &EXEC_PROMPT_EXTRA_FLAGS]);

/// `--work-item`, which `exec workflow` lists before everything else.
pub(super) const EXEC_WORKFLOW_LEADING_FLAGS: [FlagSpec; 1] = [FlagSpec {
    long: "work-item",
    short: None,
    help: "Optional work item number.",
    kind: FlagKind::OptionalString,
    default: FlagDefault::None,
    frontends: FrontendVisibility::All,
    conflicts_with: &["issue"],
    implies: &[],
    optional: true,
}];

/// `--worktree`, which `exec workflow` lists between `--launch-mode` and
/// `--yolo` — the position the flag has always had in `--help`.
pub(super) const EXEC_WORKFLOW_WORKTREE_FLAG: [FlagSpec; 1] = [FlagSpec {
    long: "worktree",
    short: None,
    help: "Run in an isolated Git worktree under ~/.awman/worktrees/.",
    kind: FlagKind::Bool,
    default: FlagDefault::Bool(false),
    frontends: FrontendVisibility::All,
    conflicts_with: &[],
    implies: &[],
    optional: true,
}];

/// The flags `exec workflow` adds after the base set.
pub(super) const EXEC_WORKFLOW_TRAILING_FLAGS: [FlagSpec; 4] = [
    FlagSpec {
        long: "issue",
        short: None,
        help: "GitHub issue number, URL, or owner/repo#N to use as work item input.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &["work-item"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "dynamic",
        short: None,
        help: "Have a leader agent design and run a workflow for --work-item. \
               Implies --yolo, --worktree, and context(workflow); the positional \
               workflow path must be omitted.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        // Mutual exclusions (positional path, --plan) and the --work-item
        // requirement are enforced in the command layer because --yolo may be
        // implied rather than explicitly supplied (WI-0092 §3).
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "leader",
        short: None,
        help: "Agent and model for the dynamic leader, as agent::model \
               (e.g. claude::claude-opus-4-8). Only valid with --dynamic.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "max-concurrent",
        short: None,
        help: "Cap on concurrently-running workflow steps (must be >= 1).",
        kind: FlagKind::UsizeAtLeastOne,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

/// Agent-run flags for `exec workflow`.
///
/// Derived rather than copied (WI 0114 F-51). Unlike `exec prompt` this is not
/// a plain append: `--work-item` comes first and `--worktree` sits inside the
/// base run, so the base set is spliced rather than concatenated. The order is
/// exactly what the literal had, because it is the order `--help` and the
/// generated command reference print.
pub(super) const EXEC_WORKFLOW_FLAGS: [FlagSpec; 15] = concat_flags(&[
    &EXEC_WORKFLOW_LEADING_FLAGS,
    AGENT_RUN_BEFORE_WORKTREE,
    &EXEC_WORKFLOW_WORKTREE_FLAG,
    AGENT_RUN_AFTER_WORKTREE,
    &EXEC_WORKFLOW_TRAILING_FLAGS,
]);

/// The three base flags `exec workflow` declares differently.
///
/// `--yolo` and `--auto` imply `--worktree` here and say so in their help;
/// `--agent`'s help is shorter. Everything else in the base set is shared
/// verbatim, which is the point of deriving it (WI 0114 F-51): before, all
/// nine were written out again and only these three were meant to differ.
pub(super) const EXEC_WORKFLOW_BASE_OVERRIDES: [FlagSpec; 3] = [
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
];

/// The base agent-run set as `exec workflow` declares it.
pub(super) const EXEC_WORKFLOW_BASE: [FlagSpec; AGENT_RUN_FLAGS_NO_WORKTREE.len()] =
    override_flags(&AGENT_RUN_FLAGS_NO_WORKTREE, &EXEC_WORKFLOW_BASE_OVERRIDES);

/// The base agent-run flags that precede `--worktree` in `exec workflow`:
/// `--non-interactive`, `--plan`, `--allow-docker`, `--launch-mode`.
const AGENT_RUN_BEFORE_WORKTREE: &[FlagSpec] = EXEC_WORKFLOW_BASE.split_at(4).0;
/// The rest of them: `--yolo`, `--auto`, `--agent`, `--model`, `--overlay`.
const AGENT_RUN_AFTER_WORKTREE: &[FlagSpec] = EXEC_WORKFLOW_BASE.split_at(4).1;

/// Concatenate flag arrays in order.
///
/// The `const fn` counterpart of `[a, b].concat()`, which is not available in
/// a `const` item. `N` must equal the total length; the compiler checks it.
/// Same technique as [`build_remote_flags`] above (WI 0114 F-51).
pub(super) const fn concat_flags<const N: usize>(parts: &[&[FlagSpec]]) -> [FlagSpec; N] {
    // Any `FlagSpec` will do as the fill value: every slot is overwritten
    // below, and the compiler rejects an `N` that leaves one behind.
    let mut out: [FlagSpec; N] = [REMOTE_TRANSPORT_FLAGS[0]; N];
    let mut written = 0;
    let mut p = 0;
    while p < parts.len() {
        let part = parts[p];
        let mut i = 0;
        while i < part.len() {
            out[written] = part[i];
            written += 1;
            i += 1;
        }
        p += 1;
    }
    out
}

/// `base`, with any flag whose `long` name appears in `overrides` replaced by
/// that override. Order follows `base`.
///
/// The `const fn` way to say "the same set, but these three are different"
/// (WI 0114 F-51). An override naming a flag that is not in `base` is a
/// mistake, and `every_override_replaces_a_base_flag` catches it.
pub(super) const fn override_flags<const N: usize>(
    base: &[FlagSpec],
    overrides: &[FlagSpec],
) -> [FlagSpec; N] {
    let mut out: [FlagSpec; N] = [REMOTE_TRANSPORT_FLAGS[0]; N];
    let mut i = 0;
    while i < base.len() {
        let mut chosen = base[i];
        let mut j = 0;
        while j < overrides.len() {
            if const_str_eq(overrides[j].long, base[i].long) {
                chosen = overrides[j];
            }
            j += 1;
        }
        out[i] = chosen;
        i += 1;
    }
    out
}
