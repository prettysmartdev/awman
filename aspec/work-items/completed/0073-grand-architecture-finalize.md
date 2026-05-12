# Work Item: Task

Title: grand architecture refactor — final parity validation, docs and aspec refresh
Issue: n/a — eighth and final work item implementing `aspec/architecture/2026-grand-architecture.md`

## Prerequisites

All eight layers of the grand architecture refactor are complete:

- **Layer 0 (data)**: `src/data/` — config, filesystem, session, workflow state, headless persistence (WIs 0066, 0072)
- **Layer 1 (engine)**: `src/engine/` — container runtime, git, overlay, auth, agent, workflow engines (WIs 0067, 0072)
- **Layer 2 (command)**: `src/command/` — dispatch, catalogue, all command bodies (WIs 0068, 0070, 0071, 0072)
- **Layer 3 (frontend)**: `src/frontend/cli/` + `src/frontend/tui/` + `src/frontend/headless/` (WIs 0069, 0070, 0071, 0072)
- **Layer 4 (binary)**: `src/main.rs` (WI 0069)

Every command body is real, all three frontends are functionally complete. The remaining work is verification and documentation. The developer will delete `oldsrc/` manually after manual testing is satisfactory — this work item does NOT include that deletion.

The implementing agent MUST read:

- `aspec/architecture/2026-grand-architecture.md` end-to-end — the source of truth for the layered architecture and its tenets.
- The entire `src/` tree — this is the code being validated and the sole survivor of the refactor.
- `oldsrc/` (briefly, for comparison during parity validation) — do not edit it.

When uncertain about any gap discovered during validation, ASK THE DEVELOPER rather than papering over it.

## Summary

- **Build a fresh integration and end-to-end test suite from scratch** under `tests/`, designed against the new four-layer architecture. Nothing is ported from the legacy `tests/` directory by default.
- Run the resulting suite as a comprehensive parity validation pass. Capture results in `aspec/review-notes/0073-parity-validation.md`.
- Audit `src/` against every architecture tenet. Fix any violations. Produce `aspec/review-notes/0073-architecture-audit.md`.
- Audit `src/` against the functionality of oldsrc, focusing on TUI, Headless, and Remote mode completeness, compatibility with on-disk files/directories, databases, and the handling of workflows, containers, security, and complex flows like `init` and `ready`. 
- Clean up stale placeholder comments and TODO markers left from prior work items.
- Refresh `docs/` and `aspec/` to describe the new architecture with no pre-refactor references.
- Add `make architecture-lint` target enforcing layering rules.

**NOTE:** Deletion of `oldsrc/`, legacy `tests/`, and legacy `benches/` is NOT part of this work item. The developer will perform those deletions manually after manual testing is satisfactory.

## User Stories

### User Story 1:
As a: maintainer
I want to: have the new architecture validated for full parity with `oldsrc/` so I can confidently delete it
So I can: trust that the new `src/` tree is a complete replacement before removing the legacy code.

### User Story 2:
As a: future implementing agent or contributor
I want to: read up-to-date `docs/` and `aspec/` that describe the four-layer architecture with no stale references
So I can: ramp up quickly and not be misled by outdated instructions.

### User Story 3:
As a: maintainer adding a new feature six months from now
I want to: have a `make architecture-lint` check that fails CI on upward imports
So I can: catch tenet violations at PR time rather than during review.

---

## Implementation Details

### 0. Ground rules

- Read the entire `src/` tree before writing any code.
- `oldsrc/` exists for comparison during parity validation. Do not edit it. The developer will delete it manually after testing.
- When uncertain, ASK THE DEVELOPER.

### 1. Build the new `tests/` tree from scratch

Work items 0066–0072 produced **only colocated unit tests** (plus the route-parity guard in WI 0072). This work item is where every cross-layer integration test, every real-Docker / real-git / real-network end-to-end test, every binary-level smoke test, and every parity test is written.

**Do not port files from the pre-refactor `tests/` directory.** Those tests target legacy command entry points, untyped flags, and frontend-conflated business logic. The legacy `tests/` directory will remain until the developer deletes it manually alongside `oldsrc/`. The narrow exception for porting: a single test file or fixture that satisfies ALL THREE of:

1. Asserts a precise wire-format or on-disk invariant the new architecture must preserve (e.g. headless SSE chunk format, workflow-state JSON schema, `.amux.json` schema, SQLite migration compatibility).
2. Compiles unchanged or with mechanical edits against the new types.
3. Adds coverage no new test in this work item already provides.

If any old test is brought forward, the PR description MUST list it with a one-sentence justification.

#### 1a. Proposed `tests/` layout

```
tests/
  data_layer/                      # Layer 0 cross-module integration
    config_session_roundtrip.rs
    sqlite_upgrade_compat.rs       # opens a fixture DB written by the prior amux release
  engine/                          # Layer 1 — real-system tests
    container_docker.rs            # real Docker daemon required
    container_apple.rs             # real Apple containers required (cfg(target_os = "macos"))
    workflow_end_to_end.rs         # real Docker, three-step workflow
    ready_engine.rs                # real Docker, real git; full ReadyPhase state machine
    init_engine.rs                 # real Docker, real git; full InitPhase state machine
    claws_engine.rs                # real Docker, real git; full ClawsPhase state machine
    agent_engine.rs                # real Docker; ensure_available download+build path
    git_engine.rs                  # real git init; worktree create/merge/remove cycle
    worktree_lifecycle.rs          # real git; full prepare→run→finalize cycle
    overlay_engine.rs              # real filesystem with canonicalization edge cases
    auth_engine_tls.rs             # real rustls cert generation, fingerprint stability
  command/                         # Layer 2 against real Layers 0+1
    dispatch_real_engines.rs       # Dispatch::run_command end-to-end
  cli_parity/                      # Layer 3 CLI parity
    help_text.rs                   # golden-file: amux help, amux <sub> --help
    init.rs
    ready.rs
    exec_workflow_worktree.rs
    user_messages.rs
    chat.rs
    exec_prompt.rs
    exec_workflow.rs
    claws.rs
    status.rs
    specs.rs
    config.rs
    headless.rs
    remote.rs
    new.rs
    auth.rs
    download.rs
    json_outputs.rs                # every --json command's JSON schema
  tui_parity/                      # Layer 3 TUI parity (vt100/expect-style harness)
    startup_and_tabs.rs
    command_box.rs
    workflow_dialog.rs
    yolo_countdown.rs
    keyboard_shortcuts.rs
    rendering_snapshots.rs
    new_dialogs.rs
    config_show_dialog.rs
    claws_dialogs.rs
    worktree_dialogs.rs
  headless_parity/                 # Layer 3 headless API
    routes.rs
    auth_modes.rs
    tls.rs
    sse_wire_format.rs
    websocket_wire_format.rs
    refresh_key_banner.rs
    background_daemonize.rs
  binary_smoke/                    # Layer 4 — invokes the real binary
    cli_subprocess.rs
    tui_subprocess.rs
    headless_subprocess.rs
  fixtures/
    sqlite_upgrade/<version>.db
    cli_help/<command>.txt
    headless_openapi.json
    workflow_state/v1.json
    ready_json/<scenario>.json
  helpers/
    docker_skip.rs
    test_repo.rs
    test_session.rs
    recording_frontend.rs
```

The exact layout MAY differ — ASK THE DEVELOPER before the file plan ossifies — but the coverage must include every category above.

#### 1b. What each tier covers

- **`tests/data_layer/`** — Layer 0 multi-module exercises. Always hermetic (tempfile, no network). Includes SQLite upgrade compatibility fixture.
- **`tests/engine/`** — Layer 1 against real systems. Real Docker, real git, real filesystem, real rustls. Gated behind `helpers::docker_skip`.
- **`tests/command/`** — Layer 2 wired into real Layers 0+1 (no fakes).
- **`tests/cli_parity/`** — for every command in `aspec/uxui/cli.md`, exercise the binary as a subprocess and assert stdout/stderr/exit-code match checked-in golden fixtures.
- **`tests/tui_parity/`** — drive the TUI under a vt100-style terminal harness. Snapshot tests must be deterministic (no wall-clock leakage; drive time with `tokio::time::pause`).
- **`tests/headless_parity/`** — start the server bound to an ephemeral loopback port; issue real `reqwest` calls; assert wire compatibility with checked-in fixtures.
- **`tests/binary_smoke/`** — exercise the real binary as a subprocess.

#### 1c. Real-system gating

Every test needing Docker, Apple containers, git, or network MUST be gated by `helpers::docker_skip!` that skips with a clear message. Add:

- `make test-full` — runs everything
- `make test-fast` — skips real-system tests
- CI runs `make test-full` on at least one runner per supported OS with Docker

### 2. Comprehensive parity validation

Produce `aspec/review-notes/0073-parity-validation.md` capturing all results.

#### 2a. CLI parity

- Run `tests/cli_parity/`; capture pass/fail per command
- For any drift: MINOR-DRIFT (justify, freeze new fixture, get developer sign-off) or REGRESSION (block)
- Manually run `amux help`, `amux <sub> --help` for every level and spot-check

#### 2b. TUI parity

- Run `tests/tui_parity/`; capture pass/fail per scenario
- The implementing agent MUST launch the new TUI on a real terminal and walk through:
  - Launch → tab list visible → status bar correct
  - Open multiple tabs, switch, close
  - Run `exec workflow` from command box; complete a workflow; exercise the workflow control dialog
  - Run a multi-step workflow with `--yolo`; observe auto-advance countdown
  - Trigger an error path and confirm error rendering
  - Resize terminal during execution
  - Exercise every documented keyboard shortcut
- Capture screenshots or terminal recordings

#### 2c. Headless parity

- Run `tests/headless_parity/`; capture pass/fail per endpoint
- Manually start headless server; confirm bind, TLS, auth banner match pre-refactor
- Issue a real `curl` to every documented endpoint; record any drift

#### 2d. Sign-off rule

Every parity entry must be PASS or have explicit developer-approved MINOR-DRIFT before this work item is considered complete. REGRESSIONs block the PR. The developer will use the parity report to decide when manual testing and `oldsrc/` deletion can proceed.

#### 2e. Parity validation matrix — explicit coverage requirements

The following specific behaviors MUST each have at least one targeted test. Track each as a row in `aspec/review-notes/0073-parity-validation.md` with PASS / MINOR-DRIFT / REGRESSION.

**Command surface parity** (against the `amux` binary as a subprocess unless noted):

1. `amux init --agent <claude|codex|opencode|maki|gemini|copilot|crush|cline> --aspec` runs to completion and produces `.amux/config.json` + `Dockerfile.dev` + aspec tree (data-table over agents).
2. `amux ready --refresh --build --no-cache --non-interactive --allow-docker --json` produces machine-readable JSON with documented schema.
3. `amux ready --json` implies `--non-interactive` (no prompts fire with stdin attached).
4. `amux ready` does NOT prompt to migrate legacy single-Dockerfile layout when `.amux/Dockerfile.<agent>` already exists.
5. `amux implement 0001 [--workflow PATH] [--worktree] [--yolo] [--auto] [--plan] [--agent NAME] [--model NAME] [--non-interactive] [--allow-docker] [--mount-ssh] [--overlay SPEC]…` runs end-to-end. Cover the implication rule (`--yolo + --workflow ⇒ --worktree`).
6. `amux chat [flags]` runs interactively (PTY); `amux chat -n` runs non-interactively. Verify exit code propagation.
7. `amux specs new --interview` prompts for kind+title+summary+interview, creates file at `aspec/work-items/<NNNN>-<slug>.md`, hands to agent.
8. `amux specs amend 0042 [-n] [--allow-docker]` runs agent against existing work-item file.
9. `amux new spec` is an alias for `amux specs new`.
10. `amux new workflow [--interview] [--global] [--format toml|yaml|md]` creates workflow at right location in right format.
11. `amux new skill [--interview] [--global]` creates skill at right location.
12. `amux claws init` / `claws ready` / `claws chat` run multi-phase flows end-to-end.
13. `amux status [--watch]` prints legacy ASCII table with TIPS; `--watch` re-renders every 3s with CLEAR_MARKER (CLI forwards marker, TUI swallows it).
14. `amux config show` / `config get FIELD` / `config set FIELD VALUE [--global]` for every documented field; invalid value rejected; unknown field returns Levenshtein suggestions.
15. `amux exec prompt "..."` runs non-interactively with non-empty prompt validator.
16. `amux exec workflow PATH [--work-item NUM] [--yolo|--auto|--worktree] …` runs end-to-end. The `wf` alias works.
17. `amux headless start [--port] [--workdirs] [--background] [--refresh-key] [--dangerously-skip-auth]` starts server; `--refresh-key` prints legacy banner once and exits; `--background` daemonizes.
18. `amux headless kill` / `headless logs` / `headless status` work against running server. Stale-PID detection on `kill`.
19. `amux remote run -- exec prompt "hi" --yolo` forwards trailing args correctly (verify `--yolo` reaches remote without "unknown flag" errors). `--follow` streams SSE until completion.
20. `amux remote session start /path` / `session kill SESSION_ID` round-trip through headless API.
21. `amux auth` consent flow: prompts `[y]/[n]/[o]`; persists to GlobalConfig. `amux auth --refresh-key` regenerates key and prints legacy banner.
22. `amux download <asset>` writes asset to disk with correct permissions.

**Engine behavior parity** (from `tests/engine/`):

23. `AgentEngine::ensure_available` per supported agent: download → build → image_exists → idempotent.
24. `AgentEngine::build_options` per-agent matrix produces correct `Vec<ContainerOption>`.
25. `OverlayEngine::agent_settings_overlays(claude)` strips `oauthAccount`, applies denylist, injects yolo settings, suppresses LSP, detects non-root `USER`.
26. `OverlayEngine::agent_settings_overlays` for non-Claude agents produces correct single-dir overlay.
27. `AuthEngine::agent_keychain_credentials` returns right env-var pairs from fake keychain.
28. `AuthEngine::resolve_agent_auth` honors `auto_agent_auth_accepted`.
29. `AuthEngine::ensure_self_signed_tls` produces cert with correct SAN; idempotent; stable fingerprint.
30. `AuthEngine::refresh_api_key` writes hash file with mode 0600; returns plaintext.
31. `WorkflowEngine` end-to-end: 3-step DAG with `LaunchNext`, `ContinueInCurrentContainer`, `RestartCurrentStep`, `CancelToPreviousStep`, `FinishWorkflow`, `Pause`, `Abort`, and `StepFailureChoice::Retry`.
32. Workflow stuck detection: agent silent > `agentStuckTimeout` → `report_step_stuck`; new output → `report_step_unstuck`; yolo → `yolo_countdown_tick` at 1 Hz.
33. Workflow file parsing: `.md`, `.toml`, `.yaml` produce identical `Workflow` structs.
34. Prompt template substitution: `{{work_item_number}}`, `{{work_item_content}}`, `{{work_item_section:[Name]}}` work; missing work item → empty + warning.
35. Workflow state persistence: save/load round-trip; legacy fallback path migration works.
36. `ContainerRuntime::detect`: Docker on Linux, Apple on macOS-with-config, error on mismatch, Docker-with-warning on unknown.
37. `DockerContainerInstance::run_with_frontend` against real Docker: spawns container, streams stdout/stderr, captures exit code, supports cancel.
38. `DockerBackend::list_running` against real Docker: returns all amux-labeled containers.
39. `DockerBackend::stats` against real container: returns CPU/memory.
40. `DockerBackend::stop` cleanly stops + removes.
41. Image tags: `<repo-hash>:latest` and `<repo-hash>:<agent>:latest` match legacy fingerprint.
42. `GitEngine` worktree path: `~/.amux/worktrees/<repo-name>/0042/`, branch: `amux/work-item-0042`.
43. `GitEngine::merge_branch` uses `git merge --squash` + `git commit -m "Implement <branch>"`.
44. `InitEngine` end-to-end: writes aspec, Dockerfile, config, optional build, optional audit.
45. `ReadyEngine` end-to-end: legacy migration phase only fires when per-agent Dockerfile absent.
46. `ClawsEngine` end-to-end for each `ClawsMode`.

**TUI behavior parity** (from `tests/tui_parity/` against vt100 harness):

47. Tab management: Ctrl+T opens NewTabDirectory, Ctrl+A/D switch, Ctrl+C closes/quits.
48. Tab color matrix: yellow (stuck), magenta (remote), red (error), green (PTY+running), blue (running no PTY), magenta (claws), dark gray (idle/done).
49. Tab subcommand label: alternating `⚠️  yolo in Ns` / `🤘 yolo in Ns` every 2s.
50. Container window cycling: Ctrl+M → Hidden → Minimized → Maximized → Hidden.
51. Focus transitions: ↑ from CommandBox to ExecutionWindow; Esc back.
52. Workflow control board: every key (→/↓/↑/←/Ctrl+Enter/Ctrl+C/d/Esc) exercised.
53. Workflow yolo countdown: opens after 30s stuck; auto-advances after 60s; Esc dismisses with 60s backoff.
54. Workflow step error: [r] retry / [q] pause / [a] abort.
55. Agent setup confirm: [y] setup / [f] fallback / [n] decline; per-tab fallback cache.
56. Mount scope: [r] root / [c] cwd / [a] abort.
57. Agent auth consent: [y]/[n]/[o] persist correctly.
58. Config show: edit mode, Ctrl+S save, Esc cancel, read-only field rejection.
59. New-artefact dialogs: kind selection, title input, multiline summary, multi-field forms.
60. Claws dialogs: every variant (HasForked, UsernameInput, SudoConfirm, DockerSocketWarning, OfferRestartStopped, OfferStart, RestartFailedOfferFresh, AuditConfirm).
61. Worktree dialogs: PreCommitWarning [c/u/a], PreCommitMessage, MergePrompt [m/d/s], CommitPrompt, MergeConfirm [y/n], DeleteConfirm [y/n].
62. Quit/CloseTab confirm: every key path.
63. PTY: vt100 ANSI rendering; scrollback (↑/↓/PageUp/PageDown/b/e); mouse selection + Ctrl+Y clipboard; carriage-return spinner.
64. Kitty keyboard protocol: enabled best-effort; non-fatal on failure.
65. Tab status log: level-colored prefixes; auto-scroll; `l` toggle.
66. Status command tab annotations: appear in TUI, not in CLI/headless.
67. TUI startup: in-repo runs `ready`; not-in-repo runs `status --watch`.
68. Tab close with running container forcibly cancels (no prompt).

**Headless behavior parity** (from `tests/headless_parity/`):

69. Every route in legacy `build_router` is reachable; method+path match frozen fixture.
70. Auth modes: token (good/bad), disabled (`X-Amux-Auth: disabled` header), TLS-required (rejects non-loopback without TLS).
71. SSE wire format: container chunks, amux-message events, completion events match frozen fixture byte-for-byte.
72. WebSocket wire format (if used): same as SSE.
73. PID file lifecycle: written on start, removed on clean shutdown, stale-PID detection on second start.
74. `--background` daemonizes; PID file points to daemon.
75. `--refresh-key` prints exactly legacy banner; old hash replaced.
76. Workdir allowlist: CLI `--workdirs` merges with config; non-existent paths rejected.
77. Headless safe-defaults for every interactive frontend method: `ReadyFrontend::ask_create_dockerfile` → true, `ask_run_audit_on_template` → false, `ask_migrate_legacy_layout` → false, `InitFrontend::ask_replace_aspec` → false, `ask_run_audit` → false, `ask_work_items_setup` → None, `ClawsFrontend::ask_replace_existing_clone` → false, `ClawsFrontend::ask_run_audit` → false, `WorkflowFrontend::user_choose_next_action` → LaunchNext, `user_choose_after_step_failure` → Pause, `WorktreeLifecycleFrontend` → UseLastCommit/Resume/Keep/None/false/false, `MountScopeFrontend` → MountGitRoot, `AgentSetupFrontend` → Setup, `AgentAuthFrontend` → DeclineOnce, `AuthCommandFrontend` → DeclineOnce.
78. SQLite session/command persistence: schema forward-compatible with legacy (open fixture DB).

**Cross-cutting parity**:

79. `AMUX_OVERLAYS` env validation fires before command construction; malformed → fatal error.
80. `--non-interactive` flag and `headless.alwaysNonInteractive` config → `AgentRunOptions::non_interactive = true` AND agent-specific print flag.
81. `auto_agent_auth_accepted` first-run consent: None → prompt → persist; Some(true) → silent; Some(false) → no inject.
82. Detached HEAD: warned via `UserMessage::warning`, command continues.
83. `--api-key` flag > `AMUX_API_KEY` env > `remote.defaultAPIKey` (only when addr matches after URL canonicalization).
84. HTTP timeouts: connect=10s, read=600s for `send_command`; read disabled for `stream_command`.
85. Error-message parity: every user-visible string from legacy is reproducible or close paraphrase.

Each row MUST appear in `aspec/review-notes/0073-parity-validation.md` with test file path and verdict. Empty cells are not acceptable.

### 3. Stale placeholder and comment cleanup

Prior work items left several intentional placeholder markers that should now be cleaned up:

- **`src/data/session.rs` (line ~244)**: contains "placeholder until work item 0067" comment. Verify the underlying code is real; remove the stale comment.
- **`src/frontend/mod.rs` (lines 8, 10)**: documents TUI and headless as "placeholder". Update to describe the real implementations.
- **`src/engine/claws/mod.rs` (line ~196)**: `TODO(issue-17): The full fork-and-clone flow (gh repo fork)` — this is a tracked issue, NOT part of this refactor. Confirm it's documented in the project's issue tracker. Leave the TODO but ensure it references the correct issue number.
- **Any remaining `NotImplemented` error returns**: grep for `NotImplemented` across `src/`. Every instance should either be a legitimate error variant definition or unreachable. If any code path still returns `NotImplemented`, it is a bug — fix it or ASK THE DEVELOPER.
- **Any remaining `"placeholder"` or `"later WI"` comments**: grep and remove or update.

### 4. Architectural tenet audit

Produce `aspec/review-notes/0073-architecture-audit.md` covering:

#### 4a. Layering — no upward calls

For each Rust file in `src/`, confirm imports respect the layering rule:
- `src/data/**`: imports from `std`, third-party crates, and `crate::data::*` only
- `src/engine/**`: above plus `crate::data::*`
- `src/command/**`: above plus `crate::engine::*`
- `src/frontend/**`: above plus `crate::command::*`
- `src/main.rs`: any

Implement this as a `make architecture-lint` rule (see step 6). Any violation must be fixed.

#### 4b. No business logic in frontends

Walk every file in `src/frontend/`. Flag any `if`, `match`, or computed default whose decision affects *behavior* rather than *presentation*.

- **Acceptable** (false positives): branching on `OutcomeKind` to choose render format, branching on terminal capabilities (TTY vs not), branching on rendering width
- **Must move to Layer 2** (true positives): default-value computation for unsupplied flags, agent selection logic, workflow step container option computation

#### 4c. Typed objects over `pub fn`

Walk every `pub fn` in `src/`. Flag any that is stateful, takes many inputs, or could be a method on an existing struct. Convert flagged ones to methods.

#### 4d. Catalogue completeness

- Confirm `CommandCatalogue::root()` covers every documented command
- Confirm `CommandCatalogue::flag_iter()` covers every documented flag
- Verify `Dispatch::parse_command_box_input` (added in WI 0071) works for every catalogue command
- Verify `CommandCatalogue::tui_completions` and `tui_hint_for` (added in WI 0071) cover all commands

### 5. (Reserved — oldsrc deletion is manual)

The developer will delete `oldsrc/`, legacy `tests/`, and legacy `benches/` manually after manual testing is satisfactory. This section is intentionally left as a placeholder to preserve numbering of subsequent sections.

When the developer performs the deletion, the following cleanup will also be needed:

- `Cargo.toml`: remove `oldsrc`/`amux-next` references; confirm `[[bin]] name = "amux"` points at `src/main.rs`
- `Makefile`: remove `oldsrc` references; confirm `make all`, `make install`, `make test`, `make test-fast`, `make test-full` all work
- `.gitignore`, `.github/workflows/*.yml`, `scripts/*.sh`, `Dockerfile.dev`: search for `oldsrc` and `amux-next`
- `aspec/`, `docs/`, `README.md`, `CLAUDE.md`: same search

### 6. `make architecture-lint`

Add a Make target that mechanically enforces layering. Two acceptable implementations:

1. **Preferred**: A small Rust binary in `tools/architecture-lint/` using `cargo metadata` + `syn` to walk modules and confirm import direction. Survives renames.
2. **Acceptable for v1**: A shell script using `rg` patterns.

Requirements:
- Runs in CI (`.github/workflows/test.yml`)
- Prints every violation with file path + line + offending import
- Exit non-zero on any violation
- Runs in well under 10 seconds
- Ignores `std::*` and external crate imports; only inspects `crate::*` paths
- `#[cfg(test)]`-gated upward imports: forbidden by default. Allow only with explicit developer approval.

Add `make pre-push` umbrella: `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test` + `make architecture-lint`. Update contributor docs.

### 7. Refresh `docs/`

- Overview pages: describe four-layer architecture in user-friendly terms
- Internal pages: point at `src/data/`, `src/engine/`, `src/command/`, `src/frontend/`
- Remove all references to `src/runtime/`, `src/tui/`, `src/commands/` (pre-refactor paths)
- `docs/releases/<next-version>.md`: changelog entry summarizing the refactor
- `docs/blog/`: optional refactor write-up (ASK THE DEVELOPER)

### 8. Refresh `aspec/`

- `aspec/foundation.md`: add one sentence noting four-layer architecture if not already present
- `aspec/architecture/design.md`: replace pre-refactor description with pointer to `2026-grand-architecture.md` and one-paragraph summary
- `aspec/architecture/security.md`: confirm every constraint still holds
- `aspec/uxui/cli.md`: regenerate from `CommandCatalogue` (preferred) or audit by hand to match
- `aspec/devops/localdev.md`, `cicd.md`, `operations.md`, `subagents.md`: update stale path/module references
- `aspec/work-items/0000-template.md`: leave unchanged unless developer requests update

### 9. Final sanity pass

- `cargo build --release` produces a single statically-linked `amux`
- `cargo test` passes (entire new suite including all `tests/*`)
- `make test-full` passes on runner with Docker
- `make test-fast` passes on runner without Docker (clear skip messages)
- `cargo clippy --all-targets -- -D warnings` passes
- `make architecture-lint` passes
- `make all`, `make install`, `make test` work
- Parity validation report is complete with no unresolved REGRESSIONs
- Repository is ready for developer's manual testing and subsequent `oldsrc/` deletion.

### 10. What must NOT happen in this work item

- No new features
- No new flags
- No new commands
- No user-visible behavior change (if a parity check shows something "feels worse" but is technically equivalent, leave it alone unless developer says otherwise)
- No deleting `oldsrc/`, legacy `tests/`, or legacy `benches/` — the developer will do this manually
- No editing `oldsrc/`

---

## Edge Case Considerations

- **Architecture-lint on third-party crate paths**: lint ignores `std::*` and external crates; only inspects `crate::*` paths.
- **`#[cfg(test)]` test modules**: tests under `src/data/` may want a helper from another layer. Default is to forbid; allow only with explicit developer approval.
- **Workspace splits**: if Cargo layout uses a workspace, confirm `Cargo.toml` reflects the correct shape for the new `src/` tree.
- **Existing user data**: users upgrading must not lose data. `SqliteSessionStore` schema must remain readable; persisted workflow state must load. Confirm with a real database from a prior install if available.
- **Headless SQLite forward-compatibility**: `HeadlessDb` schema (added in WI 0072) must open legacy databases. Test with a captured fixture.
- **Release notes**: next release should call out the refactor at a high level (CLI behavior unchanged, internal structure changed). ASK THE DEVELOPER for tone.
- **CI flake risk**: adding a new test suite + lint can mask flakes. Run full CI at least twice before merge.
- **Coverage drop**: new tests should cover equivalent behavior. Run coverage before and after on parity suite to confirm.
- **TODO(issue-17) in claws/mod.rs**: this is a tracked feature request (fork-and-clone flow), NOT a refactor regression. Leave the TODO; confirm it's in the issue tracker. Do NOT attempt to implement it in this work item.

---

## Test Considerations

### Test philosophy

This work item is the **only** point that adds tests to `tests/` (and `benches/` if needed). All prior WIs produced colocated unit tests only.

**Do not port tests from pre-refactor `tests/` or `benches/`.** See §1 for the narrow exception.

### Tests added

- Complete `tests/` tree (§1)
- `tools/architecture-lint/` unit tests (if implemented as Rust binary)

### Tests preserved

All `#[cfg(test)] mod tests` blocks from WIs 0066–0072 remain in place and continue to pass.

### Build & CI

- `make test-fast` runs in under a minute (warm cache)
- `make test-full` runs on CI with Docker
- `make architecture-lint` runs on every PR
- `make pre-push` runs locally in under 2 minutes (warm cache)
- Release build: single static binary for macOS, Linux, Windows

### Manual smoke test

- Install new binary on real machine: `amux init`, `amux ready`, open TUI, run `exec workflow`, exit
- Start `amux headless start`, issue real `curl` calls, stop cleanly

---

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth
- Follow `aspec/uxui/cli.md` after regeneration from catalogue
- Do not edit or delete `oldsrc/` — the developer will handle deletion manually after testing
- Do not introduce upward calls or new free `pub fn` for stateful concerns
- Fix any leftover violations from prior WIs as part of the audit
- The PR description MUST link to this work item, MUST include the parity report, the architecture audit report, and MUST list any developer-clarification questions raised
- After this work item lands and the developer completes manual testing and `oldsrc/` deletion, the grand architecture refactor is complete.
