//! `exec` and its two leaves.
//!
//! Split out of `dispatch/catalogue.rs` by WI 0114 F-51. The specs are data;
//! `ROOT` in `mod.rs` is what assembles them into the tree.

use super::*;

// ── exec ────────────────────────────────────────────────────────────────────

pub(super) const EXEC: CommandSpec = CommandSpec {
    name: "exec",
    aliases: &[],
    help: "Run a one-shot command: inject a prompt or run a workflow without a work item.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[&EXEC_PROMPT, &EXEC_WORKFLOW],
};

pub(super) const EXEC_PROMPT: CommandSpec = CommandSpec {
    name: "prompt",
    aliases: &[],
    help: "Send a one-shot prompt to the agent.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "prompt",
        // Greedy trailing positional: every remaining token joins into one
        // prompt string. Declaring it here keeps the "join positionals with
        // spaces" behavior spec-driven across all frontends instead of a
        // per-frontend special case (work item 0097, Finding A).
        help: "The prompt text to send to the agent.",
        kind: ArgumentKind::TrailingVarArgs,
        optional: true,
    }],
    flags: &EXEC_PROMPT_FLAGS,
    api_allowed: true,
    build: crate::command::dispatch::build::exec_prompt,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const EXEC_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &["wf"],
    help: "Run a workflow file without requiring a work item number.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "workflow",
        // Optional at the catalogue level so `--dynamic` can omit it; the
        // command layer still requires it for every non-dynamic invocation.
        help: "Path to the workflow file (omit with --dynamic).",
        kind: ArgumentKind::Path,
        optional: true,
    }],
    flags: &EXEC_WORKFLOW_FLAGS,
    api_allowed: true,
    build: crate::command::dispatch::build::exec_workflow,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};
