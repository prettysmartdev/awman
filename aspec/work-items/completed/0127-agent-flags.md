# Work Item: Feature

Title: User-configurable extra agent flags (`agentFlags` / `--agent-flag`)
Issue: n/a (newer Codex releases fail to start in a container without `--no-daemon`)

Status: Completed 2026-10-19. Implemented in 61a04b39; it survived the WI 0126 merge
(fe980408). A 2026-10-19 review added: de-duplication of an `agentFlags` entry
awman already manages, with a launch warning; a `remote exec` warning instead of
silently dropping `--agent-flag`; worktree sessions that keep the invoker's
default agent, so `--agent-flag` targets the agent a step actually runs; one-sentence
help text; and an `agentFlags` case in the Docker/Apple vs builtin argv parity
test. It also found and fixed an older bug: the non-interactive subcommand was
emitted twice (`codex exec exec`), so "after the non-interactive subcommand"
below is now what actually runs.

## Summary:

- Newer Codex releases won't start inside a container unless they get
  `--no-daemon`. awman builds each agent's argv from the agent matrix, so users
  had no way to add a flag the matrix doesn't know about.
- As an interim fix, users can set extra agent arguments per agent in repo and
  global config (`agentFlags`) or per run (`--agent-flag`). awman appends them
  to the agent command it renders at launch.
- Follow-up finding: `codex exec` *rejects* `--no-daemon`, so an `agentFlags`
  codex entry (which reaches every launch) breaks non-interactive runs.
  Interactive Codex's TUI starts through an app-server daemon that the image
  never ships. Codex looks for it under `$CODEX_HOME/packages`, and the host
  `~/.codex` mounted over `$CODEX_HOME` hides or replaces
  `packages/app-server-daemon`. So awman now passes `--no-daemon` itself, on
  interactive codex launches only, through the agent matrix's new
  `interactive_only_flags`. `agentFlags` remains the general escape hatch,
  documented with an example flag both modes accept.

## User Stories

### User Story 1:
As a: user

I want to:
add `"agentFlags": {"codex": ["--no-daemon"]}` to my global or repo
`config.json`

So I can:
keep using the latest Codex in awman containers without waiting for an awman
release that knows about the flag.

(Superseded for `--no-daemon` itself: awman now passes it on interactive codex
launches, and a configured entry for it is dropped there with a warning.)

### User Story 2:
As a: user

I want to:
pass `--agent-flag=<arg>` on `chat`, `exec prompt`, or `exec workflow`

So I can:
try a flag for one run without editing config.


## Implementation Details:

- **Schema.** A top-level `agentFlags` map from agent name to a list of
  strings (`AgentFlagsConfig = BTreeMap<String, Vec<String>>`), on both
  `RepoConfig` and `GlobalConfig`. It is keyed by agent because the flags are
  agent-specific: `--no-daemon` would break Claude, and workflows and squad mix
  agents in one session. The keyed-map shape matches
  `dynamicWorkflows.agentsToModels`.
- **Validation (Layer 0, at load).** Keys must be valid `AgentName`s. Entries
  must be non-empty after trimming (`validate_agent_flags`,
  `validate_agent_flag_values`). `--agent-flag` values get the same entry check
  in `dispatch::parse_agent_flags`.
- **Merge.** `EffectiveConfig::agent_flags(agent)` uses flag > repo entry >
  global entry > none. Each level replaces the lower one for that agent, the
  same as `yoloDisallowedTools`, `--model`, and the other replace-style
  settings. An empty repo entry turns off a global one. The CLI list applies
  only when `agent` is the invocation's selected agent (`EffectiveConfig::agent`).
- **CLI.** `--agent-flag` (`FlagKind::VecString`) is added to
  `AGENT_RUN_FLAGS_NO_WORKTREE`, so `chat`, `exec prompt`, `exec workflow`, and
  the derived `remote exec` sets all get it. Values usually begin with `-`, so
  they are passed as `--agent-flag=<value>`. The clap, TUI command-box, and API
  raw-args parsers all accept that form, so no new flag kind or
  `allow_hyphen_values` change was needed.
- **Workflows.** Every workflow launch resolves `agentFlags` from the
  *invoking* session, through `factory::workflow_agent_flags`, never from the
  re-rooted worktree session. That session re-reads `.awman/config.json`
  from the worktree checkout, which is cut from the last commit and so lacks
  uncommitted edits (choosing "use last commit" or resuming an older worktree
  makes that common). It is also opened with an empty env snapshot, which
  misses a relocated global config. Before this, a `--yolo`/`--auto`/
  `--dynamic` run silently dropped an uncommitted `agentFlags` entry.
  - Normal and generated steps: `CommandLayerFactory::agent_flags` is built
    in `execute_prepared` from `original_session` before the swap. Each step
    gets `Some(resolver.agent_flags(step_agent))`. `--agent-flag` targets the
    workflow's default agent: the workflow `agent` field, else `--agent`,
    else config. Steps naming another agent keep their configured entry.
  - Dynamic leader and its repair runs (`drive_leader_agent`): resolved from
    `self.session`, the base session. `--agent-flag` reaches the leader only
    when its agent is the selected one (`--agent`, else config).
- **Engine.** `AgentRunOptions::agent_flags: Option<Vec<String>>` carries the
  command-scoped CLI list. `None` resolves from `session.effective_config()`.
  Every launch path therefore picks up config `agentFlags` with no per-command
  plumbing (init, ready, specs, squad, dynamic leader, new skill/workflow).
- **Rendering.** `ContainerOption::AgentFlags` and `SandboxOption::AgentFlags`.
  - Docker / Apple (`build_run_argv`) and builtin (`agent_argv`) append the
    flags after every awman-generated agent flag (non-interactive subcommand,
    mode flags, tools, model, system prompt, add-dir) and before the seeded
    prompt. Flags placed after the prompt would be read as prompt text.
  - ACP launches still emit nothing of awman's own after the entrypoint, but
    they do keep the user's `agentFlags`. The user configured those flags for
    that agent, and dropping them silently would defeat the purpose.
  - sbx `kind: agent` kits get the flags after `--`. `kind: mixin` kits
    (including codex) have no argv channel, so awman emits an
    `UnsupportedNote` warning instead of dropping them silently.


## Edge Case Considerations:

- A repo entry of `[]` turns off a global entry for that agent. Omitting the
  agent from the repo map inherits the global entry.
- A workflow step that names a different agent than the default one doesn't
  get the CLI list. It gets its own configured entry.
- `--agent-flag --no-daemon` (space-separated) is a usage error in every
  frontend, because the value looks like a flag. The help text and docs show
  the `=` form.
- `exec prompt`'s trailing prompt absorbs any flags written after it (existing
  behavior), so `--agent-flag` has to come before the prompt.
- Values are passed as literal argv entries with no shell parsing, so a value
  containing spaces stays a single argument.
- `remote exec prompt` / `remote exec workflow` accept the flag (derived flag
  set) but, like `--model` and `--overlay`, don't forward it to the remote
  host. They warn that it was not forwarded.
- An `agentFlags` entry equal to a flag awman's entrypoint already carries for
  that launch (codex `--no-daemon`, interactive) is dropped from that launch,
  and `LaunchPolicy::warn_config` warns once per agent and flag. A
  non-interactive launch keeps it as configured.

## Test Considerations:

- Layer 0: repo and global parse, round-trip, and reject an invalid agent key,
  an empty entry, or a non-map shape. `EffectiveConfig::agent_flags` covers
  precedence, per-agent replacement, the empty-list override, and CLI scoping
  to the selected agent.
- Engine: `build_options` emits configured flags only for the matching agent,
  a CLI list replaces config, and an empty CLI list emits nothing.
  `build_sandbox_options` emits flags for a `kind: agent` kit and a note for a
  mixin.
- Renderers: argv order for docker and builtin, including ACP, and for the sbx
  `run` argv.
- Workflows: a `RecordingRuntime` (refuses every launch and records its agent
  argv) runs real `exec workflow` invocations covering per-step agent
  entries, a worktree run whose checkout lacks the uncommitted config (gated
  on `AWMAN_TEST_ISOLATION` because it creates a worktree under `$HOME`), and
  workflow-agent vs step-override CLI targeting. `drive_leader_agent` is
  driven directly with a config-less worktree session, because the full
  `--dynamic` path needs a real image builder first. Removing either fix makes
  the worktree and leader tests fail; this was verified by mutation.
- Command layer: `parse_agent_flags`, workflow step scoping, and clap plus
  raw-args parsing of hyphen-leading `=` values on all three agent-run
  commands. The flag-order test and the generated command reference are
  updated.

## Codebase Integration:

- Follows the established conventions from the aspec: merge rules in
  `EffectiveConfig`, schema validation at load in Layer 0, flags declared once
  in the catalogue and derived elsewhere, and the agent matrix still owning
  awman's own flags.
- Follow-up (not done): `awman config get/set agentFlags.<agent>`, mirroring
  `dynamicWorkflows.agentsToModels.<agent>`. `config set`'s comma-splitting
  would mangle arguments that contain commas, so this needs its own design.

## Documentation

- `docs/07-configuration.md`: examples, precedence note, a "Pass extra flags to
  an agent" recipe, and reference-table rows.
- `docs/03-agent-sessions.md`: a `--agent-flag <ARG>` section.
- `docs/11-runtimes.md`: mixin-limitations table row.
- `docs/14-command-reference.md`: regenerated (`make docs-reference`).
