# Work Item: Task

Title: grand architecture refactor — Layer 3 CLI frontend + Layer 4 binary; swap entrypoint
Issue: n/a — fourth-of-seven work item implementing `aspec/architecture/2026-grand-architecture.md`

> **Scope note (post-split):** the original 0069 bundled CLI, TUI, and Headless frontends. That proved too large to land in a single pass, so the bundle was split:
>
> - `0069-…` (this work item) — CLI frontend + Layer 4 binary + `Cargo.toml` swap.
> - `0070-grand-architecture-tui-frontend.md` — TUI frontend.
> - `0071-grand-architecture-headless-frontend.md` — Headless frontend.
> - `0072-grand-architecture-finalize-and-remove-oldsrc.md` — Final parity validation, oldsrc removal, docs and aspec refresh.
>
> The TUI and Headless sections (§2, §3, §7a–§7u) below are **out of scope for this work item** but kept inline as historical context for 0070/0071. The CLI section (§1) and the Layer 4 / `Cargo.toml` sections (§4, §5) are the in-scope deliverables.

## Required reading before starting

This work item is the fourth of seven executing the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`. The implementing agent **MUST** read that document, the previous three work items (`0066-…`, `0067-…`, `0068-…`), and the current state of `src/data/`, `src/engine/`, and `src/command/` before writing any code.

The four tenets, again:

1. **Frontends contain NO business logic.** This is the most heavily enforced tenet of this work item. Any `if`, `match`, or computed-default behavior that depends on the *meaning* of a command, flag, or response is wrong and lives in Layer 2. Frontends parse keystrokes/HTTP/argv into `CommandFrontend` answers and render typed outcomes back. That is all.
2. Layer 3 (frontend) consumes Layer 0 (data), Layer 1 (engine), and Layer 2 (command) only — but in practice should consume *only* Layer 2 (`Dispatch`, `*CommandFrontend` traits, `*Outcome` types) and Layer 0 (`Session`, `SessionManager`). It should rarely need to touch Layer 1 directly. Anywhere it does, ASK THE DEVELOPER whether that touch is necessary or whether a missing Layer 2 surface should be added.
3. Layer 4 (binary) is minimal. `main.rs` builds clap from `CommandCatalogue`, parses argv, and dispatches to either the CLI frontend (when a subcommand is present) or the TUI frontend (bare invocation). That is the entire body of `main`.
4. When uncertain, ASK THE DEVELOPER.

The companion work items are:

- `0066-grand-architecture-foundation-and-layer-0-data.md` (already merged)
- `0067-grand-architecture-layer-1-engines.md` (already merged)
- `0068-grand-architecture-layer-2-command-and-dispatch.md` (already merged)
- `0070-grand-architecture-finalize-and-remove-oldsrc.md`

## Summary:

- Build `src/frontend/cli/` — implements `CommandFrontend`, every `*CommandFrontend`, and the `ContainerFrontend` and `WorkflowFrontend` adapters needed for stdin/stdout/stderr binding. Builds clap arg matches and projects them through Dispatch. No business logic.
- Build `src/frontend/tui/` — fully reimplements the existing TUI on top of `SessionManager`, `Dispatch`, and the per-command frontend traits. Tabs become `Session` instances managed by `SessionManager`. Command-box input goes straight to `Dispatch`. Hints come from `CommandCatalogue::tui_hint_for`. Dialogs render data structures returned from per-command frontend trait calls; user choices are returned to lower layers as typed action enums. Every existing TUI behavior, keyboard shortcut, and visual element is preserved.
- Build `src/frontend/headless/` — fully reimplements the existing headless server on top of `SessionManager` and `Dispatch`. The route table is preserved verbatim from old-amux (a fixed set of REST endpoints, not derived from `CommandCatalogue`). The single command-execution endpoint (`POST /v1/commands`) accepts `{ subcommand, args }`, constructs a `HeadlessCommandFrontend` that parses the subcommand + args into a `CommandPath` via `Dispatch`, and runs the command — replacing the old child-process spawn. All other handlers (session management, status, log streaming, workflow state) are ported to the new layered architecture with identical schemas.
- Implement `src/main.rs` (Layer 4) as a tiny binary that builds clap from the catalogue, parses argv, constructs `SessionManager` + engines, and dispatches to either the CLI or the TUI frontend. The headless server is launched by the `headless start` *command* (Layer 2), not by `main.rs`.
- Swap the `Cargo.toml` so the user-facing `amux` binary is built from `src/main.rs`. Rename the previous `amux-next` target out of existence. The legacy `oldsrc/` tree remains in place as frozen reference material; it is no longer compiled.
- Comprehensive parity tests (existing user-visible behavior, no regressions). The next work item, 0070, deletes `oldsrc/` once parity is signed off.

## User Stories

### User Story 1:
As a: existing amux user

I want to:
upgrade to the new amux binary and have every CLI command, every TUI keystroke, every headless API endpoint be compatible with my existing workflows, but with improved quality and parity between frontends

So I can:
benefit from the new architecture without learning anything new or losing any feature.

### User Story 2:
As a: future implementing agent adding a new frontend (desktop app, code editor extension, kubernetes operator)

I want to:
read `src/frontend/cli/`, `src/frontend/tui/`, and `src/frontend/headless/` and see three small, self-similar implementations that all consume Dispatch the same way

So I can:
add a fourth frontend by following the same pattern, with no business-logic decisions to make.

### User Story 3:
As a: maintainer reading `src/main.rs`

I want to:
see fewer than 100 lines of code that build clap, dispatch, and return

So I can:
trust that the entrypoint is not hiding any business logic.

## Implementation Details:

### 0. Required reading and ground rules

- Read `aspec/architecture/2026-grand-architecture.md` end-to-end.
- Read `aspec/uxui/cli.md` for user-visible CLI behavior; nothing in this work item changes that surface.
- Read the current state of `src/data/`, `src/engine/`, and `src/command/`.
- **oldsrc code reuse policy**: The grand architecture tenet (no business logic in Layer 3) applies to *behavior*, not to Ratatui rendering. Two categories of oldsrc code are distinct:
  - **Must be reimplemented**: anything that calls the old command system, interprets command output semantics, drives workflow state, resolves agents, or makes decisions that belong in Layer 2. Do not lift this code.
  - **Should be adapted / may be copied**: pure Ratatui rendering (`draw_*` functions, layout calculations, widget construction, color computations, border styles), dialog widget state types, PTY parsing infrastructure, keyboard cursor-movement helpers. This code carries no business logic; copying it and adapting the type references (`TabState` → `Session`, old `Dialog` types → new dialog types) is the expected approach. **Re-implementing the Ratatui layout from scratch increases the risk of visual regressions.** See §8 for the per-file breakdown.
  - Key files to read: `oldsrc/main.rs`, `oldsrc/cli.rs`, `oldsrc/tui/*.rs` (~21k lines), `oldsrc/commands/headless/*.rs`.
- When uncertain, ASK THE DEVELOPER.

### 1. `src/frontend/cli/` — CLI frontend

Files:

- `mod.rs` — entry point; `pub async fn run(matches: clap::ArgMatches, runtime_ctx: RuntimeContext) -> ExitCode`.
- `command_frontend.rs` — `CliCommandFrontend` implementing `CommandFrontend` over `clap::ArgMatches`.
- `per_command/` — one file per command implementing the corresponding `*CommandFrontend` (e.g. `exec_workflow.rs` implements `ExecWorkflowCommandFrontend`). `per_command/ready.rs` implements both `ReadyFrontend` and `ReadyCommandFrontend` (supertrait), printing phase transitions and step statuses to stderr, prompting on stdin for Dockerfile and legacy-migration decisions. `per_command/init.rs` implements both `InitFrontend` and `InitCommandFrontend`, prompting for aspec replacement, audit, and work-items config. `per_command/claws.rs` implements both `ClawsFrontend` and `ClawsCommandFrontend`, printing `ClawsPhase` transitions to stderr and prompting on stdin for clone-replacement and audit decisions.
- `container_frontend.rs` — `CliContainerFrontend` binding `ContainerFrontend` to stdin/stdout/stderr (with PTY allocation when stdin is a TTY).
- `workflow_frontend.rs` — `CliWorkflowFrontend` rendering workflow status to stderr, prompting on stdin for `user_choose_next_action`. The prompt MUST present only the actions in `AvailableActions` — `LaunchNext`, `ContinueInCurrentContainer`, `RestartCurrentStep`, `CancelToPreviousStep`, `Pause`, `Abort` — each conditionally included based on the corresponding `can_*` flag. Excluded actions MUST NOT appear in the prompt. When an action is excluded, the `*_unavailable_reason` string SHOULD be printed as a parenthetical note so the user understands why.
- `output.rs` — small helpers for terminal styling (colors, hyperlinks). Pure presentation.
- `user_message.rs` — `CliUserMessageSink` implementing `UserMessageSink`. Holds a `Vec<UserMessage>` queue and a `pty_active: bool` flag. `write_message` pushes to the queue when `pty_active` is true, or writes immediately to stderr when false. `replay_queued` writes all queued messages to stderr in insertion order and clears the queue. The `CliContainerFrontend` sets `pty_active = true` before handing the terminal to the container and `pty_active = false` after the container exits. The command layer calls `replay_queued` after each `ContainerExecution::wait` and after `WorktreeLifecycle::finalize`.
- `worktree_lifecycle_frontend.rs` — `CliWorktreeLifecycleFrontend` implementing `WorktreeLifecycleFrontend`. Prompts on stdin for each decision (pre-commit warning: `[c]ommit / [u]se last commit / [a]bort`; existing worktree: `[r]esume / [R]ecreate`; post-workflow action: `[m]erge / [d]iscard / [s]kip`; merge confirm: `[y/n]`; cleanup confirm: `[y/n]`). Reports via stderr. Default commit message pre-populated as `"WIP: pre-worktree commit"`.

The `CliWorktreeLifecycleFrontend` and `CliUserMessageSink` MUST be the same concrete type (or one wraps the other) so that messages written during the `WorktreeLifecycle::prepare` call (e.g. detached-HEAD warning) are queued if a PTY container is active. In practice, the entire `CliExecWorkflowCommandFrontend` type implements all of `ContainerFrontend + WorkflowFrontend + WorktreeLifecycleFrontend + UserMessageSink` and holds the queue state in one place.

The CLI frontend's logic is small:

```rust
pub async fn run(matches: ArgMatches, ctx: RuntimeContext) -> ExitCode {
    let path = command_path_from_matches(&matches);
    let frontend = CliCommandFrontend::new(matches);
    let dispatch = Dispatch::new(frontend, ctx.session, ctx.engines);
    match dispatch.run_command(&path).await {
        Ok(outcome) => render_outcome_for_cli(outcome).await,
        Err(err) => render_error_for_cli(err).await,
    }
}
```

`render_outcome_for_cli` and `render_error_for_cli` are pure-presentation helpers that pattern-match on the typed outcome/error and write to stdout/stderr. Any decision that *changes behavior* belongs in Layer 2.

### 2. `src/frontend/tui/` — TUI frontend

This is the largest block of work in the refactor (legacy TUI is ~21k lines). The grand architecture document is explicit:

> User-perceptible functionality, UX, design, and keyboard operations should all remain identical to pre-refactor, but powered by the layered architecture instead of any TUI package business logic.

Files (proposed; ASK THE DEVELOPER if a different split fits better):

- `mod.rs` — entry point: builds `SessionManager` (in-memory), constructs the `App`, runs the event loop.
- `app.rs` — `App` owns the `Terminal`, the `SessionManager`, and the active dialog stack. No business logic.
- `tabs.rs` — tab management (one `Session` per tab) on top of `SessionManager`.
- `command_box.rs` — text input widget. Captures keystrokes; on submit, hands the raw string to Layer 2's `Dispatch::parse_command_box_input(...)` (added in 0068). Performs no parsing or interpretation itself.
- `command_frontend.rs` — `TuiCommandFrontend` implementing `CommandFrontend`. Pulls flag values from the parsed command-box input.
- `per_command/` — one file per command implementing the corresponding `*CommandFrontend`. Each is a thin wrapper that bridges command frontend trait calls into TUI dialog rendering and keyboard input.
- `container_view.rs` — `TuiContainerFrontend` implementing `ContainerFrontend`. Owns the PTY allocation, scrollback buffer, and rendering.
- `workflow_view.rs` — `TuiWorkflowFrontend` implementing `WorkflowFrontend`. Renders the workflow control dialog and yolo countdowns. The workflow control dialog MUST present only the actions present in `AvailableActions` — this includes `LaunchNext`, `ContinueInCurrentContainer`, `RestartCurrentStep`, `CancelToPreviousStep`, `Pause`, and `Abort`. Actions excluded by the engine (e.g. `ContinueInCurrentContainer` for cross-agent transitions, `CancelToPreviousStep` on the first step) MUST be visually disabled or omitted, with the corresponding `*_unavailable_reason` string shown as a tooltip or inline note.
- `ready_view.rs` — `TuiReadyFrontend` implementing both `ReadyFrontend` and `ReadyCommandFrontend`. Renders `ReadyPhase` transitions as progress steps in the TUI, opens modal dialogs for Dockerfile and legacy-migration decisions, and hands container build/audit output to a `TuiContainerFrontend`.
- `init_view.rs` — `TuiInitFrontend` implementing both `InitFrontend` and `InitCommandFrontend`. Renders `InitPhase` transitions, opens modal dialogs for aspec replacement, audit, and work-items configuration.
- `claws_view.rs` — `TuiClawsFrontend` implementing both `ClawsFrontend` and `ClawsCommandFrontend`. Renders `ClawsPhase` transitions as progress steps, opens modal dialogs for clone-replacement and audit decisions, and hands container build/audit output to a `TuiContainerFrontend`. Reproduces visual and keyboard behavior equivalent to the `claws init` flow in `oldsrc/commands/claws.rs`.
- `dialogs/` — pure-presentation dialog widgets (selection lists, confirmations, text prompts). Each dialog has a typed input (the data Layer 2 wants the user to choose from) and a typed output (the user's choice). Dialogs do NOT decide what the next step is — they only render and collect. Adapt dialog key-handling code from `oldsrc/tui/input.rs`.
- `text_edit.rs` — shared single-line and multi-line text edit widget (cursor movement, backspace/delete, home/end, Ctrl+Enter submit). Adapted from `oldsrc/tui/input.rs` cursor-movement helpers. Used by `command_box.rs`, `WorktreePreCommitMessage`, `WorktreeCommitPrompt`, `NewTitleInput`, `NewInterviewSummary`, and all other text-input dialogs.
- `pty.rs` — PTY session management (vt100 parser, channel bridge, resize handling). **Copy from `oldsrc/tui/pty.rs` with import updates only.**
- `keymap.rs` — keyboard shortcut definitions. Pure presentation.
- `render.rs` — pure rendering of UI chrome (tab bar, status bar, hints). **Adapt from `oldsrc/tui/render.rs`; see §8a.**
- `hints.rs` — pulls hint text via `CommandCatalogue::tui_hint_for`.
- `user_message.rs` — `TuiUserMessageSink` implementing `UserMessageSink`. Appends messages to a per-tab status log that the TUI renders in a scrollable panel. `replay_queued` is a no-op (messages are rendered live). The status log is visible during container execution without interrupting the container view.
- `worktree_lifecycle_frontend.rs` — `TuiWorktreeLifecycleFrontend` implementing `WorktreeLifecycleFrontend` as modal dialogs:
  - `ask_pre_worktree_uncommitted_files`: `WorktreePreCommitWarning` dialog (showing file list), transitions to `WorktreePreCommitMessage` dialog on 'c'.
  - `ask_existing_worktree`: inline prompt in the status area (or a small modal) with `[r]esume / [R]ecreate`.
  - `ask_post_workflow_action`: `WorktreeMergePrompt` dialog: `[m]erge / [d]iscard / [s]kip-and-keep`.
  - `ask_worktree_commit_before_merge`: `WorktreeCommitPrompt` dialog with editable text box (default message pre-populated, supports cursor navigation, Ctrl+Enter to submit).
  - `confirm_squash_merge`: `WorktreeMergeConfirm` dialog: `[y/n]`.
  - `confirm_worktree_cleanup`: `WorktreeDeleteConfirm` dialog: `[y/n]`.
  - All dialogs reproduce the exact key bindings and visual layout from `oldsrc/tui/` (see `Dialog` variants in `oldsrc/tui/state.rs` and `oldsrc/tui/input.rs`).

Critical constraints from the grand architecture document:

- All command-box input is routed directly to a method in the `Dispatch` package, no parsing or anything else done by the TUI itself.
- All hint text for commands, subcommands, and flags comes from methods in the `Dispatch` package.
- All data displayed in any dialog comes from per-command frontend trait calls. The dialog is a pure render; the data and the choice options flow up from Layer 2.
- Action objects (e.g. `NextAction::AdvanceWorkflow`, `NextAction::PauseWorkflow`) are typed enums returned by frontend trait methods. The TUI does not invent these; they are defined alongside `WorkflowFrontend` etc. in Layers 1/2.

#### Behavioral parity checklist

The TUI must preserve, with zero user-visible drift:

- Tab opening, closing, switching, and ordering (every existing keyboard shortcut).
- Per-tab session state (`Session` replaces `TabState`).
- Command box behavior, completion, hint display.
- Container window rendering (stdout/stderr, scrollback, dynamic tab widths from work item ~recent).
- Workflow control dialog (advance, pause, resume, abort) — content from `WorkflowFrontend`.
- Yolo-mode countdown rendering (timing from `WorkflowEngine`, rendering here).
- Stuck-agent detection display.
- All status-bar elements.
- All keyboard shortcuts documented today.
- All error rendering (translations of `CommandError`, `EngineError`, `DataError` into user-friendly strings).
- `amux ready` phase-by-phase progress display (each `ReadyPhase` transition updates the TUI; dialogs for Dockerfile creation and legacy migration fire modally).
- `amux init` phase-by-phase progress display (each `InitPhase` transition updates the TUI; dialogs for aspec replacement, audit decision, and work-items config fire modally).
- Worktree pre-creation flow: `WorktreePreCommitWarning` dialog (shows uncommitted file list, `[c]ommit / [u]se last commit / [a]bort` keybindings) and `WorktreePreCommitMessage` dialog (editable text box with default `"WIP: pre-worktree commit"`, cursor navigation, Ctrl+Enter to submit — exact key handling from `oldsrc/tui/input.rs::handle_worktree_pre_commit_message`).
- Worktree post-completion flow: `WorktreeMergePrompt` dialog (`[m]erge / [d]iscard / [s/Esc]kip-and-keep`), `WorktreeCommitPrompt` dialog (if worktree has uncommitted files, editable text box, Ctrl+Enter/Ctrl+S to submit), `WorktreeMergeConfirm` dialog (`[y/n]`), `WorktreeDeleteConfirm` dialog (`[y/n]`).
- `UserMessageSink` messages appear in the per-tab status log during container execution and are scrollable independently of the container PTY view.

A line-by-line port of the *business-logic-entangled* parts of `oldsrc/tui/` is not the goal. Where the legacy code embedded business logic in the TUI (workflow advance decisions, agent resolution, etc.), that logic lives in Layer 2 now and the TUI only renders the result. However, **pure Ratatui rendering code — layout calculations, `draw_*` functions, color functions, dialog widget state types, PTY parsing — SHOULD be adapted from `oldsrc/tui/render.rs`, `oldsrc/tui/pty.rs`, `oldsrc/tui/state.rs`, and `oldsrc/tui/input.rs`**. These carry no business logic; rewriting them from scratch is likely to introduce visual regressions. See §8 for the per-file guidance.

### 3. `src/frontend/headless/` — Headless frontend

**The HTTP API surface MUST NOT change.** Every path, every HTTP method, every request body schema, and every response body schema must be wire-identical to the old-amux headless server (`oldsrc/commands/headless/server.rs`). The only internal change is that `POST /v1/commands` dispatches through `Dispatch` instead of spawning a child `amux` process. If a required Dispatch surface is missing, stop and ask the developer.

The existing API is a **single command-execution endpoint** (`POST /v1/commands`) that accepts `{ subcommand: String, args: Vec<String> }`. `Dispatch` — not the route table — is responsible for parsing `subcommand + args` into a `CommandPath` and routing to the right `Command` implementation. `CommandCatalogue` is **not** used to derive routes; it is used only to validate the incoming subcommand name (replacing the old hardcoded `KNOWN_SUBCOMMANDS` list) and to parse args into typed flag values inside `HeadlessCommandFrontend`.

Files:

- `mod.rs` — entry point: `pub async fn serve(config: HeadlessServeConfig, engines: Engines, session_manager: Arc<RwLock<SessionManager>>) -> Result<(), HeadlessError>`. **Layer 2 cannot call `serve` directly — that would be an upward call.** Instead, `HeadlessStartCommand` (Layer 2) accepts a `HeadlessStartCommandFrontend` trait at instantiation. The trait exposes a method like `serve_until_shutdown(config: HeadlessServeConfig) -> Result<(), CommandError>`. The CLI frontend's `HeadlessStartCommandFrontend` impl calls `crate::frontend::headless::serve(...)` — that is a peer call within Layer 3 and is allowed. The headless frontend never starts itself; it is always launched by an impl living in some other Layer 3 frontend (today, only the CLI's impl exists).
- `routes.rs` — registers the **same HTTP routes as `oldsrc/commands/headless/server.rs::build_router`**, verbatim. The route list is fixed; it is not derived from `CommandCatalogue`:
  ```
  GET    /v1/status
  GET    /v1/workdirs
  GET    /v1/sessions
  POST   /v1/sessions
  GET    /v1/sessions/:id
  DELETE /v1/sessions/:id
  POST   /v1/commands                  — accepts { subcommand, args }; dispatches via Dispatch
  GET    /v1/commands/:id
  GET    /v1/commands/:id/logs
  GET    /v1/commands/:id/logs/stream  — SSE stream of the command's output.log file
  GET    /v1/workflows/:command_id
  ```
- `command_frontend.rs` — `HeadlessCommandFrontend` implementing `CommandFrontend`. Constructed from `CreateCommandRequest { subcommand: String, args: Vec<String> }`. Provides `parse_command_path(&self) -> Result<CommandPath, HeadlessError>` — derives the command path from `subcommand` and the leading positional `args` (uses `CommandCatalogue` to know which top-level commands have subcommands). Implements `CommandFrontend::get_flag` by parsing the remaining `args` against the command's known flags. For commands that require interactive input (`ready`, `init`, `claws`, worktree lifecycle decisions, agent setup), the frontend returns the safe non-interactive defaults listed in §7u; each default MAY be overridden by fields in the request body.
- `container_log.rs` — `HeadlessContainerFrontend` implementing `ContainerFrontend`. Writes container stdout/stderr to the command's `output.log` file — the same path and format as the old-amux `execute_command` function. The `GET /v1/commands/:id/logs/stream` SSE endpoint streams from this file, line-per-`data:` event, terminated by `[amux:done]`. The wire format is byte-identical to old-amux.
- `workflow_state.rs` — `HeadlessWorkflowFrontend` implementing `WorkflowFrontend`. Writes workflow state to `workflow.state.json` in the command directory — the same path and format as the old-amux `poll_workflow_state` helper. The `GET /v1/workflows/:command_id` endpoint reads from this file; the JSON schema is identical to old-amux.
- `user_message.rs` — `HeadlessUserMessageSink` implementing `UserMessageSink`. Emits each message as an SSE event of type `amux-message` with `{ "level": "info"|"warning"|"error"|"success", "text": "..." }`. `replay_queued` is a no-op (messages are streamed live).
- `worktree_lifecycle_frontend.rs` — `HeadlessWorktreeLifecycleFrontend` implementing `WorktreeLifecycleFrontend`. Uses request-parameter defaults for all decisions (see §7u). Reports stream as `amux-message` SSE events. ASK THE DEVELOPER whether to expose Q&A decisions as separate API endpoints or as upfront request parameters.
- `auth.rs` — TLS + API-key middleware. Pure plumbing; the cryptographic logic is in `AuthEngine` (Layer 1).
- `errors.rs` — translates `CommandError` etc. into HTTP status codes + JSON error bodies.

The `POST /v1/commands` handler replaces the child-process spawn with a Dispatch call. All surrounding logic (session validation, concurrency guard, `x-amux-session` header, DB inserts, command directory creation, 202 Accepted response) is copied verbatim from `oldsrc/commands/headless/server.rs::handle_create_command` and `execute_command`; only the body of `execute_command` changes:

```rust
// OLD (oldsrc): spawns child amux process
let mut cmd = tokio::process::Command::new(&amux_bin);
cmd.arg(&subcommand).args(&args); /* ... */ cmd.spawn();

// NEW: dispatches through Dispatch
let frontend = HeadlessCommandFrontend::new(subcommand, args, log_path.clone());
let command_path = frontend.parse_command_path()?;
let dispatch = Dispatch::new(frontend, session, engines);
dispatch.run_command(&command_path).await
```

`CreateCommandRequest`, `CreateCommandResponse`, `SessionResponse`, `CommandResponse`, `StatusResponse`, and `ErrorResponse` — all Serde shapes are **identical to `oldsrc/commands/headless/server.rs`**. Do not rename fields, change types, or add/remove fields.

The grand architecture document explicitly forbids the server from "just calling the CLI": the headless frontend talks to `Dispatch` directly, never spawns a child `amux` process.

#### Headless behavioral parity checklist

- Every route in `oldsrc/commands/headless/server.rs::build_router` continues to exist with the **same path pattern, same HTTP method, same request body schema, and same response body schema**. No routes may be added, removed, renamed, or have their schemas changed. The only permitted internal difference is that `POST /v1/commands` dispatches through `Dispatch` instead of spawning a child process. **Routes are NOT derived from `CommandCatalogue::rest_route_table`.**
- **Before writing any handler**, read `oldsrc/commands/headless/server.rs` end-to-end. Preserve the session-validation logic, concurrency guard (`busy_sessions`), DB-update sequence, graceful-shutdown task drain, and all error-response shapes verbatim — replacing only the `execute_command` function body.
- TLS, bind-address, and auth-disabled behavior from work item 0065 is preserved. The `AuthEngine` (Layer 1) holds the logic; this frontend is plumbing.
- SSE stream wire format (`GET /v1/commands/:id/logs/stream`): line-per-`data:` event, `[amux:done]` sentinel. Byte-identical to old-amux.
- Workflow state JSON schema (`GET /v1/workflows/:command_id`): same shape as old-amux `WorkflowState`.

### 4. `src/main.rs` — Layer 4

`main.rs` after this work item:

```rust
#![forbid(unsafe_code)]

use anyhow::Result;
use amux::command::dispatch::CommandCatalogue;
use amux::data::{Session, SessionManager, GlobalConfig};
use amux::engine::{ContainerRuntime, GitEngine, OverlayEngine, AuthEngine, AgentEngine, WorkflowStateStore};
use amux::frontend::{cli, tui};

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    let clap_cmd = CommandCatalogue::get().build_clap_command();
    let matches = clap_cmd.get_matches();

    let global_config = GlobalConfig::load().unwrap_or_default();
    let git = std::sync::Arc::new(GitEngine::new());
    let runtime = std::sync::Arc::new(ContainerRuntime::detect(&global_config)?);
    // ...other engines...

    let session_manager = std::sync::Arc::new(parking_lot::RwLock::new(SessionManager::in_memory()));
    let session = Session::open(std::env::current_dir()?, &*git)?;
    session_manager.write().insert(session.clone())?;

    let ctx = RuntimeContext { session_manager, session: std::sync::Arc::new(parking_lot::RwLock::new(session)), engines: Engines { runtime, git, /* ... */ } };

    if matches.subcommand().is_some() {
        Ok(cli::run(matches, ctx).await)
    } else {
        Ok(tui::run(matches, ctx).await)
    }
}
```

That is the entire `main.rs` body. The `headless start` command launches the headless server through Layer 2 → Layer 1 → Layer 3 (`frontend::headless::serve`); `main.rs` does not branch on `headless`.

### 5. `Cargo.toml` swap

After this work item:

```toml
[[bin]]
name = "amux"
path = "src/main.rs"

[lib]
name = "amux"
path = "src/lib.rs"
```

Remove the `amux-next` target. Remove the `[[bin]]` and `[lib]` blocks pointing at `oldsrc/`. Leave the `oldsrc/` directory and its files in place — they are no longer compiled by Cargo, but they are not deleted yet. Update `Makefile` so `make all`, `make install`, `make test` continue to work; remove any `make test-next` shim added in 0066.

The `oldsrc/README.md` from 0066 stays. Add a note: "no longer compiled — see work item 0070 for removal."

### 6. What must NOT happen in this work item

- No business logic in `src/frontend/`. If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2; add it there.
- No deletion of `oldsrc/`. That is 0070.
- No edits inside `oldsrc/` other than possibly the `oldsrc/README.md` note.
- No new commands, new flags, or new user-visible behavior. This work item is *parity only*.
- No regressions in the `aspec/uxui/cli.md` documented surface.
- **No changes to the headless HTTP API surface.** No route paths, no HTTP methods, no request body fields, no response body fields. The new headless frontend is a transparent re-plumbing of the existing API through Dispatch — clients talking to the new server must not need to change any request or response handling.

### 7. Frontend parity addenda — TUI behaviors that MUST be preserved

The legacy TUI (`oldsrc/tui/*.rs`, ~21k lines) carries non-trivial user-perceptible behavior that is easy to lose in a re-implementation. This section enumerates each preserved behavior with the corresponding new-architecture surface. Where a behavior is not yet covered by a Layer 1 / Layer 2 frontend trait, the addendum specifies which trait to extend and where.

#### 7a. Tab management — colors, indicators, focus

Each `TabState` (now wrapped around a `Session`) renders in the tab bar with a color computed from execution state. The legacy color matrix MUST be preserved; the function lives in `src/frontend/tui/tabs.rs::tab_color`:

- Stuck (any phase) → Yellow
- Remote-bound (any phase) → Magenta
- Error → Red
- Running + container PTY → Green
- Running + no container → Blue
- Running + claws command → Magenta
- Idle / Done → Dark Gray

The active tab renders with `➡ project` and TOP+LEFT+RIGHT borders (no bottom). Background yolo countdowns alternate `⚠️  yolo in Ns` and `🤘 yolo in Ns` every 2 seconds in the tab subcommand label (legacy `tab_subcommand_label`). Stuck tabs prepend `⚠️ ` to the command in the label.

Tab name truncates at 14 visible characters with `…` (logic in `tab_project_name`; copy from `oldsrc/tui/state.rs`). For remote-bound tabs, `display_host` is used in place of the local folder name, with the same 14-char cap.

Tab width algorithm (`compute_tab_bar_width`): 1 tab = 1/4 area width; 2 tabs = 1/2; 3 tabs = 3/4; 4+ tabs = full area / n. Each tab gets `min(natural_content_width + 2 borders, budget)`. Copy `compute_tab_bar_width` and `draw_tab_bar` verbatim from `oldsrc/tui/render.rs`, adapting type references.

`Focus` enum (CommandBox vs ExecutionWindow) governs which keybindings apply. ↑ from CommandBox switches focus to ExecutionWindow when a container is running. Esc from ExecutionWindow returns focus to CommandBox.

ContainerWindow state (Hidden / Minimized / Maximized) — Ctrl+M cycles. Hidden = no window rendered; Minimized = 1-line status bar; Maximized = full window.

**Execution window border color** (`window_border_color` — copy from `oldsrc/tui/state.rs`):

- Running + ExecutionWindow focused → Blue
- Running + CommandBox focused → Gray
- Done + ExecutionWindow focused → Green
- Done + CommandBox focused → Gray
- Error (any focus) → Red
- Idle → DarkGray

**Execution window phase label** (preserve verbatim):

- Idle: ` amux `
- Running: ` ● running: {command} `
- Done: ` ✓ done: {command} `
- Error: ` ✗ error: {command} (exit {exit_code}) `

**Welcome message** (shown in exec window body when Idle and no output): two lines of `Color::DarkGray` text — `"  Welcome to amux."` and `"  Running \`amux ready\` to check your environment..."`.

**Full frame layout** (copy `draw()` structure from `oldsrc/tui/render.rs`):

```
Vertical: tab bar (3 rows) | main area (Min 5)
Main area vertical:
  exec window (Min 5)
  [optional] minimized container bar OR last-container summary (3 rows each, mutually exclusive)
  [optional] workflow strip (variable height)
  status bar (1 row)
  command box (3 rows)
  suggestion row (1 row)
Container overlay (Maximized): 95% of exec area width × 95% height, centered. Inner area = outer − 2 borders.
```

Copy `calculate_container_inner_size` verbatim from `oldsrc/tui/render.rs`. This function is used to size the PTY/vt100 parser to match the rendered window.

#### 7b. Command box and autocomplete

The command box widget MUST honor the legacy keybindings and behaviors:

- Tab / Shift+Tab cycle through autocomplete suggestions (suggestions sourced from `CommandCatalogue::tui_completions`).
- Suggestion row displays first-match → `> · sugg2 · sugg3 · …` separated by middots.
- When suggestions are not visible: row shows `CWD: /path` or `Using Worktree: /path`.
- Backspace deletes char before cursor; Delete deletes char at cursor; Home/End jump.
- Ctrl+T (always) opens NewTabDirectory dialog regardless of focus.
- On invalid command typed in the box: `Dispatch::parse_command_box_input` returns a structured error (`UnknownCommand`, `MissingArgument`, etc.) AND a typo-correction suggestion (Levenshtein distance ≤ 4) when applicable. The TUI renders the suggestion as `did you mean: <suggestion>?` in red below the box. The Levenshtein helper lives in `CommandCatalogue::closest_command(input: &str)` (Layer 2 catalogue helper, not TUI logic).

#### 7c. Workflow control board — exact key matrix

`TuiWorkflowFrontend::user_choose_next_action` opens the `WorkflowControlBoard` modal. The modal MUST render with the exact arrow-key matrix from `oldsrc/tui/render.rs`:

```
         ↑ Restart current
    ← Prev   Right: Next (new container) →
         ↓ Next (same container)
         ^C Cancel workflow
  [last step only] Ctrl+Enter Finish
```

Mapping of keys to `NextAction` (per WI 0067 §9a.4):

- ↑ → `RestartCurrentStep`
- ← → `CancelToPreviousStep`
- → → `LaunchNext`
- ↓ → `ContinueInCurrentContainer { prompt }` (with the next step's prompt template substituted; the engine constructs the prompt and the dialog only renders/forwards)
- Ctrl+Enter → `FinishWorkflow` (only enabled on last step; visually disabled otherwise)
- Ctrl+C → opens `WorkflowCancelConfirm` modal; on `[y/1]` returns `NextAction::Abort`
- `d` → `DisableAutoAdvanceForCurrentStep` — sets a per-tab flag in the frontend so the stuck/yolo dialog will not auto-popup again for this step. **The flag is purely a Layer 3 concern** (engine still ticks the timers); the TUI uses the flag to suppress auto-popup. Persist as `TabState::auto_workflow_disabled_steps: HashSet<String>`.
- Esc → close the dialog without choosing an action; engine continues waiting (this is NOT a `NextAction` — the trait method blocks until a real choice is made; Esc just dismisses the modal so the user can scroll the container, then re-opens via Ctrl+W).

Disabled actions render in dark gray with the `*_unavailable_reason` string as a tooltip below the matrix. The dialog title is `" Workflow Control "` with a yellow rounded border, center-aligned popup (52 cols × 13–15 rows), step name truncated to fit width.

#### 7d. Workflow stuck detection and yolo countdown — TUI rendering

Per WI 0067 §9a.5 the engine fires `report_step_stuck` and `yolo_countdown_tick`. The TUI renders these as:

- `report_step_stuck`: the active tab turns yellow; `⚠️ ` prepends the command in the tab label; status bar shows `agent appears stuck — Ctrl+W to open workflow controls`.
- `report_step_unstuck`: tab returns to green; status bar resets.
- `yolo_countdown_tick(remaining)` (only when `--yolo` was set): opens the `WorkflowYoloCountdown` modal (magenta border) with the step name and remaining seconds. The modal is dismissable with Esc, which returns `YoloTickOutcome::Cancel` to the engine. Background-tab indicator alternates `⚠️  yolo in Ns` / `🤘 yolo in Ns` every 2 seconds.
- The TUI's per-tab `auto_workflow_disabled_steps` flag (§7c) suppresses re-opening of the modal after a manual dismissal — even though the engine still ticks the countdown, the TUI returns `YoloTickOutcome::Cancel` for every tick on a disabled-auto step. The user can manually re-arm by pressing Ctrl+W.

#### 7e. Workflow step error dialog

`WorkflowFrontend::user_choose_after_step_failure` (WI 0067 §9a.4) opens the legacy `WorkflowStepError` modal:

- Title: `" Step failed "` (red border).
- Body: step name + first-N lines of the failure output (`exit_code` and `signal` fields from `ContainerExitInfo`).
- Keys: `[r]` or `[1]` → `StepFailureChoice::Retry`; `[q]` / `[2]` / `Esc` → `Pause`; `[a]` → `Abort`.

#### 7f. Agent setup confirmation dialog

`AgentSetupFrontend::ask_agent_setup` (WI 0068 §6.3b) opens the legacy `AgentSetupConfirm` modal:

- Title varies: `" Set up <agent>? "` (Dockerfile missing) or `" Build <agent> image? "` (Dockerfile present, image missing).
- Body explains the situation and lists the planned actions.
- Keys: `[y]` / `Enter` → `AgentSetupDecision::Setup`; `[f]` → `FallbackToDefault` (only rendered when `default_available` is true and the requested agent != default); `[n]` / `Esc` → `Abort`.
- Per-step fallback caching: when the user presses `[f]` during a workflow, the TUI calls `AgentSetupFrontend::record_fallback(requested, default)`; subsequent steps in the same workflow that target `requested` automatically use `default` without re-prompting. Persist the cache in `TabState::workflow_agent_fallbacks: HashMap<AgentName, AgentName>`.

#### 7g. Mount scope dialog

`MountScopeFrontend::ask_mount_scope` (WI 0068 §6.3a) opens the legacy `MountScope` modal:

- Title: `" Mount Scope "`.
- Body: shows both paths (git_root and cwd) and explains what each option mounts.
- Keys: `[r]` → `MountGitRoot`; `[c]` → `MountCurrentDirOnly`; `[a]` / `Esc` → `Abort`.

#### 7h. Agent auth consent dialog

`AgentAuthFrontend::ask_agent_auth_consent` (WI 0068 §6.3c) opens a new modal `AgentAuthConsent`:

- Title: `" Agent credentials? "`.
- Body: lists the env-var names that will be injected (e.g. `ANTHROPIC_API_KEY`) and explains the consent semantics.
- Keys: `[y]` → `Accept` (persists `auto_agent_auth_accepted = true`); `[n]` → `Decline` (persists `false`); `[o]` (once) → `DeclineOnce` (no persistence). `Esc` → `DeclineOnce`.

#### 7i. Config show dialog

The legacy `Dialog::ConfigShow` (a full-screen interactive table) is preserved verbatim in `src/frontend/tui/config_show_view.rs`:

- Triggered by `config show` command from the command box, OR by Ctrl+, (toggle) anywhere in the TUI.
- Full-screen table: columns Field | Global | Repo | Effective.
- Arrow keys navigate rows; Enter enters edit mode on the selected cell.
- In edit mode: type to modify, Backspace/Delete supported, Ctrl+S saves to the appropriate config file (Global column → global config, Repo column → repo config), Esc cancels edit (reverts cell).
- Ctrl+, (or close button) closes the dialog. Closing without saving discards uncommitted edits with a confirmation prompt.
- Read-only fields (`auto_agent_auth_accepted`) render in gray and reject Enter (display tooltip "read-only").
- Validation errors (e.g. invalid agent name) display inline below the cell in red; the cell stays in edit mode until the user fixes or Esc-cancels.

The dialog calls into Layer 2 via `ConfigCommand::set_field` (which uses `RepoConfig::set_field` / `GlobalConfig::set_field` from Layer 0). The TUI never manipulates config JSON directly.

#### 7j. New-artefact dialogs (`new spec`, `new workflow`, `new skill`, `specs new`)

- `NewKindSelect`: radio modal with [1] Feature / [2] Bug / [3] Task / [4] Enhancement. Keys 1–4 select; Esc cancels. Rendered when `new spec` (or `specs new` alias) is invoked from the command box.
- `NewTitleInput`: single-line text input. Ctrl+Enter submits; Esc cancels.
- `NewInterviewSummary`: multiline text editor with cursor navigation (left/right/up/down, home/end, backspace/delete). Ctrl+Enter submits. Used for `--interview` mode summaries.
- `NewWorkflow` / `NewSkill`: multi-field form (title, format/extension, default agent, etc.). Tab / Shift+Tab cycle fields. Ctrl+Enter submits.
- All editable text inputs share the legacy `WORKTREE_COMMIT_PROMPT_KEYMAP` for cursor navigation — see `src/frontend/tui/text_edit.rs` (a single shared widget).

#### 7k. Claws dialogs (in addition to those listed in §2)

- `ClawsReadyHasForked`: [1] Yes / [2] No.
- `ClawsReadyUsernameInput`: GitHub username text input (single line).
- `ClawsReadySudoConfirm`: sudo password input (display masked as `*` per character).
- `ClawsReadyDockerSocketWarning`: [1] Accept / [2] Decline mounting Docker socket.
- `ClawsReadyOfferRestartStopped`: [1] Restart stopped container / [2] No.
- `ClawsReadyOfferStart`: [1] Start fresh container / [2] No.
- `ClawsRestartFailedOfferFresh`: [1] Delete and start fresh / [2] No.

These all map to `ClawsFrontend` methods (some new) — extend the trait in WI 0067 §5a.c to cover each. Update the engine state machine to fire each phase appropriately.

#### 7l. Quit and tab-close dialogs

- `QuitConfirm`: [y/Y/1/Enter] quit / [n/N/2/Esc] cancel. Triggered by Ctrl+C with a single tab, or `q` in idle command box.
- `CloseTabConfirm`: triggered by Ctrl+C with multiple tabs. Choices: Ctrl+C again → quit entire app; Ctrl+T → close just this tab; Esc → cancel.

#### 7m. PTY container view — VT100, scrollback, mouse selection

`TuiContainerFrontend` holds:

- A `vt100::Parser` instance for parsing ANSI escape sequences. Cell grid dimensions follow the rendered window size; on `resize_pty(cols, rows)` the parser is resized AND the engine is informed via `resize_pty` forwarding.
- A scrollback buffer of `terminal_scrollback_lines` lines (sourced from `EffectiveConfig::terminal_scrollback_lines`, default 10000). Configurable per-repo or globally via the catalogue's `config set terminal_scrollback_lines N`.
- `pty_pending_cr` flag for handling `\r` / `\r\n` sequences without flickering.
- A `pty_live_line` flag — true when the last line is incomplete (no trailing newline yet); used to overwrite spinner output in place.

Mouse handling:

- MouseDown in the container window starts a selection anchor (`terminal_selection_start`).
- MouseDrag extends the selection (`terminal_selection_end`).
- MouseUp finalizes — captures the vt100 cell snapshot for clipboard.
- Ctrl+Y copies the selected text to the system clipboard via the `arboard` crate (or equivalent). On wire failure, emits `UserMessage::error("clipboard unavailable")`.

Scrollback navigation (when ExecutionWindow has focus):

- ↑ / ↓ scroll one line.
- PageUp / PageDown scroll one page.
- `b` jumps to top of scrollback; `e` jumps to live (offset = 0).
- Mouse wheel scrolls (preserving selection if active).

Container output streams via `ContainerFrontend::write_stdout` / `write_stderr` chunks. The TUI does NOT distinguish stdout from stderr in rendering (matches legacy behavior); both feed the same vt100 parser.

Kitty keyboard protocol: `App::run` calls `crossterm::execute!(stdout, PushKeyboardEnhancementFlags(...))` best-effort on startup. Failure is non-fatal (legacy behavior). Cleanup on `App::drop` pops the flags.

#### 7n. Tab status log via `UserMessageSink`

`TuiUserMessageSink` writes each `UserMessage` to a per-tab status log rendered in a scrollable panel below the container window (or beside it, depending on layout). The log:

- Renders messages in insertion order with level-colored prefixes (Info: dim gray, Warning: yellow, Error: red, Success: green).
- Auto-scrolls to bottom on new message unless the user has scrolled up.
- `replay_queued()` is a no-op (messages are rendered live).

The status log is visible at all times — when a container is running, when the workflow control board is open, etc. Users can press `l` (lowercase L) when the ExecutionWindow has focus to toggle the log between collapsed (1-line summary) and expanded (full panel).

#### 7o. Status command — TUI tab annotations

`TuiStatusCommandFrontend` populates the `StatusCommandTuiContext` (WI 0068 §6.8) before each invocation:

```rust
fn build_tui_context(&self, session_manager: &SessionManager) -> StatusCommandTuiContext {
    StatusCommandTuiContext {
        tabs: session_manager.iter().enumerate().map(|(i, sess)| TuiTabSnapshot {
            tab_number: i as u32 + 1,
            container_name: sess.running_container_name(),
            is_stuck: sess.is_stuck(),
            command_label: sess.command_label(),
        }).collect(),
    }
}
```

The status command then renders the standard table with extra columns (Tab #, ⚠️ stuck) only when the context is `Some`.

#### 7p. TUI startup behavior

`tui::run(matches, ctx)` MUST:

1. Capture terminal: raw mode, alternate screen, mouse capture (`EnableMouseCapture`), Kitty keyboard protocol (best-effort).
2. Construct `App` with one initial tab at `std::env::current_dir()`.
3. Determine the startup command:
   - If `git_root_resolver` succeeds (cwd is in a git repo): build `Dispatch` for `["ready"]` with flags from `matches` (`--build`, `--no-cache`, `--refresh`) and run it through the initial tab's `TuiReadyFrontend`.
   - Otherwise: build `Dispatch` for `["status", "--watch"]` and run it through the initial tab's `TuiStatusCommandFrontend`.
4. Enter the event loop.

The startup invocation MUST run through the standard `Dispatch` → `*Command` → `*Frontend` chain — no special-casing in `App::new`. Cover with a unit test for both branches (in-repo, not-in-repo).

`StartupReadyFlags` is internal to the TUI's startup path — not a public type. It is just the legacy name for "the flags clap parsed at the top level that are also relevant to startup ready".

#### 7q. Remote session picker dialogs

The legacy TUI exposes per-tab remote session selection through several pickers. Each maps to `RemoteRunCommandFrontend` / `RemoteSessionStartCommandFrontend` / `RemoteSessionKillCommandFrontend` trait methods (added in WI 0068; not previously enumerated). For each picker:

- `RemoteSessionPicker` (selecting a session for `remote run` when `--session` is omitted): arrow-key navigable list of sessions fetched from the remote server. Enter to select; Esc to cancel.
- `RemoteSavedDirPicker` (selecting a directory for `remote session start` when `<DIR>` is omitted): list comes from `GlobalConfig::remote.saved_dirs`. Enter to select; Esc to cancel.
- `RemoteSessionKillPicker` (selecting a session for `remote session kill`): similar to `RemoteSessionPicker`.
- `RemoteSaveDirConfirm` (after `remote session start <DIR>` succeeds with a new directory): asks `[y]/[n]` whether to save the directory in `remote.saved_dirs` for future use. Headless default: false (do NOT save). CLI default: stdin prompt; non-TTY → false.

Each picker fetches data asynchronously via `RemoteClient` — the TUI shows a "loading…" placeholder until results arrive. Cover with a unit test that simulates a slow fetch.

#### 7r. Status command TIPS array and CLEAR_MARKER

`StatusCommand`'s rendered output ends with a random tip from a fixed `TIPS: &[&str]` array (legacy `oldsrc/commands/status.rs`). The selection index is `(unix_seconds % TIPS.len())` — deterministic per second. Preserve the exact array verbatim in `src/command/commands/status_tips.rs`. Cover with a unit test that asserts a frozen tip given a fixed timestamp.

`StatusCommand --watch` writes a CLEAR_MARKER (ANSI `\x1b[2J\x1b[H`) before each re-render. The CLI frontend forwards CLEAR_MARKER to stdout; the TUI swallows it (the TUI re-renders the dialog widget instead). Cover both behaviors.

#### 7s. `amux init --aspec` semantics

The legacy `--aspec` flag forces a fresh download of the aspec template tree from GitHub (`download_aspec_tarball`). The `InitPhase::CreatingAspecFolder` phase uses the bundled (compiled-in) template when `--aspec` is absent. When `--aspec` is present:

1. The `InitEngineOptions::run_aspec_setup` field is true.
2. The engine downloads the latest aspec tarball during the `CreatingAspecFolder` phase (or a new dedicated `DownloadingAspecTemplates` sub-phase).
3. If a download fails (network error, 404), fall back to the bundled template AND emit `UserMessage::warning("aspec download failed — using bundled template")`.

Cover all three paths (no `--aspec`: bundled; `--aspec` + success: downloaded; `--aspec` + failure: bundled with warning).

#### 7t. `WorkItemsConfig` structure

The `InitFrontend::ask_work_items_setup` return type:

```rust
pub struct WorkItemsConfig {
    pub dir: PathBuf,                   // required (relative to git_root)
    pub template: Option<PathBuf>,      // optional (relative to git_root)
}
```

The TUI's `InitWorkItemsDirInput` dialog prompts for `dir` (Enter to confirm). Then `InitWorkItemsTemplateInput` prompts for `template` (Enter to confirm with empty string → None, or skip via Esc → None). Cover with unit tests for: dir-only, dir+template, both empty (returns `Ok(None)`).

#### 7u. Headless dialog defaults — exhaustive list

The headless frontend implements every per-command frontend trait but defaults all interactive prompts to safe non-interactive values. Capture each default in `src/frontend/headless/defaults.rs` as named constants:

- `ReadyFrontend::ask_create_dockerfile` → `true` (always create when missing).
- `ReadyFrontend::ask_run_audit_on_template` → `false` (skip audit by default).
- `ReadyFrontend::ask_migrate_legacy_layout` → `false` (preserve legacy layout).
- `InitFrontend::ask_replace_aspec` → `false` (preserve existing).
- `InitFrontend::ask_run_audit` → `false` (skip).
- `InitFrontend::ask_work_items_setup` → `None` (skip work-items config).
- `ClawsFrontend::ask_replace_existing_clone` → `false`.
- `ClawsFrontend::ask_run_audit` → `false`.
- `WorkflowFrontend::user_choose_next_action` → `LaunchNext` for non-yolo (advance to next ready step), `LaunchNext` (with auto-advance) for yolo.
- `WorkflowFrontend::user_choose_after_step_failure` → `Pause` (do not auto-retry).
- `WorktreeLifecycleFrontend::ask_pre_worktree_uncommitted_files` → `UseLastCommit` (don't auto-commit).
- `WorktreeLifecycleFrontend::ask_existing_worktree` → `Resume`.
- `WorktreeLifecycleFrontend::ask_post_workflow_action` → `Keep` (don't auto-merge or auto-discard).
- `WorktreeLifecycleFrontend::ask_worktree_commit_before_merge` → `None`.
- `WorktreeLifecycleFrontend::confirm_squash_merge` → `false`.
- `WorktreeLifecycleFrontend::confirm_worktree_cleanup` → `false`.
- `MountScopeFrontend::ask_mount_scope` → `MountGitRoot`.
- `AgentSetupFrontend::ask_agent_setup` → `Setup` (proceed with download/build).
- `AgentAuthFrontend::ask_agent_auth_consent` → `DeclineOnce` (do NOT auto-persist consent over an API).

Each default MAY be overridden by request body parameters; the request schema lives alongside the catalogue's headless projection.

### 8. Code Reuse Policy — per-file breakdown

This section is the authoritative guide for deciding whether to adapt oldsrc code or reimplement from scratch. The rule: **business logic → reimplement on top of Dispatch; pure presentation → adapt from oldsrc**.

#### 8a. Files to copy and adapt (pure presentation — no business logic)

| oldsrc source | New destination | Notes |
|---|---|---|
| `oldsrc/tui/render.rs` | `src/frontend/tui/render.rs` | Copy all `draw_*` functions, `compute_tab_bar_width`, `calculate_container_inner_size`. Update type references: `TabState` → view-layer tab struct, `App` → new `App`, `WorkflowState` → data passed in from `WorkflowFrontend`. Remove any calls to the old command or workflow state machines. |
| `oldsrc/tui/pty.rs` | `src/frontend/tui/pty.rs` | Copy verbatim; update imports. This is pure PTY infrastructure (vt100 parser, channel bridge to the TUI event loop) with no business logic. |
| `oldsrc/tui/state.rs` — pure presentation types | `src/frontend/tui/` (split by concern) | Copy: `Focus`, `ContainerWindowState`, `ConfigDialogState`, `NewWorkflowDialogState`, `NewSkillDialogState`, `WorkflowField`, `RemoteTabBinding`, `STUCK_TIMEOUT`, `STUCK_DIALOG_BACKOFF`, `YOLO_COUNTDOWN_DURATION`. These are data-only types or pure constants. Do NOT copy the `App`/`TabState` struct definitions (replace with `Session`-backed types) or `PendingCommand` (replaced by Dispatch). |
| `oldsrc/tui/state.rs` — pure presentation methods | `src/frontend/tui/tabs.rs` | Copy `tab_color`, `tab_project_name`, `tab_subcommand_label`, `tab_display_name`, `background_yolo_color`, `background_yolo_label`, `window_border_color` as methods on the new per-tab view struct. Copy associated unit tests. |
| `oldsrc/tui/state.rs` — stuck / yolo timer logic | `src/frontend/tui/tabs.rs` | Copy `is_stuck`, `acknowledge_stuck`, `record_user_activity`, `dismiss_stuck_dialog` as methods on the new per-tab view struct. These are pure timer comparisons with no command semantics. |
| `oldsrc/tui/input.rs` — cursor movement helpers | `src/frontend/tui/text_edit.rs` | Copy all `handle_*_cursor` / `handle_worktree_commit_prompt` / `handle_worktree_pre_commit_message` key-handling functions verbatim into the shared `TextEditWidget`. These are pure text-buffer manipulations; adapt only the `Dialog` → typed dialog parameter. |
| `oldsrc/tui/input.rs` — dialog key handlers | `src/frontend/tui/dialogs/` | Copy individual dialog key-handling blocks (e.g. `handle_worktree_merge_prompt`, `handle_workflow_control_board`, `handle_agent_setup_confirm`) as methods on the corresponding dialog widget types. Adapt the `Action` return type to the new typed output enum for each dialog. |
| `oldsrc/tui/state.rs` — `Dialog` enum variants | `src/frontend/tui/dialogs/mod.rs` | Use as the exhaustive reference list of all dialogs that must exist in the new TUI. Each variant maps 1:1 to a dialog widget in `src/frontend/tui/dialogs/`. **Do not copy the enum itself** — the new dialogs use typed structs, not one fat enum. |

#### 8b. Files to reimplement from scratch (business logic entangled)

| oldsrc source | Replacement | Reason |
|---|---|---|
| `oldsrc/tui/mod.rs` — event loop body | `src/frontend/tui/app.rs` + `src/frontend/tui/mod.rs` | The old event loop calls into the old command handlers directly. New event loop dispatches all commands through `Dispatch`. Copy only the terminal setup/teardown boilerplate (raw mode, alternate screen, mouse capture, Kitty protocol). |
| `oldsrc/tui/mod.rs` — command submission | `src/frontend/tui/command_box.rs` | Old code parsed the command box string inline. New code hands the raw string to `Dispatch::parse_command_box_input`. |
| `oldsrc/tui/state.rs` — `App` / `TabState` | `src/frontend/tui/app.rs`, `src/frontend/tui/tabs.rs` | `TabState` mixes rendering state (presentational) with command-handling state (business logic). The new `App` owns `Terminal` + `SessionManager`; per-tab view state is a thin struct wrapping `Session`. |
| `oldsrc/tui/state.rs` — `PendingCommand` | Replaced by Dispatch | Business logic. |
| `oldsrc/tui/input.rs` — `Action` enum and top-level dispatch | `src/frontend/tui/per_command/` | The `Action` enum is the old command-dispatch surface. Each `Action` variant maps to a Layer 2 command invoked through `Dispatch`. Do not port the enum; instead implement each `*CommandFrontend` trait. |
| `oldsrc/tui/flag_parser.rs` | `src/frontend/tui/command_frontend.rs` | Old flag parser is a bespoke mini-parser. New code uses `Dispatch::parse_command_box_input` (Layer 2). |

#### 8c. Terminal setup / teardown — copy verbatim

The crossterm terminal initialization block in `oldsrc/tui/mod.rs` (raw mode, `EnterAlternateScreen`, `EnableMouseCapture`, Kitty `PushKeyboardEnhancementFlags` best-effort, and the corresponding cleanup on drop) MUST be copied verbatim into `src/frontend/tui/app.rs`. This is infrastructure, not business logic, and any deviation risks leaving the terminal in a broken state on panic or early exit.

#### 8d. Copy-then-prune workflow

The recommended workflow for TUI rendering files:

1. Copy the target oldsrc file into the new location.
2. Update `use` statements and type references (old → new).
3. Delete any function that calls into the old command system (these will not compile after step 2 anyway).
4. Implement the deleted functions fresh against the Dispatch/frontend-trait surface.
5. Run `cargo clippy` and fix warnings.

This ensures no visual element is accidentally dropped during the port.

## Edge Case Considerations:

- **Existing TUI tests**: `oldsrc/tui/state.rs` has substantial tests. They cannot run against the new TUI; reproduce the equivalent assertions against `Session` + `SessionManager` + the TUI's view code. ASK THE DEVELOPER if a particular test reveals a behavior that is not preserved.
- **`StartupReadyFlags`**: the legacy `main.rs` passes `--build`, `--no-cache`, `--refresh` into the TUI to be applied to a startup `ready` invocation. The new architecture handles this via `Dispatch` calling `ReadyCommand` at TUI startup; the TUI startup path constructs a `Dispatch` for `["ready"]` with the global flags pre-populated. The `ReadyCommand` then constructs a `ReadyEngine` with those options and runs it through `TuiReadyFrontend`. Confirm with developer whether this is the right model.
- **`ReadyEngine` and `InitEngine` non-interactive defaults**: when the TUI or headless frontend runs `ready` or `init` without a user present at the dialog (e.g. startup flags, headless API call), the frontend's Q&A methods MUST return safe defaults rather than blocking. The engine does not care — it calls the trait method and acts on the result. Each frontend is responsible for supplying those defaults; the engine has no `non_interactive` flag of its own (that was a legacy anti-pattern). If a caller wants non-interactive behavior, it implements a frontend that returns `false` / `None` for all decision methods.
- **Session lifetime in the TUI**: each tab owns one `Session`. Closing a tab removes the session from `SessionManager`. If a session has an in-flight container, `SessionManager::remove` must orchestrate cancellation through `ContainerExecution::cancel`. ASK THE DEVELOPER whether closing a tab forcibly kills running containers (legacy behavior) or prompts the user.
- **CLI vs TUI Session count**: `SessionManager::in_memory()` works for both single-session (CLI) and multi-session (TUI). Cover this with a unit test asserting both modes.
- **Headless multi-session concurrency**: each API session is a `Session`; `Dispatch::run_command` borrows the `Session` via the `Arc<RwLock<Session>>` provided to `Dispatch::new`. Long-running commands (chat, exec workflow) hold the read lock across the lifetime of the command. Verify this does not deadlock with concurrent inspection requests.
- **Error rendering parity**: every error message a user might see today must be reproducible by the new error rendering. Capture the existing user-visible strings (or close paraphrases) in `tests/cli_error_parity.rs` and assert.
- **Color and TTY detection**: `oldsrc/commands/output.rs` handles color/no-color logic. Move this to `src/frontend/cli/output.rs` (pure presentation).
- **Help text**: `clap` builds help from the catalogue. Compare `amux help` and `amux <subcommand> --help` output before and after; differences must be limited to noise (whitespace, version string, help-ordering).
- **TUI keyboard shortcut conflicts**: the new TUI adds no shortcuts; preserve every existing one. ASK THE DEVELOPER if any new shortcut is requested as part of this work item (default: no).
- **Tab close with running container**: legacy behavior is to **forcibly cancel** the running container without prompting. Preserve this — `SessionManager::remove(session_id)` calls `ContainerExecution::cancel` synchronously and propagates any cancel error as a `UserMessage::warning` rather than blocking the tab close. Cover with a unit test using a mock execution that records `cancel` calls.
- **Tab switching during yolo countdown**: leaving a tab while a `WorkflowYoloCountdown` modal is open MUST close the modal but keep the engine's countdown running (the engine doesn't know about tab switches). Re-entering the tab re-opens the modal at the engine's current remaining time. The TUI tracks `tab.yolo_countdown_started_at` for this purpose. Cover with a unit test.
- **Stuck-detection dismissal backoff**: per WI 0067 §9a.5, dismissing the yolo-countdown modal triggers a 60s backoff before the engine can re-fire `report_step_stuck`. The TUI also tracks per-step manual disabling via `auto_workflow_disabled_steps` (§7c). Cover both behaviors with unit tests asserting the correct interaction order.
- **CLI worktree dialog defaults**: when stdin is not a TTY (piped), the `CliWorktreeLifecycleFrontend` MUST NOT block on stdin reads. Instead, it returns the same safe-defaults as the headless frontend (§7q). Cover with a unit test using a `Cursor`-backed stdin.
- **Headless server lifecycle hand-off**: WI 0068 §6.4 introduces `HeadlessLifecycle`. The CLI frontend's `HeadlessStartCommandFrontend` impl drives the lifecycle: it calls `lifecycle.write_pid()`, opens the log for append, hands the assembled `HeadlessServeConfig` to `crate::frontend::headless::serve(...)`, and on shutdown calls `lifecycle.clear_pid()`. Cover with a unit test that asserts the PID lifecycle methods are invoked in order.
- **Mouse selection persistence**: a text selection in the container window persists across re-renders and only clears on (a) MouseDown for a new selection, (b) Esc when ExecutionWindow is focused, or (c) tab switch. Cover with a unit test using synthetic mouse events.
- **Clipboard fallback**: when the system clipboard is unavailable (no display server, OS support missing), Ctrl+Y emits `UserMessage::error("clipboard unavailable")` rather than panicking. Cover with a unit test using a fake `Clipboard` adapter that returns an error.
- **Read-only config fields**: the TUI's `ConfigShow` dialog renders `auto_agent_auth_accepted` as a read-only field with gray text and a tooltip on Enter. Cover with a unit test asserting Enter is rejected and the tooltip is rendered.

## Test Considerations:

### Test philosophy (read first)

Tests for Layer 3 + Layer 4 are **designed and written from scratch** alongside the new frontends. **Do not port tests from `oldsrc/tui/**/#[cfg(test)] mod tests`, `oldsrc/commands/headless/**/#[cfg(test)]`, or `oldsrc/cli.rs` test blocks.** The old TUI tests assume `TabState` plus business-logic-in-the-frontend; the old headless tests assume the legacy ad-hoc routing; the old CLI tests assume a parameter-style command surface. All of these are explicitly designed away.

There are two narrow exceptions:

**Exception A — presentation-layer tests** (same rule as §8a for code): tests that exercise pure-presentation functions (e.g. `tab_color`, `tab_subcommand_label`, `compute_tab_bar_width`, `window_border_color`, cursor-movement helpers) SHOULD be adapted from `oldsrc/tui/state.rs` if they satisfy all of: (1) the tested function is being copied per §8a, (2) the test compiles with only mechanical type-reference updates, (3) no legacy command or engine types appear. These tests are the fastest way to verify that visual parity is maintained; bring them forward by default.

**Exception B — other tests** must satisfy **all** of the following:

1. Asserts a user-visible behavior the new frontend MUST preserve (e.g. exact help-text format, exact SSE wire format, exact keyboard-shortcut set, exact prompt text in a confirmation dialog).
2. Compiles unchanged or with mechanical edits against the new frontend types.
3. Exercises only Layer 0 + 1 + 2 + 3 (and Layer 4 for binary-level tests). No legacy types.

If any old test is brought forward under Exception B, the PR description MUST list it with a one-sentence justification. The default answer for Exception B is "rewrite from scratch."

This work item produces **only Layer 3 unit tests and pure-presentation snapshot tests** plus a **manual sign-off checklist** that gates 0070. The full parity test suite, the real-Docker / real-network end-to-end tests, and the freshly rebuilt top-level `tests/` directory are 0070's responsibility. **Do not create any file under `tests/` in this work item.**

### Unit tests (colocated `#[cfg(test)] mod tests`)

- **CLI** (`src/frontend/cli/`):
  - `CliCommandFrontend::flag_bool / flag_string / flag_strings / flag_path / flag_enum / argument` correctly extract values from a synthesized `clap::ArgMatches` for every `FlagKind` in the catalogue (data-table test).
  - `render_outcome_for_cli` snapshot per `*Outcome` variant — uses `insta` or equivalent to lock the rendered stdout.
  - `render_error_for_cli` snapshot per `CommandError` variant — locks the rendered stderr including exit code mapping.
  - TTY-vs-pipe rendering decisions (color on, hyperlinks on/off, etc.) are unit-tested with a `Termios`-style abstraction.
- **TUI** (`src/frontend/tui/`):
  - `App` event loop processes a synthetic key event sequence and updates `SessionManager` as expected (open tab, close tab, switch tab — one test per shortcut, driven by a data table of `(key, expected_state_delta)`).
  - Command-box submit forwards the raw string to a mocked `Dispatch::parse_command_box_input` and routes the parsed result back through `Dispatch::run_command` with the expected path + flags.
  - `TuiWorkflowFrontend::user_choose_next_action` renders the dialog with the data passed in, simulates a user keypress, and returns the typed `NextAction`. (Pure unit test — no real terminal.)
  - Dialog widgets (selection list, confirmation, text input) snapshot-tested with `insta` against synthetic inputs and key sequences.
  - Hint rendering pulls from `CommandCatalogue::tui_hint_for` — assert the hint text comes from the catalogue, not a hard-coded string in the TUI.
  - Tab close with an in-flight container calls `ContainerExecution::cancel` on the right execution (mock the engine).
  - `TuiReadyFrontend::report_phase` for each `ReadyPhase` variant updates the expected TUI component state (data-table test over all variants).
  - `TuiClawsFrontend::report_phase` for each `ClawsPhase` variant updates the expected TUI component state (data-table test over all variants).
  - `TuiClawsFrontend::ask_replace_existing_clone` opens the correct dialog; key `'y'` returns `true`, `'n'`/Esc returns `false`.
  - `TuiWorkflowFrontend::user_choose_next_action` with `AvailableActions { can_continue_in_current_container: false, .. }` renders without the continue option and returns only from the available set.
  - `TuiWorkflowFrontend::user_choose_next_action` with `AvailableActions { can_cancel_to_previous_step: false, cancel_to_previous_unavailable_reason: Some("this is the first step"), .. }` renders the option as disabled with the reason string visible.
  - Selecting `RestartCurrentStep` from the dialog returns `NextAction::RestartCurrentStep` (data-table test over all available action variants).
  - `TuiWorktreeLifecycleFrontend::ask_pre_worktree_uncommitted_files` with key `'c'` transitions to `WorktreePreCommitMessage` dialog with default message pre-populated. Ctrl+Enter submits with the typed message. `'a'`/Esc returns `PreWorktreeDecision::Abort`.
  - `TuiWorktreeLifecycleFrontend::ask_post_workflow_action` with key `'m'` returns `PostWorkflowWorktreeAction::Merge`; `'d'` returns `Discard`; `'s'`/Esc returns `Keep`.
  - `TuiWorktreeLifecycleFrontend::ask_worktree_commit_before_merge`: editable text box with cursor navigation — left/right/home/end/backspace/delete key handling matches `oldsrc/tui/input.rs::handle_worktree_commit_prompt` (data-table test over cursor movements).
  - `TuiUserMessageSink::write_message` appends to the per-tab status log; status log renders messages in insertion order; `replay_queued` is confirmed to be a no-op (log is unchanged afterward).
  - **Tab color matrix** (per §7a): for each `(execution_phase, focus, container_state, is_stuck, is_remote)` tuple the rendered tab color matches the legacy specification. Drive via a data-table test.
  - **Tab subcommand label**: the alternating yolo indicator (`⚠️  yolo in Ns` / `🤘 yolo in Ns`) renders correctly across two consecutive renders 2 seconds apart (drive with `tokio::time::pause`).
  - **Container window state cycling** (Ctrl+M): Hidden → Minimized → Maximized → Hidden. Cover with a data-table test.
  - **Focus transitions**: ↑ from CommandBox with running container moves focus to ExecutionWindow; Esc from ExecutionWindow returns focus to CommandBox.
  - **`WorkflowControlBoard` arrow-key matrix** (per §7c): every key in the legend maps to the correct `NextAction`. Data-table test.
  - **`WorkflowControlBoard` Ctrl+Enter** on the last step returns `NextAction::FinishWorkflow`; on a non-last step it is visually disabled and Ctrl+Enter is a no-op.
  - **`WorkflowControlBoard` 'd' key** sets `tab.auto_workflow_disabled_steps[current_step]`; subsequent `yolo_countdown_tick` calls return `YoloTickOutcome::Cancel` for that step.
  - **`WorkflowYoloCountdown` modal** dismissed via Esc returns `YoloTickOutcome::Cancel` AND triggers a 60s backoff (`STUCK_DIALOG_BACKOFF`) before the next `report_step_stuck` fires. Drive with `tokio::time::pause`.
  - **`WorkflowStepError` modal** (per §7e): `[r]` returns `Retry`, `[q]` returns `Pause`, `[a]` returns `Abort`. Data-table test.
  - **`AgentSetupConfirm` modal** (per §7f): renders the fallback option only when `default_available` is true and `requested != default`; `[f]` records a fallback in `tab.workflow_agent_fallbacks`.
  - **Workflow agent-fallback caching**: when the cache contains the requested agent, `AgentSetupFrontend::ask_agent_setup` is NOT called for that agent in the same workflow run. (Layer 3 caching — verify by mocking the frontend.)
  - **`MountScope` modal** (per §7g): `[r]` → MountGitRoot, `[c]` → MountCurrentDirOnly, `[a]`/Esc → Abort.
  - **`AgentAuthConsent` modal** (per §7h): `[y]` persists `auto_agent_auth_accepted = true`; `[n]` persists false; `[o]`/Esc → DeclineOnce (no persistence).
  - **`ConfigShow` dialog** (per §7i): edit-mode key sequence `Enter` → typing → `Ctrl+S` saves to the right config file; `Esc` reverts; read-only field rejects Enter.
  - **`ConfigShow` validation**: setting an invalid `agent` value displays an inline red error message; the cell stays in edit mode.
  - **TUI startup branching** (per §7p): in-repo path runs `["ready"]`; not-in-repo path runs `["status", "--watch"]`. Verify both with a fake `git_root_resolver`.
  - **Tab close with in-flight container** calls `ContainerExecution::cancel` synchronously, NOT after a confirmation dialog (legacy behavior).
  - **Mouse selection** persists across re-renders; clears on MouseDown for a new selection, Esc, or tab switch.
  - **Clipboard fallback**: Ctrl+Y on a fake clipboard that errors emits `UserMessage::error("clipboard unavailable")` and does NOT panic.
  - **Levenshtein typo correction**: `Dispatch::parse_command_box_input("imp")` returns an error containing `"did you mean: implement?"`. (Catalogue helper test, but rendered by the TUI.)
  - **Per-tab `auto_workflow_disabled_steps` reset**: when a step transitions from `Failed`/`Succeeded` back to `Pending` (e.g. via `RestartCurrentStep`), the disabled flag is cleared. Cover with a unit test.
- **Headless** (`src/frontend/headless/`):
    - Route-parity assertion: define a `const EXPECTED_ROUTES: &[(&str, &str)]` table of `(method, path)` pairs copied verbatim from `oldsrc/commands/headless/server.rs::build_router`'s route registrations, and assert that the new `build_router` registers every entry. This test fails if a route is missing — it is the mechanical guard that the HTTP API surface has not changed.
  - `POST /v1/commands` handler: send a `CreateCommandRequest { subcommand: "implement", args: vec!["--chat", "hello"] }` with a valid `x-amux-session` header to a test `AppState` with a mocked `Dispatch::run_command`, and assert (a) the handler returned 202 Accepted with a `command_id`, (b) `Dispatch::run_command` was called with the command path `["implement"]` and a `HeadlessCommandFrontend` that returns `"hello"` for the `--chat` flag.
  - `HeadlessCommandFrontend::parse_command_path` correctly derives the path for each known top-level command and nested command (e.g. `"exec" + ["workflow", ...]` → `["exec", "workflow"]`). Data-table test.
  - Auth middleware: token mode rejects bad tokens with 401, accepts good tokens with the expected response; disabled mode emits `X-Amux-Auth: disabled`; TLS-required mode rejects non-loopback bind without TLS.
  - SSE/WebSocket adapter (`HeadlessContainerFrontend`) writes stdout chunks in the expected wire format against a mocked stream sink — pure unit test, no real container.
  - Error translation: each `CommandError` variant maps to the documented HTTP status code and JSON error body.
- **Layer 4** (`src/main.rs`):
  - The body of `main` is small enough to test indirectly. Add a single integration-style unit test (still colocated, still no real binary) that runs the same logic with a synthetic argv and asserts the right frontend (cli vs tui) is selected.
  - Cargo bin compiles without warnings (CI guard).

### What does NOT belong in this work item

- Tests in the top-level `tests/` directory. Leave it untouched; 0070 rebuilds it from scratch.
- Tests that exercise the real `amux` binary as a subprocess.
- Tests that start a real headless HTTP server bound to a real port.
- Tests that launch a real TUI in a real terminal (or a `vt100`/`expect`-style terminal harness).
- Tests that hit a real Docker daemon, real git remote, or real network.
- Parity tests against the pre-refactor binary's output. Those are 0070.

### Build & CI

- `cargo build --release` produces a single statically-linked `amux` binary from `src/main.rs` (after the `Cargo.toml` swap).
- `cargo test` passes including the new Layer 3 unit tests.
- `cargo clippy --all-targets -- -D warnings` passes.
- `make all`, `make install`, `make test` work.

### Manual sign-off checklist (gating 0070)

This work item is the last point at which the legacy `oldsrc/` is still in the repo. Before merging, the implementing agent MUST manually exercise the new binary against a real environment and post a sign-off checklist in the PR description. **Automated parity tests are not yet written** — they are 0070's deliverable — so this manual pass is what catches regressions before 0070 deletes the legacy code.

The PR description MUST include:

- A table listing every command and subcommand documented in `aspec/uxui/cli.md`, each marked PASS / MINOR-DRIFT (with one-sentence justification) / REGRESSION (block).
- A confirmation that the TUI was launched on a real terminal, every documented keyboard shortcut was exercised, at least 3 tabs were opened, an `exec workflow` was run end-to-end (with at least one user dialog), and rendering was visually identical (or improved with documented justification) to pre-refactor.
- A confirmation that the headless server was started, every documented endpoint received a real `curl` invocation, and responses were wire-compatible with pre-refactor.

Any item that is REGRESSION blocks the PR. The implementing agent MUST fix or escalate to the developer. Do not merge with open regressions.

The corresponding **automated** tests for all of the above are written in 0070, against the freshly rebuilt `tests/` directory.

## Codebase Integration:

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `aspec/uxui/cli.md` for user-facing behavior; nothing in this work item changes that surface.
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is 0070.
- Do not introduce business logic in `src/frontend/`. If you find yourself wanting to, the missing surface is in Layer 2.
- Do not introduce upward calls. Use traits.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, MUST include the parity smoke-test checklist, and MUST list every developer-clarification question raised.
- After this work item lands, the next agent picks up `0070-grand-architecture-finalize-and-remove-oldsrc.md`.
