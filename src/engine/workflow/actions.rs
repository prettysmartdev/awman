//! `NextAction`, `AvailableActions`, `StepFailureContext`, `YoloTickOutcome`.

use std::time::Duration;

use crate::data::prompt::Prompt;
use crate::data::workflow_state::StepState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// Launch a fresh container for the next ready step.
    LaunchNext,
    /// Push an additional prompt into the still-running container, keeping it
    /// alive for the next step. Only valid when the next step targets the
    /// same agent and the running container supports prompt injection.
    ContinueInCurrentContainer { prompt: String },
    /// Re-run the step that just completed.
    RestartCurrentStep,
    /// Revert to the immediately-previous step in topological order.
    CancelToPreviousStep,
    /// Mark every remaining step as Skipped and the workflow as completed.
    /// Only valid when the current step is the last in topological order.
    FinishWorkflow,
    /// Pause execution after the current step completes.
    Pause,
    /// Abort the workflow entirely.
    Abort,
    /// Mid-step only: dismiss the control board dialog without affecting the
    /// running step. The step continues executing undisturbed.
    Dismiss,
    /// Parallel group only: relaunch a peer step that already failed while
    /// the rest of the group is still running. Only valid when
    /// [`AvailableActions::retry_failed_step`] names that step.
    RetryFailedStep { step_name: String },
}

/// Set of `NextAction` variants the frontend may present to the user. The
/// engine computes this set; the frontend renders only what it permits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AvailableActions {
    pub can_continue_in_current_container: bool,
    pub can_launch_next: bool,
    pub can_restart_current_step: bool,
    pub can_cancel_to_previous_step: bool,
    pub can_finish_workflow: bool,
    pub can_pause: bool,
    pub can_abort: bool,
    /// The prompt to inject when the user chooses `ContinueInCurrentContainer`.
    /// Set by the engine from the next step's resolved prompt template whenever
    /// `can_continue_in_current_container` is true.
    pub continue_prompt: Option<String>,
    pub continue_unavailable_reason: Option<String>,
    pub cancel_to_previous_unavailable_reason: Option<String>,
    pub finish_workflow_unavailable_reason: Option<String>,
    /// Reason `RestartCurrentStep` is unavailable / scoped. Set when the WCB
    /// is opened inside a parallel group (WI-0096 §10): restart only affects
    /// the focused container.
    pub restart_unavailable_reason: Option<String>,
    /// Total number of steps in the focused step's parallel group (0 when the
    /// focused step is not part of a multi-step parallel batch). Lets frontends
    /// scope the Workflow Control Board to the parallel context.
    pub parallel_peer_count: usize,
    /// Live peers of the focused step still running in the same parallel group
    /// (excludes the focused step itself). Non-zero disables back/finish.
    pub parallel_peers_running: usize,
    /// True when a container is currently running (mid-step). The engine
    /// computes this from `current_execution.is_some()` in
    /// `compute_available_actions`. Changes Esc semantics from Pause to Dismiss.
    pub can_dismiss: bool,
    /// Custom label for the right-arrow (launch-next) action. Defaults to
    /// "Next: new container" when `None`. The dynamic leader step sets this to
    /// "Start dynamic workflow" so CLI, TUI, and API frontends all render the
    /// same presentation hint without forking the rendering code (WI-0092 §8).
    pub launch_next_label: Option<String>,
    /// Set when the board is being shown *because* the focused step just
    /// failed (WI-0115 §1). Frontends render it as an error banner above the
    /// action list; `None` is the ordinary between-steps board.
    pub step_failure: Option<StepFailureContext>,
    /// The step the board is about: the failed step on a failure board,
    /// otherwise the step still running. `None` when the engine is between
    /// steps with nothing running — see [`focused_step_label`].
    ///
    /// Frontends used to scan `WorkflowState::step_states` for a `Running`
    /// entry themselves (WI 0114 F-18). The engine already knows.
    ///
    /// [`focused_step_label`]: AvailableActions::focused_step_label
    pub focused_step: Option<String>,
    /// Set when this board is the plain "advance to the next step?" case, for
    /// which a frontend may offer a lightweight confirm instead of the full
    /// control board. `None` means render the board.
    ///
    /// Whether the case applies is an engine judgement — it depends on the
    /// DAG's remaining work and on whether anything failed — so the engine
    /// makes it (WI 0114 F-18). A frontend that has no lightweight form
    /// ignores this and renders the board, exactly as the CLI does.
    pub simple_advance: Option<SimpleAdvance>,
    /// A step in the still-running parallel group that has already failed
    /// and can be relaunched right now via [`NextAction::RetryFailedStep`],
    /// without waiting for the rest of the group to drain. `None` outside a
    /// running parallel group or when no peer has failed.
    pub retry_failed_step: Option<String>,
    /// True when the board is opened while a parallel group is running. Then
    /// `RestartCurrentStep`, `CancelToPreviousStep` and `LaunchNext` act on the
    /// whole group, and the engine follows the choice up with questions of its
    /// own ([`crate::engine::workflow::frontend::WorkflowFrontend::ask_parallel_group`]).
    /// A frontend uses it only to label those actions accordingly.
    pub acts_on_parallel_group: bool,
}

/// An answer to one of the engine's follow-up questions about a running
/// parallel group, asked after a Workflow Control Board choice (restart,
/// back or next) that affects the whole group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelGroupDecision {
    /// Stop every running container and run the whole group from scratch.
    RestartWholeGroup,
    /// Restart just one agent; the engine then asks which.
    RestartOneAgent,
    /// Restart this member of the group in a fresh container.
    RestartStep(String),
    /// Yes: cancel the whole group (to go back, or to move on).
    CancelGroup,
    /// Leave the group running; the board choice is dropped.
    KeepRunning,
}

/// Where the workflow goes once a running parallel group is cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupExit {
    /// Back to these steps: the group's direct dependencies.
    Back(Vec<String>),
    /// On to these steps: those that depend directly on the group. Empty
    /// when nothing does, so the workflow finishes.
    Next(Vec<String>),
}

/// One member of a running parallel group and its current state, as handed to
/// [`ParallelGroupPrompts::restart_which`].
pub type GroupMember = (String, Option<StepState>);

/// The questions the engine asks about a running parallel group.
///
/// An engine authors no prompt copy (`crate::data::prompt`): Layer 2 supplies
/// these builders (`command::prompts::parallel_group_prompts`), and the engine
/// calls them with the facts it knows. Each prompt's `default_on_dismiss` is
/// [`ParallelGroupDecision::KeepRunning`].
#[derive(Debug, Clone, Copy)]
pub struct ParallelGroupPrompts {
    /// Restart the whole group, or one agent? Takes the group's members.
    pub restart_scope: fn(&[String]) -> Prompt<ParallelGroupDecision>,
    /// Which agent to restart? Takes every member with its current state.
    pub restart_which: fn(&[GroupMember]) -> Prompt<ParallelGroupDecision>,
    /// Confirm cancelling the group, and where the workflow goes after.
    pub cancel_group: fn(&[String], &GroupExit) -> Prompt<ParallelGroupDecision>,
}

impl AvailableActions {
    /// How a frontend names the step this board is about.
    ///
    /// The fallback is copy, so it lives here rather than in each frontend.
    pub fn focused_step_label(&self) -> &str {
        self.focused_step.as_deref().unwrap_or("current step")
    }
}

/// The plain "one step finished, one step left" board: no failures, nothing
/// running, and exactly one step still pending.
///
/// Carries both names so a frontend renders the confirm without going back to
/// `WorkflowState` for them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SimpleAdvance {
    /// The step the board is about, already defaulted — the same string
    /// [`AvailableActions::focused_step_label`] returns.
    pub completed_step: String,
    /// The single step still pending.
    pub next_step: String,
}

/// Why the Workflow Control Board is being shown after a step failure, and
/// what the recovery actions will actually do. Composed by the engine so every
/// frontend renders the same copy.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StepFailureContext {
    /// The step whose container exited non-zero.
    pub step_name: String,
    pub exit_code: i32,
    pub signal: Option<i32>,
    /// Human-readable detail lines (exit code, signal, run duration). Rendered
    /// verbatim, in order.
    pub detail_lines: Vec<String>,
    /// Step `CancelToPreviousStep` returns to, when one exists.
    pub previous_step: Option<String>,
    /// Step `LaunchNext` starts once the failed step is skipped, when one
    /// exists.
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YoloTickOutcome {
    Continue,
    Cancel,
    AdvanceNow,
}

/// Why a countdown reported through the `yolo_countdown_*` hooks is running.
///
/// The two share one reporting channel deliberately — an unattended frontend
/// surfaces both the same way (WI-0115 §3) — but they mean opposite things to
/// whoever is reading, so the frontend is told which it is rather than left to
/// assume the common one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CountdownKind {
    /// A step's container has gone quiet; when the countdown expires the
    /// engine kills it and advances to the next step.
    #[default]
    StuckStep,
    /// A step's container exited non-zero and no one can be asked what to do;
    /// when the countdown expires the engine retries that same step.
    FailureRetry,
}

/// What `step_once` returned: the step that just executed plus its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub step_name: String,
    pub status: WorkflowStepStatus,
    pub remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStepStatus {
    Pending,
    Running,
    Succeeded,
    Failed { exit_code: i32 },
    Cancelled,
    Skipped,
}

/// What `run_to_completion` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowOutcome {
    Completed,
    Paused,
    Aborted,
    Failed {
        last_step: String,
        exit_code: i32,
    },
    /// Main workflow completed but a teardown step with `abort_on_failure`
    /// failed. Post-workflow actions (worktree flows) should still run, but
    /// non-interactive contexts should default to keeping the worktree.
    CompletedTeardownFailed,
}

/// What the engine produces while a step's container streams output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutput {
    pub step_name: String,
    pub kind: StepOutputKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutputKind {
    Stdout,
    Stderr,
}

/// Information that `WorkflowFrontend::confirm_resume` receives when a
/// persisted workflow's hash differs from the current parsed file's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeMismatch {
    pub workflow_name: String,
    pub saved_hash: String,
    pub current_hash: String,
    pub message: String,
}

/// Yolo-countdown tick metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoloTick {
    pub remaining: Duration,
}

/// Per-step snapshot used by `WorkflowFrontend::report_workflow_progress`.
/// The engine pre-resolves agent/model so the frontend doesn't need to.
#[derive(Debug, Clone)]
pub struct WorkflowStepProgressInfo {
    pub name: String,
    /// Resolved agent name (step > workflow > config fallback, or "?" on error).
    pub agent: String,
    /// Resolved model, if any.
    pub model: Option<String>,
    /// Whether the step itself declares an `agent` or `model` field. When
    /// `false`, the resolved `agent`/`model` above come entirely from the
    /// project defaults, so the strip renders no agent/model label for the step.
    pub has_step_override: bool,
    pub status: WorkflowStepStatus,
    /// Steps this one depends on. Drives the topological column grouping in
    /// the Workflow Overview renderer.
    pub depends_on: Vec<String>,
    /// Effective workflow concurrency cap. `None` means unlimited.
    pub max_concurrent: Option<usize>,
}
