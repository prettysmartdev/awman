# Work Item: Feature

Title: agent flag for chat, implement, workflows
Issue: issuelink

## Summary:
- Add `--agent <name>` flag support to `chat` and `implement` commands so the user can override the configured agent at invocation time without editing `config.json`.
- When the requested agent's Dockerfile (`.amux/Dockerfile.<agent>`) is missing, prompt the user to download and build it; if the user declines, bail out.
- Add an optional `Agent:` field to workflow step definitions so each step can run in a different containerized agent. Before a workflow starts, validate all required agent images and prompt for missing ones; offer fallback to the default agent if the user declines setup.
- When transitioning between workflow steps that require different agents, disable the "continue in same container" option in the workflow control dialog with an explanatory message.

## User Stories

### User Story 1:
As a: user

I want to: pass `--agent codex` (or any supported agent name) to `amux chat` or `amux implement` and have the session run inside that agent's container instead of the one set in `config.json`

So I can: experiment with different agents on a per-invocation basis without permanently changing my repo configuration.

### User Story 2:
As a: user

I want to: specify `Agent: <name>` per step in a workflow file so each step runs in the most appropriate containerized agent, and have amux validate that all required images are available before the workflow starts

So I can: build multi-agent workflows where different steps leverage different AI coding assistants, with a clear setup prompt when a new agent needs to be installed.

### User Story 3:
As a: user

I want to: be guided through downloading and building a new agent Dockerfile when I request an agent that has no `.amux/Dockerfile.<agent>` file, and be offered a fallback to the default agent if I decline when running a workflow, or bail out when running `chat` or `implement` with no workflow.

So I can: easily onboard new agents or gracefully continue with my existing setup if I choose not to install a new one.

## Implementation Details:

### 1. Workflow file parser (`src/workflow/parser.rs`)

- Add `agent: Option<String>` field to `WorkflowStep`.
- Parse a new `Agent:` field in the step header block (alongside `Depends-on:` and before `Prompt:`).
  - Example syntax:
    ```
    ## Step: implement
    Depends-on: plan
    Agent: codex
    Prompt: Implement the plan.
    ```
- `Agent:` is optional; omitting it means "use the default agent" (from config or `--agent` flag if passed).
- Validate parsed agent names using `cli::validate_agent_name()` during `load_workflow_file()`.

### 2. Workflow state (`src/workflow/mod.rs`)

- Add `agent: Option<String>` field to `WorkflowStepState` (serialized to JSON for persistence/resume).
- Add `agent: Option<String>` field to `WorkflowStep` (to propagate through `WorkflowState::new()`).
- Update `WorkflowState::new()` to copy `agent` from parsed steps into `WorkflowStepState`.
- `validate_resume_compatibility()` does not need to check `agent` (it is a runtime hint, not a structural dependency).

### 3. Agent setup helper (`src/commands/agent.rs`)

- Extract a new async function `ensure_agent_available(git_root, agent_name, out, ask_fn) -> Result<bool>`:
  - Returns `true` if the agent is ready (Dockerfile + image exist or were just built).
  - Returns `false` if the user declined setup.
  - Logic:
    1. Check if `.amux/Dockerfile.<agent>` exists.
    2. If missing, call `ask_fn` (the Q&A closure) to ask whether to download and build it.
    3. If the user accepts: download the canonical agent Dockerfile (URL from a per-agent constant table, pulling from GitHub amux repo templates folder), save it to `.amux/Dockerfile.<agent>`, then build the image using the existing `runtime.build_image()` path.
    4. If the user declines: return `false`.
  - Use `out.println()` for status messages (e.g. "Downloading Dockerfile.codex…", "Building amux-<project>-codex:latest…").

### 4. CLI/TUI `--agent=<name>` OR `--agent <name>`  flag for `chat` and `implement`

- The `--agent` CLI flag already exists in both command definitions (`cli.rs` lines ~103-104 and ~138-139) and is threaded through as `agent_override: Option<String>`.
- In `run_agent_with_sink()` (`agent.rs`), after resolving the effective agent name, call `ensure_agent_available()` before launching the container. If it returns `false`, return early (bail out).
- For **CLI mode** (stdin-based prompts), the `ask_fn` passed to `ensure_agent_available()` should use the existing `print!()` + `stdin.lock().lines()` pattern (e.g. `"Agent 'codex' has no Dockerfile. Download and build it? [y/N]: "`).
- For **TUI mode**, a new `Dialog` variant `AgentSetupConfirm { agent: String }` should be added to handle the yes/no prompt inline with the TUI event loop. Ensure TUI flag parsing accepts flag with or without `=`
- if the requested agent is not available AND the user declines to download/build it in-situ, BAIL OUT for both CLI and TUI with a helpful error message.

### 4.5 `ready --build` behaviour
- if the user runs `ready --build` in CLI or TUI and multiple `.amux/Dockerfile.{agent}` files exist, ASK the user (using Q&A traits for CLI and TUI) if the user wants to build ONLY the default agent's image, or if they want to build all agent images. If they decline to build all, only build the agent image listed in contig.json. When ASKING the user, print the list of which agents are present and label them as default/extra so the user knows which will not be built if they decline.
- the `--no-cache` flag should extend to all builds during `ready`, including the project docker image and all agent images
- for ALL docker builds in ready, make the info lines stating when builds are starting more prominent (add some empty lines around them and add some ASCII flair or colored text to make it very obvious when each build starts and ends, regardless of how many agents, or which flags are passed.

### 5. Workflow pre-flight check (`src/commands/implement.rs`)

In `run_workflow()`, before entering the main step loop:

1. Collect the set of distinct agent names required across all steps:
   - For each step, the effective agent is `step.agent.as_deref().unwrap_or(effective_default_agent)`.
2. For each required agent, call `ensure_agent_available()`.
   - If any agent returns `false` (user declined), ask a follow-up:
     `"Use the default agent (<name>) for steps that specify '<missing-agent>'? [y/N]: "`
     - If yes: substitute the default agent for those steps at runtime.
     - If no: abort the workflow.
3. Store the resolved per-step agent map in a local `HashMap<&str, String>` (step name → effective agent) for use during execution.

### 6. Per-step agent execution in `run_workflow()`

- When executing each step, look up the resolved agent for that step from the map built in the pre-flight check.
- Pass the resolved agent name as `agent_override: Some(agent_name)` to `run_agent_with_sink()`.
- Do not reuse `agent_override` from the CLI flag as a blanket override for all steps; only apply the CLI flag as the default when a step has no explicit `Agent:` field.

### 7. Workflow control dialog: "next step in same container" restriction

- In the TUI `WorkflowControlBoard` dialog and the `WorkflowStepConfirm` dialog, when determining whether to offer the "continue in same container" option:
  - Resolve the agent for the current step and the next step(s) using the same per-step agent map.
  - If they differ, render the option as **disabled/greyed** (or skip rendering it) and display an explanatory message, e.g.:
    `"Next step uses agent 'codex'; cannot reuse current 'claude' container."`
- In CLI mode, simply skip offering the "same container" option and print the explanation instead.
- The relevant TUI handling is in `src/tui/state.rs` around the `WorkflowStepConfirm` and `WorkflowControlBoard` variants (lines ~128-147).

### 8. Workflow state persistence

- `WorkflowStepState` gains `agent: Option<String>`. This is a new JSON field; existing state files without it will deserialize with `None` (use `#[serde(default)]`).
- When resuming a workflow with an existing state file, the `agent` field in the state takes precedence (it was resolved at the time the workflow was started).

## Edge Case Considerations:

- **Unknown agent name at parse time**: `load_workflow_file()` should call `validate_agent_name()` on any `Agent:` field values and return an error before the workflow state is created.
- **`--agent` flag + workflow with per-step agents**: The `--agent` flag acts as the default for steps that omit `Agent:`. It does NOT override steps that explicitly specify an agent.
- **All steps use a non-default agent**: The pre-flight check still runs; the "use default agent" fallback question is offered only if the user declined to set up an agent that at least one step requires.
- **Dockerfile download failure**: If the HTTP fetch for the agent Dockerfile fails, print a clear error and return `false` from `ensure_agent_available()` so the caller can handle it gracefully.
- **Image build failure**: If `runtime.build_image()` fails after downloading the Dockerfile, surface the error and return `false`; do not leave a partial Dockerfile in `.amux/`.
- **Resuming a workflow with a different `--agent` flag**: On resume, the persisted `agent` per step in the state JSON is used. Warn the user if the CLI `--agent` flag differs from the persisted default.
- **Parallel steps with different agents**: Not blocked — each step spawns its own container. No cross-step container sharing is affected.
- **TUI launch path** (`tui/mod.rs` `launch_implement()` / `launch_chat()`): These currently pass no `agent_override`. The TUI does not expose an agent-selection UI. For now, TUI uses config default; the `AgentSetupConfirm` dialog handles the missing-Dockerfile prompt during a TUI-initiated workflow or chat session.
- **Legacy Dockerfile layout** (no `.amux/Dockerfile.<agent>`, falls back to `Dockerfile.dev`): `ensure_agent_available()` should treat the fallback path as "available" (no prompt needed) when the requested agent equals the configured default and `Dockerfile.dev` exists.

## Test Considerations:

- **Parser unit tests** (`src/workflow/parser.rs`):
  - `parse_workflow` with `Agent:` field populates `WorkflowStep.agent`.
  - `parse_workflow` without `Agent:` field gives `agent: None`.
  - `Agent:` after `Prompt:` is treated as prompt body (not a directive).
  - Invalid agent name in `Agent:` field returns an error from `load_workflow_file()`.

- **`WorkflowState` unit tests** (`src/workflow/mod.rs`):
  - `WorkflowState::new()` propagates `agent` from `WorkflowStep` to `WorkflowStepState`.
  - Serialize/deserialize round-trip preserves `agent` field.
  - Old state JSON without `agent` field deserializes without error (`serde(default)`).

- **`ensure_agent_available()` unit tests** (`src/commands/agent.rs`):
  - When Dockerfile exists: returns `true` without calling `ask_fn`.
  - When Dockerfile missing and user accepts: downloads file, builds image, returns `true`.
  - When Dockerfile missing and user declines: returns `false`, no side effects.
  - When download fails: returns `false` with error surfaced.

- **Workflow pre-flight integration tests** (`src/commands/implement.rs`):
  - All agents available: workflow proceeds immediately.
  - One agent missing, user accepts setup: workflow proceeds after build.
  - One agent missing, user declines setup, user accepts default fallback: workflow runs all steps with default agent.
  - One agent missing, user declines both: workflow does not start.

- **"Same container" disabled tests** (TUI state unit tests in `src/tui/state.rs`):
  - When current step and next step have the same agent: option is enabled.
  - When current step and next step have different agents: option is disabled; message is set.

- **End-to-end tests**:
  - `amux chat --agent codex` with `Dockerfile.codex` present: launches codex container.
  - `amux implement --agent opencode` with missing Dockerfile, user accepts: Dockerfile downloaded and image built before launch.
  - Workflow with mixed agents: correct per-step agent containers are launched in order.

## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The `--agent` CLI flag already exists in `src/cli.rs` and is wired through `agent_override: Option<String>` in both `chat::run()` and `implement::run()` — no new CLI plumbing needed.
- Agent image resolution lives in `src/commands/agent.rs` (`resolve_agent_image_and_dockerfile()`); extend this file with `ensure_agent_available()`.
- The existing stdin prompt pattern (`print!()` + `stdin.lock().lines()`) is used throughout `src/commands/implement.rs`; follow the same style for new CLI prompts (no trait abstraction needed).
- TUI dialog variants are defined in `src/tui/state.rs`; add `AgentSetupConfirm` there and handle it in the TUI event loop following the pattern of existing yes/no dialogs (e.g. `ClawsReadyDockerSocketWarning`).
- `WorkflowStep` and `WorkflowStepState` are in `src/workflow/parser.rs` and `src/workflow/mod.rs` respectively; add the `agent` field to both and update all construction sites.
- Known agent names are in `KNOWN_AGENT_NAMES` in `src/cli.rs`; the `validate_agent_name()` function there is already used in `run_agent_with_sink()` and should be reused during workflow file parsing.
- Per-agent Dockerfile download URLs should be defined as a constant table (e.g. `static AGENT_DOCKERFILE_URLS: &[(&str, &str)]`) in `src/commands/agent.rs`, co-located with `resolve_agent_image_and_dockerfile()`.
- `#[serde(default)]` must be added to the new `agent` field in `WorkflowStepState` to maintain backwards compatibility with existing persisted state files.
