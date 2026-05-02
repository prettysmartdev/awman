# Work Item: Task

Title: grand architecture refactor — TUI frontend
Issue: n/a — sixth-of-eight work item implementing `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item builds the TUI frontend on top of the now-real Layer 1/2 implementations completed in `0070-grand-architecture-layer-1-2-completion-and-cli.md`. The implementing agent **MUST** read:

- `aspec/architecture/2026-grand-architecture.md` end-to-end.
- `0066-…` through `0069-…` (the foundation work items).
- `0070-grand-architecture-layer-1-2-completion-and-cli.md` (the Layer 1/2 + CLI completion work item — this WI's prerequisite).
- `0069-…` §2 + §7a–§7r + §8a–§8d (the original TUI section and parity addenda — they remain authoritative for TUI specifics; this WI references them rather than restating).
- The current state of `src/data/`, `src/engine/`, `src/command/`, and `src/frontend/cli/`.

The four tenets, again:

1. **Frontends contain NO business logic.** This is the most heavily enforced tenet of this work item. Any `if`, `match`, or computed-default behavior that depends on the *meaning* of a command, flag, or response is wrong and lives in Layer 2. Frontends parse keystrokes/HTTP/argv into `CommandFrontend` answers and render typed outcomes back. That is all.
2. **Lower layers never call upward.** Use frontend traits to delegate user input from Layer 1/2 up to Layer 3.
3. **Typed objects over `pub fn`.**
4. **When uncertain, ASK THE DEVELOPER.**

The companion work items are:

- `0066-grand-architecture-foundation-and-layer-0-data.md` (merged)
- `0067-grand-architecture-layer-1-engines.md` (merged)
- `0068-grand-architecture-layer-2-command-and-dispatch.md` (merged)
- `0069-grand-architecture-layer-3-frontends-and-binary.md` (merged)
- `0070-grand-architecture-layer-1-2-completion-and-cli.md` (must be merged)
- `0072-grand-architecture-headless-frontend.md`
- `0073-grand-architecture-finalize-and-remove-oldsrc.md`

## Scope

Build `src/frontend/tui/` per `0069-…` §2 and the §7 addenda. After this work item, `main.rs` MUST dispatch bare invocations to `tui::run` and the TUI MUST exhibit user-perceptible parity with the legacy TUI.

The §1 in this WI is intentionally short because the heavy lifting was specified in `0069-…` §2. Read that section as the implementation guide; the bullets below capture the deltas, the gating conditions specific to this WI, and the test layout.

### 1. `src/frontend/tui/` — files and structure

Per `0069-…` §2, build these files:

- `mod.rs`, `app.rs`, `tabs.rs`, `command_box.rs`, `command_frontend.rs`, `per_command/` (one file per command), `container_view.rs`, `workflow_view.rs`, `ready_view.rs`, `init_view.rs`, `claws_view.rs`, `dialogs/`, `text_edit.rs`, `pty.rs`, `keymap.rs`, `render.rs`, `hints.rs`, `user_message.rs`, `worktree_lifecycle_frontend.rs`.

Follow `0069-…` §8 (Code Reuse Policy) — copy-and-adapt for pure presentation files (`render.rs`, `pty.rs`, dialog widgets, cursor-movement helpers); reimplement from scratch where the legacy code embedded business logic in the TUI (event loop, command submission, `App`/`TabState`, `PendingCommand`, `flag_parser.rs`).

### 2. Behavioral parity checklist

The TUI must preserve, with zero user-visible drift, every behavior listed in `0069-…` §2 "Behavioral parity checklist" + the §7a–§7r addenda. That list is treated as authoritative — re-read it when implementing each TUI component. Notable items (not exhaustive — the §7 addenda are):

- Tab opening/closing/switching (every shortcut), per-tab `Session` state, command box behavior, container window rendering, workflow control dialog, yolo countdown rendering, stuck-agent detection, status bar, every keyboard shortcut, error rendering, `amux ready` and `amux init` phase-by-phase progress display with modal dialogs, worktree pre-creation and post-completion flows, `UserMessageSink` per-tab status log.
- The `per_command/` files implement the corresponding `*CommandFrontend` traits — every Q&A method introduced in `0070-…` §1 (e.g. `SpecsCommandFrontend::ask_kind`, `NewCommandFrontend::ask_workflow_name`, `ClawsCommandFrontend::confirm_sudo_actions`) MUST have a TUI dialog implementation here. The dialog is pure presentation; the typed action enum it returns is defined in Layer 2.

### 3. Startup branching

Per `0069-…` §7p: the TUI's startup path constructs a `Dispatch` for `["ready"]` (when in a git repo) or `["status", "--watch"]` (when not) and runs it through the standard frontend trait chain — no special-cased business logic in `App::new`. Cover both branches with a unit test using a fake `git_root_resolver`.

### 4. Test layout and philosophy

Same philosophy as `0069-…` §"Test Considerations": **only Layer 3 unit tests and pure-presentation snapshot tests**. The full parity test suite, the real-Docker / real-network end-to-end tests, the `tests/` directory rebuild, and the cross-frontend integration suite are 0073's responsibility. **Do not create any file under `tests/` in this work item.**

The unit-test catalogue from `0069-…` §"Test Considerations" — TUI section — is treated as authoritative; copy-paste applies. Notable additions beyond what was originally listed (because 0070 added new Q&A methods):

- Per `SpecsCommandFrontend` Q&A method (`ask_kind`, `ask_title`, `ask_summary`, `ask_interview_summary`): dialog opens on the right phase, key sequence produces the right typed output, Esc cancels.
- Per `NewCommandFrontend` Q&A method (`ask_workflow_name`, per-step prompts, `ask_skill_*`, `ask_interview_summary`): same.
- Per `ClawsCommandFrontend` confirmation (`confirm_sudo_actions`, `confirm_restart_stopped`, `confirm_offer_init`): correct dialog variant opens, `[y]/[n]` returns the right typed action.
- Per `ConfigCommandFrontend` set/get/show: the `ConfigShow` dialog (already specified in §7i) renders every field returned by `ConfigShowOutcome.fields`; read-only fields reject Enter; Ctrl+S persists.
- `StatusCommandFrontend` TUI annotations (already specified in §7o): every running container's row is decorated with the tab number when the container's name matches a tab's bound container.

### 5. Manual sign-off checklist (gating 0072)

The PR description MUST include:

- A confirmation that the TUI was launched on a real terminal, every documented keyboard shortcut was exercised, at least 3 tabs were opened, an `exec workflow` was run end-to-end (with at least one user dialog), and rendering was visually identical (or improved with documented justification) to pre-refactor.
- A table of every dialog from §7a–§7r marked PASS / MINOR-DRIFT (one-sentence justification) / REGRESSION (block).
- A confirmation that `oldsrc/` was NOT touched (other than possibly `oldsrc/README.md`).

## What must NOT happen in this work item

- No business logic in `src/frontend/tui/`. If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2; ASK THE DEVELOPER about adding it.
- No deletion of `oldsrc/`. That is `0073-…`.
- No edits inside `oldsrc/` other than possibly the `oldsrc/README.md` note.
- No new commands, new flags, or new user-visible behavior. This work item is *parity only*.
- No headless work. That is `0072-…`.
- No Layer 1/2 changes — every gap discovered during TUI implementation is logged in `aspec/review-notes/0071-followups.md` and addressed in 0073, unless the gap blocks TUI parity (in which case ASK THE DEVELOPER).

## Edge Case Considerations

The full edge-case list lives in `0069-…` §"Edge Case Considerations" (TUI subset); copy-paste applies. Notable reaffirmations:

- **Tab close with running container** forcibly cancels via `ContainerExecution::cancel` (now real after 0070); no confirmation prompt.
- **Tab switching during yolo countdown** closes the modal but keeps the engine's countdown running.
- **Stuck-detection dismissal backoff** (60s) prevents re-firing.
- **Mouse selection persistence** across re-renders.
- **Clipboard fallback** emits `UserMessage::error` rather than panicking.
- **Read-only config fields** in the `ConfigShow` dialog reject Enter with a tooltip.
- **Per-tab `auto_workflow_disabled_steps`** reset when a step transitions back to `Pending`.

## Test Considerations

### Test philosophy

Tests for Layer 3 TUI are **designed and written from scratch** alongside the new TUI. Per `0069-…` §"Test Considerations" Exception A, pure-presentation tests from `oldsrc/tui/state.rs` (e.g. `tab_color`, `tab_subcommand_label`, `compute_tab_bar_width`, `window_border_color`, cursor-movement helpers) SHOULD be adapted when the corresponding production code is being adapted per §8a — fastest path to confirming visual parity. Tests under Exception B (other tests) require all-three-criteria justification per `0069-…`.

This work item produces **only Layer 3 unit tests and pure-presentation snapshot tests** plus a **manual sign-off checklist** that gates 0072. **Do not create any file under `tests/`** in this work item.

### Build & CI

- `cargo build --release` produces a single statically-linked `amux`.
- `cargo test` passes including the new Layer 3 TUI unit tests.
- `cargo clippy --all-targets -- -D warnings` passes.
- `make all`, `make install`, `make test` work.

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `0069-…` §2, §7a–§7r, §8a–§8d for TUI specifics — copy verbatim where applicable rather than re-deriving.
- Follow `0070-…` for the typed surfaces the TUI's `*CommandFrontend` impls bind against.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is `0073-…`.
- Do not introduce business logic in `src/frontend/tui/` — if a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2.
- Do not introduce upward calls — use traits.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, MUST include the TUI parity smoke-test checklist, and MUST list every developer-clarification question raised.
- After this work item lands, the next agent picks up `0072-grand-architecture-headless-frontend.md`.
