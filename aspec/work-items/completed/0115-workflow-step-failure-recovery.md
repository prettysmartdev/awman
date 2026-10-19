# Work Item: Feature

Title: Workflow step-failure recovery and workflow resume
Issue: n/a

## Summary:
- A failed agent step in a workflow currently ends the run: the interactive
  frontends surface a small "Step failed" dialog whose default (Esc/dismiss)
  pauses the workflow, and the user is dropped straight into the post-workflow
  merge/keep/discard worktree prompt. There is no way to retry the step, step
  back, or step over it.
- After abandoning a failed `exec workflow --dynamic` run there is no way back
  in: the generated `workflow.toml` lives in the leader's per-invocation context
  directory (keyed by a random session UUID), so a subsequent
  `--dynamic --work-item N` always re-runs the leader from scratch and throws
  away whatever the previous run achieved.
- Resuming a plain `exec workflow` is worse than useless after an abort. An
  abort marks every remaining step `Cancelled`, so the saved state is
  all-terminal; `is_complete()` reads that as finished and the resumed run
  reports success having executed nothing. The two modes also ask different
  resume questions — dynamic offers named steps, plain offers yes/no.
- Unattended runs (squad daemon, API server) abort the whole workflow on the
  first non-zero step exit, with no allowance for a transient failure.

## User Stories

### User Story 1:
As a: user

I want to:
be shown the Workflow Control Board — with the error that killed the step —
when an agent step fails in an interactive run, and to choose between restarting
the failed step, going back to the previous step, or starting the next step in a
fresh container

So I can:
recover a workflow in place instead of losing the whole run to one bad step.

### User Story 2:
As a: user

I want to:
be offered a resume of my previous dynamic workflow when I re-run
`awman exec workflow --dynamic --work-item N` and the previous run's worktree is
still on disk, picking by name whether to start from the step that failed, the
step before it, or the step after it

So I can:
pick a failed dynamic run back up without paying for a second leader-design
pass and without losing the work already committed in the worktree.

### User Story 3:
As a: admin

I want to:
an unattended workflow (squad daemon / API server) to wait out a 60s yolo
countdown and retry a failed step once before the run is declared failed

So I can:
ride out transient agent/container failures without a human in the loop.

## Implementation Details:

### 1. Interactive step-failure control board

- `WorkflowFrontend` gains `supports_interactive_recovery(&self) -> bool`
  (default `false`). TUI returns `true`; CLI returns `!non_interactive`; the
  squad `UnattendedFrontend` and `ApiDispatchFrontend` keep the default.
  The engine still knows nothing about *which* frontend is attached — it asks
  for a capability, and decides policy itself.
- `AvailableActions` gains `step_failure: Option<StepFailureContext>`
  (step name, exit code, signal, human-readable detail lines, and the resolved
  names of the previous/next steps). The board renders it as an error banner.
- `WorkflowEngine::handle_step_failure` replaces the two
  `user_choose_after_step_failure` call sites (single-step path and
  parallel-group drain). When the frontend supports interactive recovery it
  loops on `show_workflow_control_board` with a failure-scoped
  `AvailableActions`:
  - `↑` `RestartCurrentStep` → failed step back to `Pending`.
  - `←` `CancelToPreviousStep` → failed step *and* previous step both back to
    `Pending`. Neither status is in `completed_steps`, so readiness is the same
    either way; `Pending` is the honest one for two steps that are both about
    to run again.
  - `→` `LaunchNext` → failed step `Skipped` so its dependents become ready,
    then the next ready step launches in a fresh container.
  - `Ctrl-C` → `Abort`; `Esc` → `Pause`. `can_finish_workflow` is forced off,
    so there is no "Enter to finish workflow" on a failure board.
- The TUI gates each arrow on its `can_*` flag rather than sending the action
  and letting the engine reject it. An action the board renders greyed-out
  cannot be chosen, so an unavailable arrow is inert instead of re-presenting
  an identical board — which reads as a broken keystroke.
- Every failure in a drained parallel group gets its own board, in exit order.
  A step left `Failed` is not in `completed_steps`, so the DAG still reports it
  ready: recovering only the first failure would leave its peers to be silently
  relaunched with no board and no retry accounting.
- `WorkflowFrontend::user_choose_after_step_failure`, `StepFailureChoice`, and
  the TUI `WorkflowStepError` dialog are removed — the control board is now the
  single failure-recovery surface, and leaving a dead engine→frontend contract
  behind would invite divergence.

### 2. Workflow resume — one flow for both modes

Resume is a single code path. `WorkflowResumePrompt` (built by the command
layer, rendered verbatim by every frontend, exactly as
`PostWorkflowWorktreePrompt` already is) offers three named start points — the
step the run stopped on, the one before it, the one after it — and both
`exec workflow` and `exec workflow --dynamic` raise it. Dynamic mode raises it
before the leader phase, because accepting a resume means no leader runs at
all; the plain path raises it before `WorktreeLifecycle::prepare`, for the
reason below; nothing else differs.

`WorkflowResumeDecision` has three answers, not two: `ResumeFrom`, `Fresh`, and
`Cancel`. `Fresh` is destructive — it deletes the saved state, and in dynamic
mode the leader design with it — so it has to be typed (`f`). `Cancel` is what
Esc means, and it is a cancellation of the *command*: nothing runs, nothing is
created, nothing is deleted, and the same offer stands next time. That is only
truthful if the question is asked before anything exists, which is why both
paths ask ahead of worktree preparation. `execute_prepared` keeps a fallback
copy of the question for runs that reach it without one (no worktree, or a
freshly designed dynamic workflow); `PreparedRun.skip_state_resume_prompt`
suppresses it for the paths that already asked.

The decision and its consequences live together in one `offer_state_resume`,
so "resume" cannot come to mean three subtly different things at three sites.
An accepted resume also answers the existing-worktree question — the saved run
lives in that worktree, and recreating it would delete the state just rewound —
so both paths pass `ExistingWorktreeDecision::Resume` through
`prepare_with_existing`.

Three Layer 0 additions on `WorkflowState` carry the mechanics, so neither the
command layer nor the engine reimplements them:

- `resume_stop_point(&dag)` — index of the first `Failed`/`Cancelled` step in
  topological order, falling back to the first step that never succeeded.
- `rewind_to(&dag, start)` — steps at or after `start` back to `Pending`;
  earlier non-succeeded steps to `Skipped` so they count as satisfied
  dependencies without re-running.
- `unrecovered_steps()` — the `Failed`/`Cancelled` steps a saved run left behind.
- `retain_steps_in(&dag)` — drop steps the workflow no longer defines. A saved
  state outlives edits to its workflow file; such a step can never run (the DAG
  decides what runs) but still counts towards `is_complete()`, so a
  non-terminal orphan strands the run on "no ready steps remaining". This
  matters *because* of the reset below, which turns exactly such an orphan from
  terminal to `Pending`.

`WorkflowEngine::resume_with_state_root` resets `unrecovered_steps()` to
`Pending` at load, alongside the existing interrupted-`Running` reset and for
the same reason. This is the engine's own guard rather than a command-layer
convention: an aborted run marks *every* remaining step `Cancelled`, so its
saved state is all-terminal, `is_complete()` reads it as finished, and the
first check in `run_to_completion` would report instant success having executed
nothing. `is_complete()` itself is left alone — it is public API and its
"every step reached a terminal state" reading is correct; the bug was resuming
into it, not the predicate.

Frontend defaults: TUI and CLI-on-a-TTY prompt; `--non-interactive` and the API
server take `WorkflowResumePrompt::resume_from_stop_point()`, preserving the
old "don't discard saved work" default; the squad daemon alone answers `Fresh`,
because each scheduled evaluation is meant to be its own run. None of the
unattended frontends can answer `Cancel` — there is nobody to press Esc — so
the cancel path is interactive-only by construction.

The stop point's label follows the status that produced it: "the step that
failed", "the step that was cancelled", or "the step the run stopped on" for
the never-succeeded fallback, which is what an interrupted or paused run
leaves. Naming that last case a failure would describe one that never happened.

### 2b. Dynamic-workflow resume specifics

- After the leader's `workflow.toml` validates, a copy is saved to
  `<worktree>/.awman/workflows/dynamic-<NNNN>.toml`
  (`WorkflowDirs::dynamic_workflow_path`). `.awman/workflows` is already
  gitignored and already holds the engine's `WorkflowState` JSON, so the two
  halves of a resumable run live side by side and die with the worktree.
- `run_dynamic` looks for a previous run before touching the leader: worktree on
  disk → saved `workflow.toml` → parse → `WorkflowStateStore` load by
  (work item, workflow title).
  - All three present and the state is not already all-succeeded → offer the
    shared resume prompt (§2) and skip the leader phase entirely on acceptance.
  - Neither half present → `DynamicDiscoveryMiss::NothingToResume`: a silent
    fresh start. A first run and a worktree kept after a finished run both look
    exactly like this, and neither is a broken run to warn about.
  - One half present without the other → `DynamicDiscoveryMiss::Unusable`, and
    `notify_dynamic_workflow_resume_unavailable` states what is missing and
    waits for Enter before a fresh dynamic workflow starts. "Has state with
    work left in it" is the test — deliberately not `is_complete()`, which a
    failed run also satisfies.
- `WorktreeLifecycle::prepare_with_existing` takes a pre-selected
  `ExistingWorktreeDecision` so a confirmed resume does not ask the
  existing-worktree question a second time.
- `PreparedRun.skip_state_resume_prompt` suppresses `execute_prepared`'s own
  resume prompt when an earlier path already asked it and already rewound.
- A dynamic run that finishes with exit code 0 deletes *both* halves — the
  saved `dynamic-<NNNN>.toml` and the `WorkflowState` — from the
  leader-designed and the resumed path alike. Deleting only the workflow would
  strand a kept worktree in the `Unusable` case above, greeting a successful
  run with "cannot be resumed" on its next invocation.

### 3. Unattended retry

- When `supports_interactive_recovery()` is false, a failed step runs the
  standard 60s yolo countdown (`timing::YOLO_COUNTDOWN_DURATION`, driven through
  `yolo_countdown_started`/`_tick`/`_finished` so API and squad frontends report
  it exactly as they report a stuck-step countdown) and is then retried once.
- `yolo_countdown_started` carries a `CountdownKind` (`StuckStep` /
  `FailureRetry`). Sharing the channel is the point — an unattended frontend
  surfaces both the same way — but the two mean opposite things to whoever is
  reading, so a frontend that narrates the countdown in words is told which it
  is rather than left to assume the common one. The API frontend says
  "retrying after failure" instead of "auto-advancing"; frontends that render
  only a clock ignore it.
- The retry allowance is tracked per step in `auto_retried_steps`, independent of
  the existing `auth_retries_used` guard, so a step can burn its auth refresh and
  its failure retry without either resetting the other.
- A second failure of the same step ends the run as
  `WorkflowOutcome::Failed { last_step, exit_code }` (remaining steps
  `Cancelled`), which `execute_prepared` already maps to the step's exit code.
  `abort_on_failure` steps still abort immediately, before any retry.

## Edge Case Considerations:
- `LaunchNext` on the *last* failed step leaves nothing ready, so
  `can_launch_next` is false and the board does not offer it. The TUI gates the
  right arrow on that flag, so the action cannot be raised at all; the engine's
  "no next step" arm is unreachable defensive code rather than a loop the user
  can fall into. The left arrow on the first step is gated the same way.
- A failure surfacing after a parallel group drains uses the same board, scoped
  to the failed step; peers have already exited by then. A group with several
  failures raises one board per failure, in exit order.
- `state.is_complete()` is true after an abort (every step terminal), so a
  resumed dynamic state must be rewritten before the engine runs or the run
  would report instant success.
- The previous/next step names come from the DAG topological order, so a
  fan-out/fan-in workflow names a real neighbour rather than a source-order one.
- A worktree that survives but whose `.awman/workflows` was cleaned yields the
  "resume unavailable" notice, never a half-resumed run. A worktree with
  *nothing* in it — never run, or run to completion — yields no notice at all.
- A dynamic resume must not re-run the leader mutation guard against a
  worktree that legitimately holds the previous run's commits.
- Cancelling at the resume prompt must be a true no-op: it is raised before the
  worktree is prepared and before the leader launches, and writes and deletes
  nothing. It reports `exit_code: None`, the same "no terminal status" the
  paused outcome already uses.
- A saved state whose workflow has since dropped or renamed a step, resumed
  after the hash-drift prompt is accepted: the orphan is pruned rather than
  reset, or the run could never satisfy `is_complete()`.

## Test Considerations:
- Data: `resume_stop_point` picks the failed step, falls back to the first
  unfinished one, and is `None` when everything succeeded; `rewind_to` makes an
  all-terminal aborted state runnable again, skips predecessors when starting
  later, and is a no-op for an unknown step name; `retain_steps_in` drops steps
  the DAG no longer defines, leaves the rest alone, and clears them from
  `completed_steps`.
- Engine: a resumed state carrying a step the workflow dropped runs to
  completion instead of stranding on "no ready steps remaining".
- Engine: a parallel group with two failures raises a board for each.
- Command: `Cancel` at the prompt leaves the state file byte-identical and
  reports no exit code; `Fresh` deletes it; only an explicit `f` is `Fresh`.
- Command: the stop point is labelled by what actually stopped the run.
- Command: dynamic discovery is silent for a worktree with no run and for one
  kept after a completed run, and reports the missing half otherwise.
- Frontend: an arrow whose `can_*` flag is false raises no action.
- Engine: resuming a genuinely aborted run re-runs its cancelled steps instead
  of reporting instant success, and leaves succeeded steps alone.
- Command: the resume prompt copy covers both modes; the unattended answer is
  the step the run stopped on.
- Engine: failed step + interactive frontend shows the board; each of restart /
  back / next produces the right `StepState` transitions.
- Engine: non-interactive frontend retries once then yields
  `WorkflowOutcome::Failed`; `abort_on_failure` still short-circuits.
- Engine: `compute_failure_actions` forces `can_finish_workflow` off and fills
  `step_failure`.
- Data: `WorkflowDirs::dynamic_workflow_path` shape.
- Frontends: CLI board renders the failure banner; TUI board state carries the
  failure lines.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/05-workflows.md` — the failure control board, the unattended retry, the
  shared resume prompt, and the unfinished-step reset.
- `docs/06-dynamic-workflows.md` — resuming a failed dynamic run.
