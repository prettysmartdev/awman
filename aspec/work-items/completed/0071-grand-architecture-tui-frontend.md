# Work Item: Task

Title: grand architecture refactor — TUI frontend
Issue: n/a — sixth-of-eight work item implementing `aspec/architecture/2026-grand-architecture.md`

## Prerequisites

All Layer 0 (data), Layer 1 (engine), Layer 2 (command/dispatch), and the CLI frontend (Layer 3) are complete and tested in prior work items (0066–0070). The CLI frontend in `src/frontend/cli/` serves as the reference implementation for how a frontend implements the trait system. The old TUI in `oldsrc/tui/` serves as the visual/behavioral reference for what the new TUI must reproduce.

The implementing agent MUST read:

- `aspec/architecture/2026-grand-architecture.md` end-to-end — this is the source of truth for the layered architecture.
- The current state of `src/data/`, `src/engine/`, `src/command/`, and `src/frontend/cli/` — these are the real layers the TUI frontend will call into.
- `oldsrc/tui/` (mod.rs, state.rs, render.rs, input.rs, pty.rs) — the legacy TUI whose behavior must be reproduced.

## Architecture tenets

These four tenets govern every decision in this work item:

1. **Frontends contain NO business logic.** This is the most heavily enforced tenet. Any `if`, `match`, or computed-default behavior that depends on the *meaning* of a command, flag, or response is wrong and lives in Layer 2. Frontends parse keystrokes into `CommandFrontend` answers and render typed outcomes back. That is all.
2. **Lower layers never call upward.** Layer 1/2 code uses frontend traits (e.g. `WorkflowFrontend`, `ContainerFrontend`) to delegate user interaction to Layer 3. The TUI implements these traits.
3. **Typed objects over `pub fn`.** Build structs with well-understood options that expose public methods, rather than standalone pub functions.
4. **When uncertain, ASK THE DEVELOPER.** Do not make assumptions about behavior, defaults, or architecture decisions.

## Scope

Build `src/frontend/tui/` — a complete Ratatui-based interactive terminal UI. After this work item:
- `main.rs` dispatches bare `amux` invocations to `tui::run`
- The TUI exhibits user-perceptible parity with the legacy TUI in `oldsrc/tui/`
- Every keyboard shortcut, dialog, tab behavior, workflow control, and rendering detail matches pre-refactor

---

## 1. Required Layer 2 additions

The TUI requires several Layer 2 helpers that do not yet exist because no frontend has needed them until now. These are the ONLY Layer 2 changes permitted in this work item:

### 1a. `Dispatch::parse_command_box_input`

Add to `src/command/dispatch/mod.rs`:

```rust
pub fn parse_command_box_input(text: &str, catalogue: &CommandCatalogue)
    -> Result<(Vec<String>, clap::ArgMatches), CommandBoxParseError>
```

- Tokenizes command-box text into argv-style tokens (respecting shell-like quoting)
- Runs the tokens through the catalogue's clap app via `try_get_matches_from`
- On parse failure, returns `CommandBoxParseError` with:
  - The invalid token
  - A Levenshtein suggestion (threshold ≤4) from `CommandCatalogue::command_names()`
  - The original clap error message
- This method is pure parsing — no side effects, no I/O

### 1b. `CommandCatalogue::tui_completions`

Add to `src/command/dispatch/catalogue.rs`:

```rust
pub fn tui_completions(&self, partial: &str) -> Vec<String>
```

- Returns all command paths (e.g. `exec prompt`, `exec workflow`, `config show`) that prefix-match the partial input
- Used by the TUI command box for autocomplete suggestions
- Sorted alphabetically, deduped

### 1c. `CommandCatalogue::tui_hint_for`

Add to `src/command/dispatch/catalogue.rs`:

```rust
pub fn tui_hint_for(&self, command_path: &[&str]) -> Option<String>
```

- Returns a one-line hint string for the given command path (e.g. `"exec workflow <path> [--yolo] [--worktree]"`)
- Built from the catalogue's `CommandSpec` and `FlagSpec` metadata
- Used by the TUI suggestion row and status bar

### 1d. `RemoteCommandFrontend` interactive methods

The `RemoteCommandFrontend` trait (currently a marker trait in `src/command/commands/remote.rs`) needs interactive methods for TUI dialogs. Add:

```rust
pub trait RemoteCommandFrontend: UserMessageSink + Send + Sync {
    fn ask_session_picker(&mut self, sessions: &[RemoteSessionInfo]) -> Result<RemoteSessionId, CommandError>;
    fn ask_saved_dir_picker(&mut self, saved_dirs: &[PathBuf]) -> Result<PathBuf, CommandError>;
    fn ask_session_kill_picker(&mut self, sessions: &[RemoteSessionInfo]) -> Result<RemoteSessionId, CommandError>;
    fn confirm_save_dir(&mut self, dir: &Path) -> Result<bool, CommandError>;
}
```

The CLI implementation should prompt on stdin (with TTY-aware defaults: first session, first dir, first session, `false`). The TUI implementation will open the corresponding picker dialogs.

### 1e. Unit tests for Layer 2 additions

- `parse_command_box_input`: data-table test for valid commands, invalid commands with suggestions, edge cases (empty input, partial commands, quoted strings)
- `tui_completions`: prefix matching, empty prefix returns all, no-match returns empty
- `tui_hint_for`: every top-level command has a hint, nested subcommands have hints, unknown path returns None

---

## 2. `src/frontend/tui/` — files and structure

Build these files under `src/frontend/tui/`:

### `mod.rs` — TUI entry point

```rust
pub async fn run(catalogue: &CommandCatalogue, engines: Engines, session_manager: SessionManager) -> Result<(), TuiError>
```

- Captures the terminal: raw mode, alternate screen, mouse capture, Kitty keyboard protocol (best-effort, non-fatal on failure)
- Constructs the `App` state
- Enters the event loop
- On exit: restores terminal, drops alternate screen

### `app.rs` — Application state

`App` struct — the central state object:

- `tabs: Vec<Tab>` — ordered tab list, each bound to a `Session`
- `active_tab: usize` — index into `tabs`
- `active_dialog: Option<Dialog>` — currently open modal dialog (one at a time)
- `focus: Focus` — enum: `CommandBox` or `ExecutionWindow`
- `catalogue: Arc<CommandCatalogue>`
- `engines: Engines`
- `session_manager: Arc<RwLock<SessionManager>>`
- `suggestion_row: Vec<String>` — current autocomplete suggestions
- `status_bar: StatusBar` — bottom status line

The `App` struct must NOT contain business logic. It stores UI state only. All command execution delegates to `Dispatch` and the per-command frontend trait chain.

### `tabs.rs` — Tab state

`Tab` struct — per-tab state:

- `session: Session` — the Layer 0 session bound to this tab
- `execution_phase: ExecutionPhase` — `Idle`, `Running { command: String }`, `Done { command: String, exit_code: i32 }`, `Error { command: String, message: String }`
- `pty: Option<PtySession>` — active pseudo-terminal, if any
- `vt100_parser: vt100::Parser` — terminal output parser with configurable scrollback (default 10000 lines)
- `container_window_state: ContainerWindowState` — `Hidden`, `Minimized`, `Maximized`
- `workflow_state: Option<WorkflowViewState>` — visible workflow status when a workflow is running
- `status_log: Vec<(MessageLevel, String)>` — per-tab message history
- `status_log_collapsed: bool` — toggled with `l` key
- `scroll_offset: usize` — execution window scrollback position
- `mouse_selection: Option<TextSelection>` — current mouse text selection
- `workflow_agent_fallbacks: HashMap<String, String>` — per-step agent fallback cache
- `auto_workflow_disabled_steps: HashSet<String>` — steps where auto-advance was manually disabled
- `is_remote: bool` — true when tab is bound to a remote session
- `is_claws: bool` — true when tab is running a claws command

### `command_box.rs` — Command input area

- Wraps `TextEdit` for the input field
- On Enter: tokenize input, pass to `Dispatch::parse_command_box_input`, handle result
- On Tab/Shift+Tab: cycle through suggestions from `CommandCatalogue::tui_completions`
- Suggestion row renders: `> sugg1 · sugg2 · sugg3 · …` separated by middots
- When no suggestions and a session is active: show `CWD: /path` or `Using Worktree: /path`
- Invalid commands: display `did you mean: <suggestion>?` in red
- Ctrl+T: opens NewTabDirectory dialog (not handled here — routed to keymap)

### `command_frontend.rs` — TUI's CommandFrontend implementation

`TuiCommandFrontend` — implements `CommandFrontend` + all per-command frontend traits:

- Constructed from the command-box parse result (clap `ArgMatches` from `Dispatch::parse_command_box_input`)
- `flag_bool`, `flag_string`, etc. read from the ArgMatches (same pattern as CLI's `CliFrontend`)
- Implements `UserMessageSink` by appending to the active tab's `status_log`
- Per-command trait methods open modal dialogs (see §3 below) and block until the user responds

Unlike the CLI frontend which uses stdin prompts, the TUI frontend opens modal dialogs for every interactive Q&A method. The dialog is pure presentation; the typed action enum it returns is defined in Layer 2.

### `per_command/` — One file per command's frontend trait implementation

One file for each per-command frontend trait, implementing the TUI dialog for each interactive method:

| File | Trait | Dialog methods |
|------|-------|----------------|
| `init.rs` | `InitCommandFrontend` (via `InitFrontend`) | `ask_replace_aspec`, `ask_run_audit`, `ask_work_items_setup`, `report_phase` |
| `ready.rs` | `ReadyCommandFrontend` (via `ReadyFrontend`) | `ask_create_dockerfile`, `ask_run_audit_on_template`, `ask_migrate_legacy_layout`, `report_phase` |
| `claws.rs` | `ClawsCommandFrontend` (via `ClawsFrontend`) | `confirm_sudo_actions`, `confirm_restart_stopped`, `confirm_offer_init`, `ask_replace_existing_clone`, `ask_run_audit`, `report_phase` |
| `chat.rs` | `ChatCommandFrontend` | `set_pty_active` |
| `exec_prompt.rs` | `ExecPromptCommandFrontend` | `set_pty_active` (default no-op) |
| `exec_workflow.rs` | `ExecWorkflowCommandFrontend` | `set_pty_active`, `report_workflow_summary` |
| `implement.rs` | `ImplementCommandFrontend` | `set_pty_active`, `report_implement_summary` |
| `specs.rs` | `SpecsCommandFrontend` | `ask_spec_kind`, `ask_spec_title`, `ask_spec_summary`, `ask_interview_summary` |
| `new.rs` | `NewCommandFrontend` | `ask_workflow_name`, `ask_workflow_summary`, `ask_skill_name`, `ask_skill_summary` |
| `config.rs` | `ConfigCommandFrontend` | (marker — config show dialog is handled in dialogs/) |
| `status.rs` | `StatusCommandFrontend` | `tui_context`, `should_continue_watching`, `write_clear_marker` |
| `auth.rs` | `AuthCommandFrontend` | `ask_consent` |
| `remote.rs` | `RemoteCommandFrontend` | `ask_session_picker`, `ask_saved_dir_picker`, `ask_session_kill_picker`, `confirm_save_dir` |
| `headless.rs` | `HeadlessCommandFrontend` | (marker) |
| `download.rs` | `DownloadCommandFrontend` | (marker) |
| `agent_setup.rs` | `AgentSetupFrontend` | `ask_agent_setup` — opens agent setup confirm dialog |
| `agent_auth.rs` | `AgentAuthFrontend` | `ask_agent_auth_consent` — opens agent auth consent dialog |
| `mount_scope.rs` | `MountScopeFrontend` | `ask_mount_scope` — opens mount scope dialog |
| `container_frontend.rs` | `ContainerFrontend` | `write_stdout`, `write_stderr` → feed PTY's vt100 parser; `read_stdin` → read from PTY writer; `resize_pty`; `report_status`; `report_progress` |
| `workflow_frontend.rs` | `WorkflowFrontend` | `user_choose_next_action` → open workflow control board; `confirm_resume` → resume mismatch dialog; `user_choose_after_step_failure` → step error dialog; `report_step_stuck/unstuck`; `yolo_countdown_tick`; `report_workflow_completed` |
| `worktree_lifecycle.rs` | `WorktreeLifecycleFrontend` | All worktree Q&A methods → corresponding modal dialogs |

### `container_view.rs` — PTY/container output rendering

- Renders the vt100 parser output into a ratatui widget
- Overlay window centered at 95% of terminal width/height
- Cycles through Hidden → Minimized → Maximized → Hidden (Ctrl+M)
- Minimized: single-line summary showing last output line

### `workflow_view.rs` — Workflow status strip

- Horizontal strip showing: step name, status (pending/running/done/error), progress indicators
- Workflow control board modal (see §3c)

### `ready_view.rs`, `init_view.rs`, `claws_view.rs` — Phase-by-phase progress display

- Each renders the corresponding engine's phase progression as a modal dialog with phase indicators and messages
- Phase transitions update the dialog in-place

### `dialogs/` — Pure-presentation dialog widgets

All modal dialog implementations. Each dialog:
- Renders centered in the terminal with a colored border and title
- Captures all keyboard input while open (modal)
- Returns a typed Layer 2 enum value when the user responds
- Cancellable with Esc (returns None or a cancel variant)

See §3 below for every dialog specification.

### `text_edit.rs` — Shared text editing widget

- Single-line and multiline modes
- Cursor movement: Left, Right, Home, End, Ctrl+Left (word), Ctrl+Right (word)
- Editing: Backspace, Delete, Ctrl+Backspace (word delete)
- Multiline: Enter inserts newline, Ctrl+Enter or Ctrl+S submits

Copy-adapt from `oldsrc/tui/` text editing helpers (pure presentation, no business logic).

### `pty.rs` — Pseudo-terminal management

- `PtySession` wraps `portable-pty` for interactive shell sessions
- `PtyEvent` enum: `Data(Vec<u8>)`, `Exit(i32)`
- Background threads: reader (PTY → channel), wait (exit code), writer (keystrokes → PTY)
- `spawn_text_command()` — async task for non-PTY text commands (init, ready)

Copy verbatim from `oldsrc/tui/pty.rs` — this is pure I/O plumbing with no business logic.

### `keymap.rs` — Keyboard shortcut definitions

Defines the complete keyboard shortcut map. All shortcuts are defined here, not scattered across handlers:

| Context | Key | Action |
|---------|-----|--------|
| Global | Ctrl+T | Open NewTabDirectory dialog |
| Global | Ctrl+A | Switch to previous tab |
| Global | Ctrl+D | Switch to next tab |
| Global | Ctrl+C | Close tab (multi-tab) or quit (single-tab) |
| Global | Ctrl+M | Cycle container window state |
| Global | Ctrl+, | Open config show dialog |
| CommandBox | Enter | Submit command |
| CommandBox | Tab | Next autocomplete suggestion |
| CommandBox | Shift+Tab | Previous autocomplete suggestion |
| CommandBox | ↑ | Move focus to ExecutionWindow (when running) |
| ExecutionWindow | Esc | Move focus back to CommandBox |
| ExecutionWindow | ↑/↓ | Scroll one line |
| ExecutionWindow | PageUp/PageDown | Scroll one page |
| ExecutionWindow | b | Scroll to top |
| ExecutionWindow | e | Scroll to live (bottom) |
| ExecutionWindow | Ctrl+Y | Copy mouse selection to clipboard |
| ExecutionWindow | l | Toggle status log collapsed/expanded |
| Dialog | Esc | Cancel/dismiss dialog |
| Dialog | (per-dialog keys) | See §3 |

### `render.rs` — UI chrome rendering

Adapted from `oldsrc/tui/render.rs`. Responsible for:

- **Frame layout** (top to bottom):
  1. Tab bar (3 rows)
  2. Execution window (Min 5 rows, fills remaining)
  3. Optional minimized container summary bar
  4. Optional workflow strip
  5. Status bar (1 row)
  6. Command box (3 rows)
  7. Suggestion row (1 row)

- **Tab bar rendering**: horizontal tabs with project names and subcommand labels
- **Execution window**: PTY output or text command output with scrollback
- **Container overlay**: centered at 95% width/height when Maximized
- **Status bar**: shows git root, agent name, current session info
- **Welcome message**: two lines of dark gray when idle: `"Welcome to amux."` and `"Running 'amux ready' to check your environment..."`

### `hints.rs` — TUI hint text

Pulls all hint text via `CommandCatalogue::tui_hint_for`. No hardcoded command or flag strings.

### `user_message.rs` — TUI message sink

`TuiUserMessageSink` implementing `UserMessageSink`:
- Appends each message to the active tab's `status_log` with level-colored prefix
- `Info`: dim gray prefix
- `Warning`: yellow prefix
- `Error`: red prefix
- `Success`: green prefix
- Auto-scrolls to bottom unless user has scrolled up
- `replay_queued()`: replays all queued messages (relevant during PTY-active periods)

### `worktree_lifecycle_frontend.rs`

Implements `WorktreeLifecycleFrontend` with modal dialogs for all worktree decisions:
- Pre-commit warning, commit message input, merge prompt, merge confirm, delete confirm
- See §3 (worktree dialogs) for exact specifications

---

## 3. Behavioral parity — dialog and component specifications

The TUI must preserve, with zero user-visible drift, every behavior specified below. These specifications are authoritative — implement each TUI component against them.

### 3a. Tab management — colors, indicators, focus

**Tab color matrix** (based on execution state):

| State | Color |
|-------|-------|
| Stuck (agent silent > agentStuckTimeout) | Yellow |
| Remote-bound (is_remote = true) | Magenta |
| Error (execution phase = Error) | Red |
| Running + PTY active | Green |
| Running + no container | Blue |
| Running + claws | Magenta |
| Idle / Done | Dark Gray |

**Tab rendering**:
- Active tab: `➡ project` with TOP+LEFT+RIGHT borders
- Yolo countdown active in background: tab label alternates between `⚠️  yolo in Ns` and `🤘 yolo in Ns` every 2 seconds
- Stuck tabs: prepend `⚠️ ` to the command label
- Tab name truncates at 14 characters with `…`

**Tab width algorithm**:
- 1 tab: 1/4 terminal width
- 2 tabs: 1/2 width each
- 3 tabs: 3/4 width each
- 4+ tabs: full width / n

**Execution window border**:

| State | Focused | Color |
|-------|---------|-------|
| Running | ExecutionWindow | Blue |
| Running | CommandBox | Gray |
| Done | ExecutionWindow | Green |
| Done | CommandBox | Gray |
| Error | any | Red |
| Idle | any | DarkGray |

**Execution window phase labels** (in the border title):
- Idle: ` amux `
- Running: ` ● running: {command} `
- Done: ` ✓ done: {command} `
- Error: ` ✗ error: {command} (exit {exit_code}) `

### 3b. Command box and autocomplete

- Tab/Shift+Tab cycle through suggestions from `CommandCatalogue::tui_completions`
- Suggestion row format: `> sugg1 · sugg2 · sugg3 · …` separated by middots
- When no suggestions and session active: `CWD: /path` or `Using Worktree: /path`
- Standard text editing: Backspace, Delete, Home, End
- Ctrl+T opens NewTabDirectory dialog (handled by keymap, not command box)
- Invalid commands: `Dispatch::parse_command_box_input` returns structured error → render `did you mean: <suggestion>?` in red
- Levenshtein threshold for suggestions: ≤4

### 3c. Workflow control board — exact key matrix

Opens when `WorkflowFrontend::user_choose_next_action` is called.

**Layout**: centered modal, yellow rounded border, 52 cols × 13–15 rows. Title: ` Workflow Control `

**Key mappings**:

| Key | Action | Maps to |
|-----|--------|---------|
| → (Right) | Advance to next step | `NextAction::LaunchNext` |
| ↓ (Down) | Continue in current container | `NextAction::ContinueInCurrentContainer` |
| ↑ (Up) | Restart current step | `NextAction::RestartCurrentStep` |
| ← (Left) | Go back to previous step | `NextAction::CancelToPreviousStep` |
| Ctrl+Enter | Finish workflow | `NextAction::FinishWorkflow` |
| Ctrl+C | Abort | Opens abort confirmation sub-dialog |
| d | Disable auto-advance for current step | `NextAction::DisableAutoAdvanceForCurrentStep` |
| Esc | Close dialog (keeps engine running) | Dialog dismissed |

- Disabled/unavailable actions render in dark gray with an `unavailable_reason` tooltip
- Only actions present in `AvailableActions` are enabled

### 3d. Workflow stuck detection and yolo countdown

- `report_step_stuck`: tab turns yellow, `⚠️ ` prepends command label, status bar shows stuck message
- `report_step_unstuck`: tab returns to green, prefix and status bar reset
- `yolo_countdown_tick(remaining)`: opens WorkflowYoloCountdown modal
  - Magenta border, shows step name and seconds remaining
  - Dismissible with Esc (60-second backoff before re-opening)
  - Per-tab `auto_workflow_disabled_steps` flag suppresses re-opening for dismissed steps even though engine continues ticking

### 3e. Workflow step error dialog

Opens when `WorkflowFrontend::user_choose_after_step_failure` is called.

- Title: ` Step failed ` (red border)
- Body: step name + first N lines of failure output
- Keys:
  - `[r]` or `[1]` → `StepFailureChoice::Retry`
  - `[q]` or `[2]` or Esc → `StepFailureChoice::Pause`
  - `[a]` → `StepFailureChoice::Abort`

### 3f. Agent setup confirmation dialog

Opens when `AgentSetupFrontend::ask_agent_setup` is called.

- Title varies: ` Set up <agent>? ` or ` Build <agent> image? `
- Body: explains the situation and lists planned actions
- Keys:
  - `[y]` or Enter → `AgentSetupDecision::Setup`
  - `[f]` → `AgentSetupDecision::FallbackToDefault` (only when default agent is available and != requested)
  - `[n]` or Esc → `AgentSetupDecision::Abort`
- Per-step fallback caching: `Tab::workflow_agent_fallbacks: HashMap<AgentName, AgentName>` prevents re-prompting for the same agent within a workflow run

### 3g. Mount scope dialog

Opens when `MountScopeFrontend::ask_mount_scope` is called.

- Title: ` Mount Scope `
- Body: shows both paths (git root and cwd)
- Keys:
  - `[r]` → `MountScope::MountGitRoot`
  - `[c]` → `MountScope::MountCurrentDirOnly`
  - `[a]` or Esc → `MountScope::Abort`

### 3h. Agent auth consent dialog

Opens when `AgentAuthFrontend::ask_agent_auth_consent` is called.

- Title: ` Agent credentials? `
- Body: lists env-var names that will be injected into the container
- Keys:
  - `[y]` → `AuthConsentChoice::Accept` (persists `auto_agent_auth_accepted = true`)
  - `[n]` → `AuthConsentChoice::Decline` (persists `auto_agent_auth_accepted = false`)
  - `[o]` or Esc → `AuthConsentChoice::DeclineOnce` (no persistence)

### 3i. Config show dialog

Opens when `config show` is run from the TUI command box.

- Full-screen interactive table with columns: Field | Global | Repo | Effective
- Arrow keys navigate rows
- Enter enters edit mode for the selected field
- Edit mode: Ctrl+S saves, Esc cancels
- Read-only fields (e.g. `auto_agent_auth_accepted`) render in gray and reject Enter with a tooltip
- Validation errors display inline in red
- Ctrl+, from anywhere opens this dialog (global shortcut)

### 3j. New-artefact dialogs

For `specs new`, `new workflow`, `new skill`:

- `NewKindSelect`: `[1]` Feature `[2]` Bug `[3]` Task `[4]` Enhancement
- `NewTitleInput`: single-line text input, Ctrl+Enter submits
- `NewInterviewSummary`: multiline editor with cursor navigation, Ctrl+Enter submits
- `NewWorkflow`: multi-field form (name, step count, per-step prompts), Tab cycles between fields
- `NewSkill`: multi-field form (name, description, body), Tab cycles between fields
- All use the shared `text_edit.rs` widget

### 3k. Claws dialogs

One dialog variant for each `ClawsFrontend` interactive method:

- `HasForked`: informs user about existing fork, options to proceed or abort
- `UsernameInput`: prompts for GitHub username (single-line text input)
- `SudoConfirm`: confirms sudo actions with `[y]`/`[n]`
- `DockerSocketWarning`: warns about Docker socket exposure, `[y]`/`[n]`
- `OfferRestartStopped`: offers to restart a stopped container, `[y]`/`[n]`
- `OfferStart`: offers to start a container, `[y]`/`[n]`
- `RestartFailedOfferFresh`: restart failed, offers fresh start, `[y]`/`[n]`
- `AuditConfirm`: confirms running audit, `[y]`/`[n]`

### 3l. Quit and tab-close dialogs

- `QuitConfirm`: triggered by Ctrl+C with a single tab. `[y]` quits, `[n]`/Esc cancels.
- `CloseTabConfirm`: triggered by Ctrl+C with multiple tabs. `[q]` quits entire app, `[c]` closes just this tab, `[n]`/Esc cancels.

### 3m. PTY container view

- `vt100::Parser` instance with configurable scrollback buffer (default 10000 lines)
- Both stdout and stderr feed the same vt100 parser (no visual distinction)
- Scrollback navigation:
  - ↑/↓: scroll one line
  - PageUp/PageDown: scroll one page
  - `b`: scroll to top (beginning)
  - `e`: scroll to live (end/bottom)
- Mouse support:
  - MouseDown: anchors selection start
  - MouseDrag: extends selection
  - MouseUp: finalizes selection
  - Ctrl+Y: copies selection to clipboard
- Clipboard fallback: if clipboard access fails, emit `UserMessage::error` rather than panicking
- Kitty keyboard protocol: enabled best-effort on startup; non-fatal on failure
- Carriage-return spinner overwrite: vt100 parser handles this natively

### 3n. Tab status log via UserMessageSink

- Per-tab status log with level-colored prefixes:
  - Info: dim gray
  - Warning: yellow
  - Error: red
  - Success: green
- Auto-scrolls to bottom unless user has scrolled up
- Press `l` to toggle between collapsed (1-line summary showing most recent message) and expanded (scrollable list)

### 3o. Status command — TUI tab annotations

When `amux status` is run from the TUI:
- `TuiStatusCommandFrontend` populates `StatusCommandTuiContext` with snapshots of all open tabs
- Each running container's row in the status output is decorated with the tab number when the container's name matches a tab's bound container
- Includes: tab number, container name, is_stuck flag, command label
- These annotations do NOT appear in CLI or headless mode

### 3p. TUI startup behavior

1. Capture terminal: raw mode, alternate screen, mouse capture, Kitty keyboard protocol
2. Construct initial tab at cwd
3. **If in a git repo**: build a `Dispatch` for `["ready"]` with startup flags (e.g. `--non-interactive` if appropriate), run through the standard `TuiReadyFrontend` trait chain
4. **If NOT in a git repo**: build a `Dispatch` for `["status", "--watch"]`
5. Enter event loop

The startup invocation runs through the standard Dispatch → Command → Frontend chain. No special-cased business logic in `App::new`.

Cover both branches with a unit test using a fake `git_root_resolver`.

### 3q. Remote session picker dialogs

For `remote run`, `remote session start`, `remote session kill`:

- `RemoteSessionPicker`: list of available sessions, arrow-key selection, Enter selects
- `RemoteSavedDirPicker`: list of saved directories from config, arrow-key selection, Enter selects
- `RemoteSessionKillPicker`: list of sessions with kill confirmation
- `RemoteSaveDirConfirm`: `[y]` saves, `[n]` skips

All fetch data asynchronously and show a "loading…" placeholder while waiting.

### 3r. Status command TIPS and CLEAR_MARKER

- TIPS array: displayed after status output, one random tip per second (`unix_seconds % TIPS.len()`)
- CLEAR_MARKER (`\x1b[2J\x1b[H`): emitted before each re-render in `--watch` mode
  - CLI forwards CLEAR_MARKER to terminal
  - TUI swallows CLEAR_MARKER (it handles re-rendering itself)

---

## 4. Code reuse policy

### 4a. Copy-and-adapt (pure presentation files)

These files may be copied from `oldsrc/tui/` and mechanically adapted to the new type system:
- `render.rs` — UI chrome rendering functions
- `pty.rs` — PTY management (verbatim copy is preferred)
- Dialog widgets — visual layout and rendering
- Cursor-movement helpers in text editing
- `tab_color`, `tab_subcommand_label`, `compute_tab_bar_width`, `window_border_color` — pure state-to-color/string mapping functions

"Adapted" means: change type imports to point at the new Layer 0/1/2 types, remove any embedded business logic (move to Layer 2), preserve visual output.

### 4b. Reimplement from scratch

These components embedded business logic in the old TUI and MUST be reimplemented:
- Event loop (`oldsrc/tui/mod.rs::run_app`) — the new event loop delegates to Dispatch, not to command functions
- Command submission — now goes through `Dispatch::parse_command_box_input` + `Dispatch::run_command`
- `App`/`TabState` — replaced by `App`/`Tab` with `Session` instead of `TabState`
- `PendingCommand` — replaced by the per-command frontend trait chain
- `flag_parser.rs` — replaced by `CommandCatalogue` + clap parsing

### 4c. Test adaptation

Pure-presentation tests from `oldsrc/tui/state.rs` (e.g. `tab_color`, `tab_subcommand_label`, `compute_tab_bar_width`, `window_border_color`, cursor-movement helpers) SHOULD be adapted when the corresponding production code is being adapted — fastest path to confirming visual parity.

Other tests require justification: the test must (1) assert a precise visual invariant, (2) compile with mechanical edits against new types, and (3) add coverage no new test provides.

---

## 5. Startup branching in `main.rs`

Update `src/main.rs` to dispatch bare `amux` invocations (no subcommand) to `tui::run` instead of the current placeholder that prints a notice and exits.

The TUI branch constructs:
1. `SessionManager` (in-memory, not persisted — headless persistence is WI 0072)
2. All engines (same construction as the CLI branch)
3. An initial `Session` at cwd
4. Calls `tui::run(catalogue, engines, session_manager)`

---

## 6. Test layout and philosophy

**Only Layer 3 unit tests and pure-presentation snapshot tests.** The full parity test suite (real-Docker, real-network, end-to-end tests) and the `tests/` directory rebuild happen in WI 0073. **Do not create any file under `tests/` in this work item.**

### Unit tests to include

- **Tab state**: `tab_color` mapping for every execution phase, `compute_tab_bar_width` for 1/2/3/4+ tabs, `window_border_color` matrix
- **Command box**: autocomplete cycling, suggestion row rendering, invalid command error display
- **Keyboard shortcuts**: every key in keymap.rs produces the expected Action enum variant
- **Dialog responses**: for each dialog in §3, verify that key sequences produce the correct typed Layer 2 enum values
- **Per-command frontend traits**: for each `*CommandFrontend` Q&A method:
  - Dialog opens on the right phase
  - Key sequence produces the right typed output
  - Esc cancels and returns the appropriate cancel variant
- **Parse command box input** (Layer 2 addition): valid commands, invalid with suggestions, edge cases
- **Startup branching**: in-repo vs not-in-repo produces the right Dispatch command path (use fake git_root_resolver)
- **Rendering snapshots**: key render functions produce expected ratatui Buffer output for known inputs
- **UserMessageSink**: messages appear in tab status log with correct level prefixes

### Build & CI

- `cargo build --release` produces a single statically-linked `amux`
- `cargo test` passes including the new Layer 3 TUI unit tests
- `cargo clippy --all-targets -- -D warnings` passes
- `make all`, `make install`, `make test` work

---

## 7. Manual sign-off checklist (gating WI 0072)

The PR description MUST include:

- A confirmation that the TUI was launched on a real terminal, every documented keyboard shortcut was exercised, at least 3 tabs were opened, an `exec workflow` was run end-to-end (with at least one user dialog), and rendering was visually identical (or improved with documented justification) to pre-refactor.
- A table of every dialog from §3a–§3r marked PASS / MINOR-DRIFT (one-sentence justification) / REGRESSION (block). A REGRESSION blocks the PR.
- A confirmation that `oldsrc/` was NOT touched (other than possibly `oldsrc/README.md`).

---

## What must NOT happen in this work item

- **No business logic in `src/frontend/tui/`.** If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2; ASK THE DEVELOPER about adding it.
- **No deletion of `oldsrc/`.** That is WI 0073.
- **No edits inside `oldsrc/`** other than possibly `oldsrc/README.md`.
- **No new commands, new flags, or new user-visible behavior.** This work item is parity only.
- **No headless work.** That is WI 0072.
- **No Layer 1/2 changes beyond those enumerated in §1.** Every other gap discovered during TUI implementation is logged in `aspec/review-notes/0071-followups.md` and addressed in WI 0073, unless the gap blocks TUI parity (in which case ASK THE DEVELOPER).
- **No tests under `tests/`.** WI 0073 owns that tree.

---

## Edge Case Considerations

- **Tab close with running container**: forcibly cancels via `ContainerExecution::cancel` (real after WI 0070); no confirmation prompt.
- **Tab switching during yolo countdown**: closes the modal but keeps the engine's countdown running.
- **Stuck-detection dismissal backoff**: 60 seconds before re-firing after Esc dismissal.
- **Mouse selection persistence**: selection must persist across re-renders until a new selection is started.
- **Clipboard fallback**: emit `UserMessage::error` rather than panicking when clipboard is unavailable.
- **Read-only config fields**: in the ConfigShow dialog, reject Enter with a tooltip; render field in gray.
- **Per-tab `auto_workflow_disabled_steps`**: reset when a step transitions back to `Pending`.
- **Terminal resize during execution**: dynamic tab widths recalculate, PTY resize propagates to container.
- **UTF-8 in command box**: full Unicode support in text editing and display.
- **Rapid keystroke during dialog transition**: queue keystrokes, process after dialog is fully rendered.
- **Empty command box submission**: no-op (do not send empty string to Dispatch).
- **Very long command in command box**: horizontal scrolling, no line wrapping.
- **Many tabs (10+)**: tab bar scrolls or truncates gracefully.

---

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- The CLI frontend in `src/frontend/cli/` is the reference implementation for trait patterns.
- `oldsrc/tui/` is the visual/behavioral reference for what to reproduce.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is WI 0073.
- Do not introduce business logic in `src/frontend/tui/`.
- Do not introduce upward calls — use traits.
- The PR description MUST link to this work item, MUST include the TUI parity smoke-test checklist, and MUST list every developer-clarification question raised.
- After this work item lands, the next agent picks up `0072-grand-architecture-headless-frontend.md`.
