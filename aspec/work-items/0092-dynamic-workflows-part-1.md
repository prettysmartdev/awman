# Work Item: Feature

Title: Dynamic Workflows Part 1
Issue: issuelink

## Summary

Introduces `--dynamic` mode for `awman exec workflow`. Instead of the user providing a hand-crafted workflow file, a "leader" agent is launched inside a container to design a purpose-built `workflow.toml` tailored to the given work item. Once the leader agent becomes stuck (signalling it has finished), awman kills it, validates the file it produced, and immediately executes that workflow as if the user had run `exec workflow <path> --yolo --work-item <N>`.

Key flag rules:
- `--dynamic` is mutually exclusive with a positional workflow path argument
- `--dynamic` requires `--work-item`, or the command fails immediately
- `--dynamic` implies and enforces `--yolo`, `--worktree`, and `--overlay context(workflow)`
- `--leader agent::model` is an optional flag only valid alongside `--dynamic`; it fully specifies the container and model for the leader agent and takes precedence over `--model` for the leader
- If `--model` is passed without `--leader`, the default agent is used as the leader and `--model` is passed to it
- If both `--leader` and `--model` are passed, `--leader` governs the leader agent entirely; `--model` still applies to the generated workflow's steps as their default model (same as it does for any non-dynamic `exec workflow` invocation)
- `--leader` without `--dynamic` is an error

Execution order:
1. Validate all flags
2. Resolve work item content
3. Prepare worktree (because `--dynamic` implies `--yolo` which implies `--worktree`) — ALL subsequent steps work against the worktree
4. Resolve the workflow context directory (scope `workflow`, read-write)
5. Write two embedded static files to the context dir: `example-workflow.toml` and `workflow-usage.md`
6. Construct the leader prompt from the embedded `leader-prompt.md` template with runtime substitution of `{{work_item_number}}`, `{{work_item_path}}`, and `{{available_agents}}`
7. Launch the leader agent container with the constructed prompt
8. Leader runs through the standard stuck detection → 60-second yolo countdown → auto-advance pipeline (WCB available via `Ctrl+W` at any point, with `"Start dynamic workflow"` as the right-arrow label); kill the container on advance
9. Load and validate `workflow.toml` from the context dir (TOML structure + agent validation); if validation fails, re-launch the leader with the repair prompt containing the error — repeat up to 3 times before aborting
9a. Validate that every agent in the workflow has a `Dockerfile.<agent>` in the project; unknown agents are a validation error passed to the repair loop
9b. For agents with Dockerfiles but no built container image, build the missing images before starting the workflow
10. Execute the validated workflow through the normal workflow engine with `--yolo --work-item <N>` and the already-prepared worktree


## User Stories

### User Story 1
As a: user

I want to: run `awman exec workflow --dynamic --work-item 42` without writing a workflow file myself

So I can: have a code agent automatically design and execute a workflow custom-built for that specific work item, with no manual workflow authoring required.

### User Story 2
As a: user

I want to: optionally pass `--leader claude::claude-opus-4-8` to choose which agent container and model architects the workflow

So I can: use a more capable or specialized model for the planning step when it matters for complex work items.

### User Story 3
As a: user

I want to: have the leader agent's output automatically trigger workflow execution the moment it stops (i.e., becomes stuck)

So I can: walk away after launching the command and come back to a completed workflow run — no manual follow-up needed.


## Implementation Details

### 1. New CLI Flags (`src/command/dispatch/catalogue.rs`)

Add two new flags to the `exec workflow` subcommand:

- **`--dynamic`**: bool flag, default `false`. Conflicts with the positional workflow path argument. Implies `--yolo`, `--worktree`, and appends `context(workflow)` to the overlay list.
- **`--leader`**: optional string flag, format `agent::model` (e.g. `claude::claude-opus-4-8`). Only valid when `--dynamic` is set; error if provided without `--dynamic`. When provided, takes full precedence over `--model` for the leader agent; `--model` still applies to the generated workflow's steps as their session-level default.

### 2. `ExecWorkflowCommandFlags` Updates (`exec_workflow.rs:39-52`)

Add fields:
```rust
pub dynamic: bool,
pub leader: Option<String>,  // raw "agent::model" string
```

Add a parsed helper type:
```rust
pub struct LeaderSpec {
    pub agent: String,
    pub model: String,
}
```

Parse `--leader` into `LeaderSpec` by splitting on `::`. Both components must be non-empty; a missing `::` or empty component is a hard validation error surfaced before any container work begins.

`LeaderSpec` is only constructed when `--leader` is explicitly provided. When `--leader` is absent, the leader's agent and model are derived separately (see Section 7).

### 3. Validation (early, before any IO)

In the command's entry point, before worktree setup:
- `dynamic && workflow_path_provided` → error: "cannot specify a workflow file path with --dynamic; the path is created automatically"
- `dynamic && work_item.is_none()` → error: "--dynamic requires --work-item"
- `leader.is_some() && !dynamic` → error: "--leader is only valid with --dynamic"
- Malformed `--leader` value (no `::`, empty agent, empty model) → error with format hint

### 4. Implied Flags Enforcement

When `dynamic` is `true`, the effective flags at runtime must be:
- `yolo = true` (regardless of whether the user passed `--yolo`)
- `worktree = true` (regardless of whether the user passed `--worktree`)
- `overlay` list gains `"context(workflow)"` if not already present

This enforcement happens right after flag validation, before worktree setup, so all downstream code sees the correct values without special-casing.

### 5. Worktree Setup Order

Because `--dynamic` implies `--yolo` which implies `--worktree`, run the `WorktreeLifecycle` setup steps (checkout, branch) **before** launching the leader agent. This mirrors the normal `--yolo --worktree` path (see `exec_workflow.rs:624-724`) and ensures the leader agent operates on the isolated worktree.

### 6. Embedded Static Assets

Create a new module `src/data/dynamic_workflow_assets.rs` (or embed in an `assets/` subdir) with:

```rust
pub const EXAMPLE_WORKFLOW_TOML: &str = include_str!("../../assets/dynamic/example-workflow.toml");
pub const WORKFLOW_USAGE_MD: &str = include_str!("../../assets/dynamic/workflow-usage.md");
pub const LEADER_PROMPT_MD: &str = include_str!("../../assets/dynamic/leader-prompt.md");
pub const LEADER_REPAIR_PROMPT: &str = include_str!("../../assets/dynamic/leader-repair-prompt.md");
```

The source files live at `src/assets/dynamic/` and are checked into the repository. Their candidate versions are created alongside this work item (`0092-example-workflow.toml`, `0092-workflow-usage.md`, `0092-leader-prompt.md`, and `0092-leader-repair-prompt.md`) for review before being placed at their final paths.

After resolving the workflow context directory (the `context(workflow)` overlay), write both reference files there:
- `<context_dir>/example-workflow.toml`
- `<context_dir>/workflow-usage.md`

Overwrite unconditionally; these are always regenerated from the embedded binary content.

The leader prompt template (`LEADER_PROMPT_MD`) is not written to the context directory — it is used in code to construct the pre-seeded prompt (Section 7) with runtime substitution of `{{work_item_number}}`, `{{work_item_path}}`, and `{{available_agents}}`.

### 7. Leader Agent Launch

Reuse the existing agent container launch infrastructure (`CommandLayerFactory`, `agent_image_tag()`, Docker/Apple backend). The leader is a single-step execution, not a full workflow run.

**Leader agent selection precedence:**
1. `--leader agent::model` provided → use `LeaderSpec.agent` for image tag and `LeaderSpec.model` as the model; `--model` is ignored for the leader
2. `--model` provided, no `--leader` → use the repo's configured default agent for image tag; use the `--model` value as the leader's model
3. Neither → use the repo's configured default agent for image tag; no model override

Note: `--model` continues to apply to the generated workflow's agent steps as the session-level default model — exactly as it does today for any `exec workflow` invocation. It is only the leader-specific model selection above that changes based on whether `--leader` is also present.

**Pre-seeded prompt**: defined in `0092-leader-prompt.md` (embedded as a static asset alongside the example workflow and usage docs). The prompt is constructed in code with runtime template substitution for `{{work_item_number}}`, `{{work_item_path}}`, and `{{available_agents}}`.

**Overlays for the leader container:**
- Worktree mount (read-write, because the leader needs to understand the codebase to design a good workflow — it may need to read source files)
- `context(workflow)` read-write (so it can write `workflow.toml`)

`--yolo` is enforced, so the leader container runs in yolo mode. This is what triggers stuck-detection → yolo countdown → auto-advance through the standard codepath.

### 8. Stuck Detection, Yolo Countdown, and WCB for the Leader

The leader agent container uses the **exact same** stuck detection → yolo countdown → WCB pipeline as a regular workflow step. No custom timeout loop, no special stuck handling — the leader is run through `step_once_interruptible()` (or equivalent single-step execution that reuses the same `io_bridge` stuck detector, `StuckEvent` subscription, `handle_step_stuck()`, and `run_mid_step_yolo_countdown()` codepaths).

**Stuck detection:** The `io_bridge` stuck detector fires `StuckEvent::Stuck` after 30 seconds of inactivity, just like any workflow step.

**Yolo countdown:** Because `--yolo` is enforced, `handle_step_stuck()` enters the 60-second yolo countdown (`YOLO_COUNTDOWN_DURATION`). During the countdown:
- The TUI frontend shows the standard yolo countdown tick (remaining seconds)
- The CLI frontend shows the standard overlay: `yolo: auto-advancing in Xs [n] now [a] abort [p] pause`
- The user can press `Ctrl+W` to open the full Workflow Control Board
- `StuckEvent::Unstuck` cancels the countdown (leader resumed output)
- If the leader's container exits (step completes) during the countdown, the countdown ends and proceeds to file validation

**Auto-advance on expiry:** When the 60-second countdown expires (or the user presses `[n]`), the leader container is killed and the engine proceeds to validate `workflow.toml`.

**Workflow Control Board during leader step:** The WCB is available both during the leader's active execution (via `Ctrl+W`) and during the yolo countdown (via `Ctrl+W`). The WCB for the leader step has one key difference from a regular workflow step:

- **Right arrow (`→`) label**: `"Start dynamic workflow"` instead of `"Next: new container"`. This is the advance/auto-advance action — it kills the leader container and proceeds to validate and execute the generated `workflow.toml`.

All other WCB actions work identically to a regular workflow step:
- **`↑` Restart current step** — kills the leader container, re-launches it with the same prompt
- **`[^C]` Abort** — kills the leader container, aborts the entire dynamic workflow invocation
- **`[p]` Pause / `[Esc]` Pause** — kills the leader container, pauses (user can resume later)
- **`[Esc]` Dismiss** (when container is running) — closes the WCB without affecting the leader
- **`←` Cancel to prev** — unavailable (there is no previous step; reason: `"this is the first step"`)
- **`↓` Next: same container** — unavailable (there is no "next step" in the workflow-step sense; reason: rendered as appropriate unavailable text)
- **`[Enter]` Finish workflow** — unavailable (the workflow hasn't started yet)

**Implementation approach for the label change:**

Add a `launch_next_label` field to `WorkflowControlBoardState`:
```rust
pub struct WorkflowControlBoardState {
    // ... existing fields ...
    /// Custom label for the right-arrow action. Defaults to "Next: new container"
    /// when `None`. Set to `Some("Start dynamic workflow")` for the leader step.
    pub launch_next_label: Option<String>,
}
```

In the TUI renderer (`render.rs`), use `state.launch_next_label.as_deref().unwrap_or("Next: new container")` for the right-arrow label text. The engine's dynamic pre-flight sets this field when constructing `AvailableActions` / `WorkflowControlBoardState` for the leader step. Alternatively, add the label to `AvailableActions` so both TUI and CLI frontends can use it.

For the CLI frontend's yolo countdown overlay, the label change is not needed — the `[n] now` shorthand is sufficient and already conveys "advance past this step."

**Post-advance: file validation, agent validation, and repair loop:**

After the leader step is advanced (whether by yolo countdown expiry, user pressing `[n]`, or user pressing `→` in the WCB):

1. Resolve `<context_dir>/workflow.toml`
2. If the file does not exist → surface error: "leader agent did not produce workflow.toml at `<path>`; cannot continue"
3. Attempt `Workflow::load(&path)` — if parse fails → enter the repair loop (Section 9)
4. If TOML is structurally valid → validate all agents referenced in the workflow (Section 9a)
5. If all agents are valid → build any missing agent images (Section 9b), then proceed to step 10

### 9. Workflow File Repair Loop

When `Workflow::load()` fails on the leader's output, the engine does not abort immediately. Instead, it launches a **repair agent** — the same leader agent container and model — with a second embedded prompt instructing it to fix the file. This retry loop runs up to **3 times** before giving up.

**Repair prompt template** (embedded as a static asset, `LEADER_REPAIR_PROMPT`):

```
The workflow file you produced is not valid. Your only task is to fix it.

File path:
    /context/workflow/workflow.toml

Error:
    {{validation_error}}

Reference:
    /context/workflow/workflow-usage.md — complete workflow format documentation

Rules:
  1. Read the error message above carefully
  2. Open /context/workflow/workflow.toml and fix the problem
  3. The file must be valid TOML that conforms to the format in workflow-usage.md
  4. Do not modify any other files
  5. When you have finished fixing the file, stop
```

`{{validation_error}}` is substituted at runtime with the verbatim error string from `Workflow::load()` (the TOML parse error or structural validation error).

**Repair loop flow:**

```
attempt = 0
loop:
    result = Workflow::load(<context_dir>/workflow.toml)
    if result is valid:
        proceed to step 10
    attempt += 1
    if attempt > 3:
        surface final error: "leader agent failed to produce a valid workflow.toml
            after 3 repair attempts; last error: <error>; file is at <path>"
        abort dynamic workflow
    log warning: "workflow.toml validation failed (attempt {attempt}/3): <error>"
    launch repair agent with LEADER_REPAIR_PROMPT ({{validation_error}} = <error>)
    run repair agent through the same stuck → yolo countdown → WCB pipeline as the leader
    goto loop
```

The repair agent container:
- Uses the same agent and model as the leader (resolved from `--leader` or defaults, per Section 7)
- Has the same overlays: worktree mount (read-write) and `context(workflow)` (read-write)
- Runs in `--yolo` mode with the standard 60-second yolo countdown
- Shows the same WCB with `"Start dynamic workflow"` as the right-arrow label
- Supports restart, abort, pause, dismiss — identical to the leader step

**Embedded asset:**

Add to `src/data/dynamic_workflow_assets.rs`:
```rust
pub const LEADER_REPAIR_PROMPT: &str = include_str!("../../assets/dynamic/leader-repair-prompt.md");
```

The source file lives at `src/assets/dynamic/leader-repair-prompt.md` and contains the raw prompt template above. A candidate version is created alongside this work item (`0092-leader-repair-prompt.md`) for review.

### 9a. Agent Validation

After `Workflow::load()` succeeds (the TOML is structurally valid), validate that every agent referenced in the workflow has a corresponding `Dockerfile.<agent>` in the project. This catches the case where the leader agent invents an agent name that doesn't exist.

**Agent collection:** Collect the set of all unique agent names from the parsed `Workflow`:
- `workflow.agent` (the workflow-level default, if set)
- Each `step.agent` (per-step overrides, if set)

For each agent in the set, check whether `RepoDockerfilePaths::agent_dockerfile(agent)` exists on disk (i.e. `.awman/Dockerfile.<agent>` is present). If any agent has no corresponding Dockerfile, this is a validation error — the workflow references an agent that the project cannot build.

**Error handling:** Agent validation failures are treated identically to TOML parse failures — the error is passed to the repair loop (Section 9). The error message should list all invalid agents and the expected Dockerfile path pattern:

```
workflow.toml references agents with no Dockerfile in the project:
  - "codex" (expected .awman/Dockerfile.codex)
  - "gemini" (expected .awman/Dockerfile.gemini)
Available agents: claude, maki
```

This gives the leader agent enough information to fix the workflow file by replacing the unknown agents with available ones.

**Validation order:** Agent validation runs after `Workflow::load()` succeeds but before the missing-image build step (Section 9b). The repair loop handles both TOML errors and agent errors — each iteration re-validates from scratch (parse → agent check), so a repair attempt that fixes the TOML but introduces a bad agent name is caught on the next iteration.

### 9b. Build Missing Agent Images

After all agents in the workflow are validated (every referenced agent has a `Dockerfile.<agent>`), check whether a built container image exists for each agent. If any images are missing, build them before starting the workflow.

**Image check:** For each unique agent name in the workflow, compute the expected image tag via `agent_image_tag(&git_root, agent)` and check `container_runtime.image_exists(&tag)`. Collect all agents whose images do not exist.

**Build missing images:** For each missing agent image, build it using the existing `container_runtime.build_image()` codepath — the same one used by `ReadyEngine` when `--build` is set (see `ready/mod.rs:359-397`):

```rust
let tag = agent_image_tag(&git_root, &agent_name);
let dockerfile_path = paths.agent_dockerfile(&agent_name);
container_runtime.build_image(&tag, &dockerfile_path, &git_root, no_cache, &mut sink)?;
```

Report build progress through the frontend (TUI/CLI) the same way `ReadyEngine` does — per-agent status lines showing `Running` → `Done` or `Failed`.

**Build failure:** If any agent image fails to build, this is a hard error — surface the build error and abort the dynamic workflow. Do not enter the repair loop for build failures; the problem is in the project's Dockerfiles, not in the workflow file the leader produced. The error message should name the agent, the Dockerfile path, and the build error.

**This step is not part of the repair loop.** It only runs after agent validation has passed — meaning every agent in the workflow has a Dockerfile. The repair loop handles "unknown agent" errors; this step handles "known agent, not yet built." These are distinct failure modes with different remediation paths (fix the workflow file vs. fix the Dockerfile/build environment).

### 10. Workflow Execution

After the leader's `workflow.toml` is validated, all agents are confirmed, and all required images are built, execute the workflow through the standard workflow engine as if the user had run:

```
exec workflow <context_dir>/workflow.toml --yolo --work-item <N> --overlay context(workflow)	
```

All worktree pre- and post-workflow steps (setup, teardown) defined in the generated `workflow.toml` run normally. The worktree is already set up from step 3; teardown (commit, push, PR) runs against it as usual.

This reuses the existing `ExecWorkflowCommand::run(...)` internals — the dynamic path is a pre-flight that produces the workflow file and then falls through to the same engine.


## Edge Case Considerations

- **Leader produces no file**: enter the repair loop (Secion 9) - launch leader with repair prompt and the missing file error surfaced in the prompt.
- **Leader produces invalid TOML**: enters the repair loop (Section 9) — the leader agent is re-launched with a repair prompt containing the validation error, up to 3 times; if all attempts fail, the final error and file path are surfaced so the user can inspect and manually fix
- **Repair agent produces a different error**: each repair attempt re-validates from scratch; the new error is substituted into the next repair prompt
- **Repair agent deletes the file instead of fixing it**: treated the same as "leader produces no file" — error with expected path, no further repair attempts
- **Leader produces valid TOML but references unknown agent names** (no Dockerfile): caught by agent validation (Section 9a) before the workflow starts; the error listing unknown agents and available agents is passed to the repair loop so the leader can fix it
- **Leader references an agent with a Dockerfile but no built image**: caught by Section 9b; the missing image is built automatically — this is not a repair-loop error
- **Agent image build fails**: hard error — abort the dynamic workflow with the build error; do not enter the repair loop (the problem is the Dockerfile, not the workflow file)
- **Workflow uses only the workflow-level `agent` with no per-step overrides**: agent validation checks the workflow-level agent; if it's invalid, the error is passed to the repair loop
- **Workflow sets no agent at all** (relies on repo config default): agent validation skips it — the config default is assumed valid since it was already verified during `ReadyEngine` startup
- **Repair attempt fixes TOML but introduces a bad agent name**: caught on the next iteration — the repair loop re-validates from scratch (parse → agent check) each time
- **`--leader` format wrong** (e.g. `claude` without `::`, or `::model`, or `agent::`): validate eagerly before any container work; error with format hint `agent::model (e.g. claude::claude-opus-4-8)`
- **`--dynamic` with positional path**: mutually exclusive; error immediately with a clear message
- **`--dynamic` without `--work-item`**: error immediately — the leader prompt and worktree naming both require a work item number
- **`--leader` without `--dynamic`**: error immediately
- **Context dir already contains a `workflow.toml` from a previous run**: the seed files (`example-workflow.toml`, `workflow-usage.md`) are overwritten unconditionally; the existing `workflow.toml` is left in place until the leader overwrites it — no special handling needed since the leader is expected to write a fresh one
- **Leader container crashes before writing file**: treat the same as stuck (container gone, check file presence, error if missing)
- **User restarts leader step via WCB**: kills the current leader container, re-launches a fresh one with the same pre-seeded prompt; any previously written `workflow.toml` in the context dir is left in place (the leader will overwrite it)
- **User aborts during leader yolo countdown**: kills the leader container, aborts the entire `--dynamic` invocation — no workflow is executed
- **User pauses during leader step**: kills the leader container, pauses execution; resume semantics follow the standard workflow pause/resume path
- **Leader becomes unstuck during yolo countdown**: countdown is cancelled, leader continues running; stuck detection resumes normally
- **Work item file not found**: follow the existing `exec workflow` behavior — warn and continue with empty substitutions; the leader will have an empty `{{work_item_content}}` but will still attempt to produce a file
- **Worktree creation fails**: surface the error and abort before touching the context dir or launching the leader
- **Context dir creation fails** (permissions, disk full): surface the OS error, abort
- **`--dynamic` + `--plan`**: `--dynamic` implies `--yolo` which already conflicts with `--plan`; the existing yolo/plan conflict check catches this
- **`--leader` + `--model` together**: `--leader` takes full precedence for the leader agent's model; `--model` is silently ignored for the leader but continues to apply as the session-level default model for the generated workflow's steps — this is intentional and requires no error or warning


## Test Considerations

- **Unit — flag validation**: each mutual exclusion (dynamic+path, dynamic without work-item, leader without dynamic) triggers the correct error
- **Unit — `LeaderSpec` parsing**: `"claude::claude-opus-4-8"` parses correctly; `"claude"`, `""`, `"::model"`, `"agent::"`, `"a::b::c"` all error
- **Unit — implied flags**: when `dynamic=true`, effective flags have `yolo=true`, `worktree=true`, `context(workflow)` in overlays
- **Unit — leader prompt construction**: substituted prompt contains the correct context dir paths, work item number, and work item content
- **Unit — embedded assets**: `EXAMPLE_WORKFLOW_TOML` parses as a valid `Workflow` via `Workflow::parse()`; `WORKFLOW_USAGE_MD` is non-empty
- **Unit — leader model selection**: (a) `--leader` present → leader uses `LeaderSpec` agent+model, `--model` ignored for leader; (b) `--model` present, no `--leader` → default agent used, `--model` value passed to leader; (c) neither → default agent, no model override; (d) both `--leader` and `--model` → leader uses `--leader`'s model, `--model` still surfaces as session default for workflow steps
- **Integration — happy path**: mock leader writes a minimal valid `workflow.toml` to the context dir → workflow engine launches with that file
- **Integration — missing file**: mock leader writes nothing → error message contains expected path
- **Integration — invalid TOML, repair succeeds**: mock leader writes malformed TOML → repair agent launched with error in prompt → repair agent fixes file → workflow proceeds
- **Integration — invalid TOML, repair exhausted**: mock leader and all 3 repair attempts produce invalid TOML → final error surfaced with path, workflow aborts
- **Integration — repair prompt substitution**: the `{{validation_error}}` in the repair prompt contains the verbatim error from `Workflow::load()`
- **Integration — unknown agent triggers repair**: workflow.toml references `"gemini"` but no `.awman/Dockerfile.gemini` exists → agent validation error passed to repair loop → repair agent replaces with a valid agent → workflow proceeds
- **Integration — unknown agent, repair exhausted**: all repair attempts keep referencing unknown agents → final error lists unknown agents and available agents, workflow aborts
- **Integration — missing image auto-build**: workflow.toml references `"codex"`, `.awman/Dockerfile.codex` exists but no built image → image is built automatically before workflow starts
- **Integration — image build failure aborts**: workflow.toml references `"codex"`, Dockerfile exists, but build fails → hard error with build output, workflow aborts (no repair loop)
- **Integration — mixed agent issues**: workflow has one unknown agent (no Dockerfile) and one unbuilt agent (Dockerfile exists) → unknown agent caught first by validation, passed to repair loop; unbuilt agent built after repair succeeds
- **Integration — no per-step agents, workflow-level only**: workflow sets `agent = "badname"` with no per-step overrides → agent validation catches it, passes to repair loop
- **Integration — stuck triggers yolo countdown**: leader container emits `StuckEvent::Stuck` → 60-second yolo countdown starts → on expiry, container killed → `workflow.toml` loaded → workflow executed
- **Integration — yolo countdown unstuck recovery**: leader emits `Stuck`, countdown starts, leader emits `Unstuck` → countdown cancelled, leader continues running
- **Integration — WCB label**: when WCB is shown for the leader step, the right-arrow label is `"Start dynamic workflow"`, not `"Next: new container"`
- **Integration — WCB restart leader**: user opens WCB during leader, selects restart → leader container killed, fresh leader container launched with same prompt
- **Integration — WCB abort during leader**: user opens WCB during leader or yolo countdown, selects abort → leader killed, entire dynamic invocation aborts
- **Integration — worktree before leader**: assert that `WorktreeLifecycle` setup steps complete before the leader container is launched
- **E2E — full dynamic flow**: `awman exec workflow --dynamic --work-item 42` in a test repo with a real (or stubbed) leader agent produces and executes a workflow


## Codebase Integration

- Follow the `ExecWorkflowCommandFlags` struct pattern at `exec_workflow.rs:39-52` for the two new fields
- Validate `--leader` format in the same early-validation block where other flag conflicts are checked (before any `async` IO begins)
- For implied-flags enforcement, set `flags.yolo = true`, `flags.worktree = true`, and append `"context(workflow)"` to `flags.overlay` immediately after validation — keep it in one place so there is no drift
- Embed assets via `include_str!()` in a dedicated `src/assets/dynamic/` source tree; add the module to `src/data/mod.rs`
- Reuse `WorktreeLifecycle` (see `exec_workflow.rs:624-724`) — do not duplicate worktree logic
- Reuse `resolve_context_overlays()` (see `exec_workflow.rs:332-344`) to get the context dir host path before writing seed files
- Reuse `agent_image_tag()` and the existing model flag injection for leader container launch
- Reuse `io_bridge` stuck event channel for leader monitoring — do not roll a custom timeout loop
- Reuse `handle_step_stuck()` → `run_mid_step_yolo_countdown()` for the leader's stuck → yolo countdown → auto-advance flow; the leader step must go through the same `step_once_interruptible()` codepath (or equivalent) as a regular workflow step
- Reuse `show_workflow_control_board()` and `compute_available_actions()` for the leader step's WCB — add `launch_next_label: Option<String>` to `WorkflowControlBoardState` (and optionally to `AvailableActions`) so the dynamic pre-flight can set the right-arrow label to `"Start dynamic workflow"` without forking the rendering code
- Reuse `Workflow::load()` (`data/workflow_definition.rs:284-289`) to validate the generated file
- Reuse `RepoDockerfilePaths::discover_agent_dockerfiles()` and `RepoDockerfilePaths::agent_dockerfile()` for agent validation — check that every agent referenced in the parsed workflow has a corresponding `Dockerfile.<agent>` in `.awman/`
- Reuse `agent_image_tag()` + `container_runtime.image_exists()` + `container_runtime.build_image()` for the missing-image build step — same pattern as `ReadyEngine`'s `--build` flow (`ready/mod.rs:359-397`)
- The dynamic pre-flight (steps 1–9b) should live in a clearly named async function `run_dynamic(...)` in `exec_workflow.rs` or a new `exec_workflow_dynamic.rs`; it returns the validated `Workflow` and context dir path, then the caller falls through to the existing workflow execution path


## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Create `docs/13-dynamic-workflows.md`** as a new user guide describing `exec workflow --dynamic`, the `--leader` flag, what the leader agent does, and what to expect from the full flow
- **Update `docs/05-workflows.md`** to mention dynamic workflows and cross-reference the new guide
- **Never create work-item-specific docs** (e.g., no "WI 0092 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
