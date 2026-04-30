# Work Item: Enhancement

Title: init unification
Issue: issuelink

## Summary:
- Unify the `init` subcommand so CLI and TUI are pure presentation layers over a single shared implementation module. All business logic, stages, structs, and helpers live in one place. The only permitted differences between CLI and TUI are: (1) how they perform user Q&A (stdin prompts vs. modal dialogs) and (2) how they launch containers (blocking stdio vs. PTY window). It must be structurally impossible for the two surfaces to diverge in behaviour.

## User Stories

### User Story 1:
As a: user

I want to:
run `amux init` from either the CLI or the TUI and get identical outcomes — the same files written, the same images built, the same work-items setup offered

So I can:
trust that whichever interface I use, the tool behaves consistently and I won't encounter features or fixes that exist in one surface but not the other

### User Story 2:
As a: contributor

I want to:
add or change a stage of `init` in exactly one file and have that change apply to both CLI and TUI automatically

So I can:
avoid the maintenance burden of keeping two divergent implementations in sync and prevent regressions caused by partial updates

### User Story 3:
As a: user

I want to:
have the work-items setup step offered to me when running `init` from the TUI, just as it is from the CLI

So I can:
complete the full init workflow without switching to the CLI for features the TUI currently omits


## Implementation Details:

### Current state (the problem)

The `run_with_sink()` function in `src/commands/init.rs` is shared, but the following divergences exist:

- **Q&A before `run_with_sink()`**: CLI calls `ask_yes_no_stdin()` at lines 42–72 upfront. TUI drives this via `Dialog::InitReplaceAspec` and `Dialog::InitAuditConfirm` states defined in `src/tui/state.rs:207–216`, with input handlers in `src/tui/input.rs:1765–1797`.
- **Audit execution**: CLI runs the audit inline inside `run_with_sink()` (lines 164–294, 342–415). TUI skips this entirely, sets a `pending_init_run_audit` flag, and defers audit to a separate `ready --refresh` invocation via `check_init_continuation()` in `src/tui/mod.rs:1518–1564`.
- **Work-items setup**: CLI-only interactive step (lines 422–498), gated by `out.supports_color()` — a hack that uses the output sink to detect execution mode. TUI never runs this step.
- **Container launch**: CLI blocks synchronously; TUI spawns an async task and streams output to a tab channel.

### Target architecture

Introduce `src/commands/init_flow.rs` as the canonical, mode-agnostic implementation of `init`. It owns all business logic and exposes two trait boundaries that callers must satisfy.

#### Trait 1 — `InitQa`

Handles all user question-and-answer interactions needed during the init flow:

```rust
pub trait InitQa {
    fn ask_replace_aspec(&mut self) -> anyhow::Result<bool>;
    fn ask_run_audit(&mut self) -> anyhow::Result<bool>;
    fn ask_work_items_setup(&mut self) -> anyhow::Result<Option<WorkItemsConfig>>;
}
```

CLI provides `CliInitQa` backed by `ask_yes_no_stdin()` and `read_line()`. TUI provides `TuiInitQa` backed by the existing dialog mechanism — answers are already collected before `launch_init()` is called, so `TuiInitQa` can be a simple struct holding pre-collected answers that returns them without blocking.

#### Trait 2 — `InitContainerLauncher`

Handles container build and run operations so the flow does not hard-code a blocking vs. async strategy:

```rust
pub trait InitContainerLauncher {
    fn build_image(&self, tag: &str, dockerfile: &Path, context: &Path, sink: &OutputSink) -> anyhow::Result<()>;
    fn run_audit(&self, agent: Agent, cwd: &Path, sink: &OutputSink) -> anyhow::Result<()>;
}
```

CLI provides `CliContainerLauncher` which blocks synchronously (existing behaviour). TUI provides `TuiContainerLauncher` which either runs inline in the spawned task or dispatches to the PTY tab — this is its prerogative as a launcher, not a concern of the flow.

#### `InitFlow::execute()`

Replace the 420-line `run_with_sink()` monolith with a structured, sequential set of named stages. Each stage is a private method on `InitFlow` and updates `InitSummary`. The public entry point is:

```rust
pub async fn execute<Q: InitQa, L: InitContainerLauncher>(
    params: InitParams,
    qa: &mut Q,
    launcher: &L,
    sink: &OutputSink,
    runtime: &dyn AgentRuntime,
) -> anyhow::Result<InitSummary>
```

Stages (in order, matching current CLI behaviour):
1. Collect Q&A (calls `qa.ask_replace_aspec()` and `qa.ask_run_audit()`)
2. Load and update repo config
3. Download or skip aspec folder
4. Write `Dockerfile.dev`
5. Write `.amux/Dockerfile.{agent}`
6. Check container runtime availability
7. If `run_audit`: build project image → build agent image → run audit container → rebuild both
8. Else if new Dockerfiles: build project image → build agent image
9. Call `qa.ask_work_items_setup()` and apply result (previously CLI-only; now offered in TUI too)
10. Print `InitSummary` and "What's Next?" guide

All helper functions (`write_project_dockerfile`, `write_agent_dockerfile`, `download_or_fallback_agent_dockerfile`, `find_git_root_from`, `print_init_summary`, `print_whats_next`, `dockerfile_for_agent_embedded`, etc.) move into `init_flow.rs` as module-private functions.

#### CLI adapter (`src/commands/init.rs`)

Becomes a thin shim:

```rust
pub async fn run(agent: Agent, aspec: bool, cwd: PathBuf, runtime: &dyn AgentRuntime) -> anyhow::Result<()> {
    let git_root = find_git_root_from(&cwd)?;
    let mut qa = CliInitQa::new(&git_root);
    let launcher = CliContainerLauncher::new(runtime);
    let sink = OutputSink::Stdout;
    let params = InitParams { agent, aspec, git_root };
    init_flow::execute(params, &mut qa, &launcher, &sink, runtime).await?;
    Ok(())
}
```

No upfront pre-flight Q&A outside `execute()`. The `ask_replace_aspec` and `ask_run_audit` calls happen inside the flow, via the `qa` object, at the correct stage.

#### TUI adapter (`src/tui/mod.rs`)

- Existing dialog states (`InitReplaceAspec`, `InitAuditConfirm`) remain but now collect answers into a `TuiInitAnswers` struct rather than directly triggering `launch_init()` with hardcoded parameters.
- Add new dialog states for the work-items setup question (mirrors the existing work-items interactive flow in `ready`).
- `launch_init()` constructs `TuiInitQa { answers }` and `TuiContainerLauncher` and calls `init_flow::execute()` in a background task, identical in shape to how CLI does it.
- **Remove** `pending_init_run_audit` flag and `check_init_continuation()`. The audit is now run inside `execute()` via the `TuiContainerLauncher`, which can choose whether to block in the task thread (acceptable since the task is already on its own thread) or to invoke a PTY — either way this decision belongs to the launcher, not to `mod.rs` lifecycle hacks.

#### Eliminations

- Remove the `out.supports_color()` mode-detection hack for work-items gating.
- Remove `pending_init_run_audit` state and `check_init_continuation()` from `src/tui/mod.rs`.
- Remove the duplicate upfront `ask_yes_no_stdin()` calls from the top of CLI `run()`.
- The `Action::InitAuditAccepted` / `InitAuditDeclined` / `InitReplaceAspecAccepted` / `InitReplaceAspecDeclined` action variants remain but now only populate `TuiInitAnswers` and trigger the flow, not the audit separately.


## Edge Case Considerations:

- **TUI async contract**: `TuiContainerLauncher::run_audit()` executes inside the background task spawned by `launch_init()`, so blocking there is safe. The PTY rendering for the audit container can stream through the existing tab `output_tx` channel just as build output does now — no additional mechanism is needed.
- **Audit modifies `Dockerfile.dev`**: Stage 7 rebuilds both images after audit because the audit may rewrite `Dockerfile.dev`. The flow must not skip the rebuild step regardless of which launcher is in use.
- **`aspec/` directory already exists in TUI**: The dialog for `InitReplaceAspec` is only shown when `--aspec` was passed and the directory exists. `TuiInitQa` must faithfully encode whether the user was asked this question at all (a user who was never asked should be treated as `replace_aspec = false`, not as an error).
- **Work-items setup dialog in TUI**: This is a net-new TUI dialog. Design it to follow the same modal patterns as `InitReplaceAspec`. If the user declines, `ask_work_items_setup()` returns `None` and the step is skipped in `InitSummary`.
- **Git root not found**: `find_git_root_from()` must return an error before any Q&A begins. Both CLI and TUI must propagate this as a user-visible error before entering the flow.
- **Runtime not available**: Stage 6 (runtime check) must still short-circuit the flow with a clear error via `sink`; the flow should not attempt builds if the daemon is absent.
- **Partial failure recovery**: `InitSummary` already tracks per-stage status. If an early stage fails, later stages should set their status to `Skipped` rather than attempting to run on broken preconditions.
- **OutputSink divergence for "What's Next?"**: The ANSI rainbow text in `print_whats_next()` is already gated by `supports_color()` on `OutputSink`. This gating is legitimate (styling only) and must be kept.


## Test Considerations:

- **Unit tests for `InitFlow`**: Use mock implementations of `InitQa` and `InitContainerLauncher` that record calls and return preset answers. Test each stage independently and verify that `InitSummary` reflects the correct status for each. These tests must not touch the filesystem or Docker.
- **Unit tests for `CliInitQa`**: Provide fake stdin input via a byte cursor and assert that `ask_replace_aspec()`, `ask_run_audit()`, and `ask_work_items_setup()` parse responses correctly, including edge cases like empty input, unexpected characters, and EOF.
- **Unit tests for `TuiInitQa`**: Construct a `TuiInitQa` with known pre-collected answers and verify it returns them without blocking.
- **Integration test — CLI full path**: Spin up a temp git repo, provide mock answers via `CliInitQa`, and use a fake `CliContainerLauncher` that no-ops builds and audit. Assert all expected files are written and `InitSummary` reports `Ok` for each stage.
- **Integration test — TUI full path**: Run the same scenario with `TuiInitQa` and `TuiContainerLauncher`. Assert identical file outcomes to the CLI integration test. This test is the structural guarantee that the two surfaces cannot diverge.
- **Regression test — work-items in TUI**: Verify that `ask_work_items_setup()` is called during the TUI flow and that declining it does not cause a panic or missing summary row.
- **Regression test — audit deferred removal**: Verify `pending_init_run_audit` no longer exists in TUI state and that no `check_init_continuation()` code path remains.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `init_flow.rs` belongs in `src/commands/` alongside `init.rs`. Update `src/commands/mod.rs` to `pub mod init_flow`.
- Trait objects (`dyn InitQa`, `dyn InitContainerLauncher`) are acceptable if needed for test mocking; generics are preferred if the compiler accepts it without excessive bounds duplication.
- The `OutputSink` abstraction in `src/commands/output.rs` is the correct mechanism for routing output — do not add a second output abstraction.
- The `AgentRuntime` trait in `src/runtime/mod.rs` is the correct mechanism for Docker operations — `InitContainerLauncher` should delegate to it rather than calling Docker directly.
- Keep `InitSummary` and its status enum (`Pending`, `Ok`, `Skipped`, `Failed`, `Warn`) in `init_flow.rs` since they are part of the shared flow, not the CLI or TUI presentation layer.
- All existing tests in `src/commands/init.rs` must continue to pass; migrate them into `init_flow.rs` and extend them as described in the test plan.
