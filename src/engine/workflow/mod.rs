//! `engine::workflow` — `WorkflowEngine`.
//!
//! Owns every workflow-execution concern: state, advance logic, yolo
//! countdowns, agent/model resolution, exit-code interpretation, persistence,
//! and per-step container lifecycle. Forbidden: rendering, direct user
//! input, knowledge of which frontend is on the other side of the trait,
//! worktree lifecycle management, direct container construction.
//!
//! The engine is the single source of truth for ALL workflow state.
//! No workflow execution state lives in the frontend — zero, none.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;

use crate::data::config::effective::EffectiveConfig;
use crate::data::session::{AgentName, Session};
use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_state::{
    PhaseKind, StepState, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION,
};
use crate::data::workflow_state_store::WorkflowStateStore;
use crate::engine::agent_runtime::background::AgentExec;
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, CancelHandle, StuckEvent, KILLED_EXIT_CODE,
};
use crate::engine::agent_runtime::output_tail::OutputTail;
use crate::engine::container::options::OverlayPermission;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, CountdownKind, NextAction, ResumeMismatch, SimpleAdvance, StepFailureContext,
    StepOutcome, WorkflowOutcome, WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::{EngineHandles, WorkflowFrontend};
use crate::engine::workflow::step_commands::PhaseStepSpec;

pub mod actions;
pub mod factory;
pub mod frontend;
pub mod poll_ci;
pub mod step_commands;
pub mod timing;

// The `WorkflowEngine` impl, split across five child modules (WI 0114 F-51).
// Private, so nothing outside `engine::workflow` can name them: the engine's
// public surface is unchanged, because every method still hangs off
// `WorkflowEngine`, which is declared here.
mod control;
mod parallel;
mod phases;
mod queries;
mod single_step;

/// Result of a mid-step yolo countdown (step is still running while
/// the countdown ticks).
enum MidStepYoloResult {
    /// Step completed while the countdown was ticking.
    StepCompleted(StepOutcome),
    /// Countdown expired or user pressed AdvanceNow.
    Advanced,
    /// User pressed Esc: cancel the countdown.
    Cancelled,
    /// User pressed Ctrl-W: show the WCB instead.
    ShowControlBoard,
    /// Container recovered (StepUnstuck received).
    Recovered,
}

/// Result of mid-step control board interaction.
enum MidStepOutcome {
    /// User dismissed the dialog — resume waiting on the step.
    Continue,
    /// Step completed while dialog was open; outcome is ready.
    StepCompleted(StepOutcome),
    /// User chose a workflow-level action (pause/abort/finish).
    WorkflowEnded(WorkflowOutcome),
    /// User chose an action that re-enters the loop (restart/advance/etc).
    LoopContinue,
}

/// Result of one iteration of the outer `run_to_completion` loop (either a
/// single-step iteration or a parallel-group run).
enum IterationOutcome {
    /// The iteration finished; re-evaluate the outer loop (find the next batch).
    Continue,
    /// A workflow-level action ended the run.
    Ended(WorkflowOutcome),
}

/// A dynamically-sized set of container-wait futures, one per launched
/// parallel step. Each future resolves to `(step_name, exit_result)` when its
/// container terminates. Using `FuturesUnordered` (rather than a hand-rolled
/// `select!` array) lets the engine poll an arbitrary number of concurrent
/// containers.
type ParallelWaits = FuturesUnordered<
    std::pin::Pin<
        Box<dyn std::future::Future<Output = (String, Result<AgentExitInfo, EngineError>)> + Send>,
    >,
>;

/// Sender half of the unified stuck channel that fans every container's
/// per-step stuck broadcast into a single fixed-arity `select!` branch.
type StuckFanIn = tokio::sync::mpsc::UnboundedSender<(String, StuckEvent)>;

/// Result of `run_parallel_group`.
enum GroupOutcome {
    /// The whole group reached a terminal state. `failed` carries every
    /// non-abort step failure (name + exit code), in the order the containers
    /// exited, for the outer loop to walk through the failure recovery path;
    /// empty when every member succeeded/cancelled.
    ///
    /// Every failure is carried, not just the first: a step left `Failed` is
    /// not in `completed_steps`, so the DAG still reports it ready and the next
    /// iteration would relaunch it — silently, with no recovery board and no
    /// retry accounting (WI-0115 §1).
    Drained { failed: Vec<(String, i32)> },
    /// A workflow-level action ended the run (abort_on_failure, WCB abort/pause).
    Ended(WorkflowOutcome),
}

/// Result of `step_once_interruptible`.
enum InterruptibleStepResult {
    /// Step completed (naturally or while dialog was open).
    StepCompleted(StepOutcome),
    /// Mid-step action ended the workflow.
    WorkflowEnded(WorkflowOutcome),
    /// Mid-step action requires the outer loop to continue (restart/advance).
    LoopContinue,
}

pub use actions::{
    StepOutput, StepOutputKind, WorkflowOutcome as Outcome, WorkflowStepStatus as Status,
};
pub use factory::{AgentExecutionFactory as Factory, WorkflowRuntimeContext as RuntimeContext};
pub use frontend::WorkflowFrontend as Frontend;

/// Request sent from the TUI event loop (via per-tab channel) to the engine.
///
/// The frontend detects stuck/unstuck state and routes user actions;
/// the engine decides the response.
#[derive(Debug, Clone)]
pub enum EngineRequest {
    /// User pressed Ctrl-W. Engine should show the WCB for `step_name`
    /// (the currently-focused container in a parallel group; the single
    /// running step otherwise).
    OpenControlBoard { step_name: String },
    /// Frontend detected that `step_name`'s container is stuck
    /// (no PTY output for STUCK_TIMEOUT). Engine responds: if --yolo,
    /// start yolo countdown; if not --yolo, open WCB.
    StepStuck { step_name: String },
    /// Frontend detected that `step_name`'s container is no longer stuck
    /// (new PTY output arrived). Engine cancels any active yolo countdown.
    StepUnstuck { step_name: String },
}

/// One running (or just-launched) container in a parallel group.
///
/// The engine owns all concurrency state; this is the per-slot record it keeps
/// while a step's container is alive. In the single-step path exactly one entry
/// exists (`active_steps[0]`, the "focused" step) and it retains its
/// `execution` for prompt injection / put-back. In the multi-step parallel
/// path each launched step gets its own entry; the `execution` is moved into a
/// background wait future, so the entry keeps only a `cancel_handle` for
/// engine-initiated kills (yolo expiry, abort_on_failure, WCB abort/pause).
struct ActiveParallelStep {
    step_name: String,
    /// Retained only by the single-step path (for inject / put-back after the
    /// wait task resolves). `None` in the multi-step path, where the execution
    /// lives inside the FuturesUnordered wait future.
    execution: Option<AgentExecution>,
    /// Standalone kill handle, extracted before the execution is moved into a
    /// wait future. Used by the multi-step path to kill just this container.
    cancel_handle: Option<CancelHandle>,
    /// The container's name, retained so a failure log can be named after it
    /// once the execution has been consumed by its wait future.
    container_name: String,
    /// Rolling buffer of this container's recent combined stdout/stderr,
    /// extracted from the execution at launch so it outlives the wait future.
    /// `None` for runtimes without a byte-stream bridge (e.g. sandbox-class).
    output_tail: Option<Arc<OutputTail>>,
    /// Set when awman itself terminated this container (yolo auto-advance, WCB
    /// abort/pause/finish, stuck cancel, startup-grace kill, abort_on_failure
    /// peer kill). A non-zero exit on an awman-killed container is expected and
    /// must NOT produce a failure log.
    awman_killed: bool,
    /// Whether this step is currently marked stuck (non-yolo stuck handling).
    stuck: bool,
    /// When a per-step yolo countdown is running, the instant it expires.
    yolo_deadline: Option<Instant>,
    agent: AgentName,
    model: Option<String>,
}

/// What a workflow run is: the definition, the work item it serves, and where
/// its state file lives.
///
/// Together with [`WorkflowEngineDeps`] this replaces the five- and
/// six-parameter `new` / `resume` / `resume_with_state_root` constructors
/// (F-34).
pub struct WorkflowSpec {
    pub workflow: Workflow,
    pub work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
    /// Root for the workflow-state file. `None` uses the session's git root,
    /// which is where `exec workflow` has always kept it.
    ///
    /// The state store creates, rewrites and deletes its file, so a caller
    /// whose session root must stay untouched between runs — a squad task
    /// bound to its durable workspace (WI 0106 §6a) — points this at a
    /// run-scoped directory instead.
    pub state_root: Option<std::path::PathBuf>,
}

impl WorkflowSpec {
    /// A spec for a session-rooted run of `workflow`.
    pub fn new(workflow: Workflow) -> Self {
        Self {
            workflow,
            work_item_context: None,
            state_root: None,
        }
    }

    pub fn with_work_item_context(
        mut self,
        ctx: Option<crate::data::workflow_prompt_template::WorkItemContext>,
    ) -> Self {
        self.work_item_context = ctx;
        self
    }

    pub fn with_state_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.state_root = root;
        self
    }
}

/// The two Layer 2/3 collaborators a `WorkflowEngine` needs: somewhere to
/// report to and ask questions of, and something that turns a step into a
/// running agent.
pub struct WorkflowEngineDeps {
    pub frontend: Box<dyn WorkflowFrontend>,
    pub agent_factory: Box<dyn AgentExecutionFactory>,
}

pub struct WorkflowEngine {
    session: Session,
    workflow: Workflow,
    dag: WorkflowDag,
    state: WorkflowState,
    state_store: WorkflowStateStore,
    effective_config: EffectiveConfig,
    frontend: Box<dyn WorkflowFrontend>,
    agent_factory: Box<dyn AgentExecutionFactory>,
    /// Containers currently alive. The single-step path keeps exactly one
    /// entry (the focused step); the parallel path keeps up to `max_concurrent`.
    active_steps: Vec<ActiveParallelStep>,
    /// Resolved once at construction from `effective_max_concurrent_agents()`.
    /// `None` means unlimited.
    max_concurrent: Option<usize>,
    current_step_name: Option<String>,
    current_step_agent: Option<AgentName>,
    current_step_model: Option<String>,
    work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
    workflow_context_permission: Option<OverlayPermission>,
    yolo: bool,
    abort_on_failure_triggered: bool,
    last_exit_info: Option<AgentExitInfo>,
    /// Automatic credential refresh/retry is permitted once per workflow step.
    /// Kept independently of an individual container slot so a relaunch cannot
    /// reset the guard.
    auth_retries_used: HashSet<String>,
    /// Steps the unattended failure path has already auto-retried once. Kept
    /// separate from `auth_retries_used` so a credential refresh and a failure
    /// retry cannot consume each other.
    auto_retried_steps: HashSet<String>,
    engine_rx: Option<tokio::sync::mpsc::UnboundedReceiver<EngineRequest>>,
    /// Where to mirror this run's summary after every persist, so a session
    /// view can render the run without loading the state file (decision Q3,
    /// WI 0114 F-22). Set by the Layer 2 caller that owns the session handle;
    /// `None` for a run whose caller has no session to mirror into (the squad
    /// daemon's per-task runs, and every engine test).
    ///
    /// Layer 1 holds a Layer 0 `Session` either way — this is the *shared*
    /// handle rather than the engine's own clone, which is what makes the
    /// mirror visible to anyone else.
    session_mirror: Option<Arc<tokio::sync::RwLock<Session>>>,
}

impl WorkflowEngine {
    fn msg_info(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: text.into(),
            });
    }
    fn msg_warning(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: text.into(),
            });
    }
    fn msg_success(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Success,
                text: text.into(),
            });
    }
    fn msg_error(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Error,
                text: text.into(),
            });
    }

    /// Persist a failed step container's buffered output to
    /// `~/.awman/logs/{workflow-id}-{step-name}-{container-name}.log` and point
    /// the user at the file. Called only when a step container exits non-zero
    /// on its own (awman did not kill it). Best-effort: a resolve/write failure
    /// downgrades to a warning rather than derailing the workflow.
    fn dump_container_failure_log(
        &mut self,
        step_name: &str,
        container_name: &str,
        tail: &OutputTail,
        exit_code: i32,
    ) {
        let contents = tail.snapshot_text();
        let paths = match crate::data::fs::WorkflowLogPaths::from_env(self.session.env()) {
            Ok(p) => p,
            Err(e) => {
                self.msg_warning(format!(
                    "Step '{step_name}' container '{container_name}' exited with code \
                     {exit_code}, but the log directory could not be resolved to save its \
                     output: {e}"
                ));
                return;
            }
        };
        match paths.write_container_log(
            self.state.invocation_id,
            step_name,
            container_name,
            &contents,
        ) {
            Ok(path) => self.msg_error(format!(
                "Step '{step_name}' container '{container_name}' exited with code {exit_code}. \
                 Recent output saved to {}",
                path.display()
            )),
            Err(e) => self.msg_warning(format!(
                "Step '{step_name}' container '{container_name}' exited with code {exit_code}, \
                 but writing its output log failed: {e}"
            )),
        }
    }

    /// If a just-finished step container failed on its own (non-zero exit that
    /// awman did not cause) and a captured output tail exists, flush it to a
    /// failure log. Reads the slot for `step_name` if it is still present.
    fn maybe_dump_step_failure(&mut self, step_name: &str, exit_code: i32) {
        if exit_code == 0 {
            return;
        }
        let dump = self
            .active_steps
            .iter()
            .find(|s| s.step_name == step_name)
            .filter(|s| !s.awman_killed)
            .and_then(|s| {
                s.output_tail
                    .clone()
                    .map(|tail| (s.container_name.clone(), tail))
            });
        if let Some((container_name, tail)) = dump {
            self.dump_container_failure_log(step_name, &container_name, &tail, exit_code);
        }
    }

    /// Mark the focused (single-step) container as awman-killed so a subsequent
    /// non-zero exit is treated as expected and produces no failure log.
    fn mark_focused_killed(&mut self) {
        if let Some(s) = self.active_steps.first_mut() {
            s.awman_killed = true;
        }
    }

    /// Start a fresh run of `spec`, discarding any persisted state.
    pub fn new(
        session: &Session,
        spec: WorkflowSpec,
        deps: WorkflowEngineDeps,
    ) -> Result<Self, EngineError> {
        let WorkflowSpec {
            workflow,
            work_item_context,
            state_root,
        } = spec;
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let workflow_hash = compute_workflow_hash(&workflow);
        let work_item_number = work_item_context.as_ref().map(|c| c.number);
        let state = WorkflowState::new(
            workflow_name_for(&workflow),
            &workflow.steps,
            workflow_hash,
            work_item_number,
        );
        Ok(Self::assemble(
            session,
            workflow,
            dag,
            state,
            state_store_for(session, state_root),
            work_item_context,
            deps,
        ))
    }

    pub fn abort_on_failure_triggered(&self) -> bool {
        self.abort_on_failure_triggered
    }

    /// The resolved per-workflow concurrency cap (`None` = unlimited). Resolved
    /// once at construction from `effective_max_concurrent_agents()`. Frontends
    /// read this to size the Workflow Overview / parallel UX.
    pub fn max_concurrent(&self) -> Option<usize> {
        self.max_concurrent
    }

    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Override the active workflow-context overlay permission after the command
    /// layer has merged config/env/CLI/workflow overlay sources.
    pub fn set_workflow_context_permission(&mut self, permission: Option<OverlayPermission>) {
        self.workflow_context_permission = permission;
    }

    /// The focused step's live execution (the single running step, or the first
    /// parallel slot). `None` while a wait future owns it or between
    /// launch/finalize.
    fn focused_execution(&self) -> Option<&AgentExecution> {
        self.active_steps.first().and_then(|s| s.execution.as_ref())
    }

    /// Put an execution back into the focused slot after its wait future
    /// resolves (single-step path).
    fn set_focused_execution(&mut self, exec: AgentExecution) {
        if let Some(s) = self.active_steps.first_mut() {
            s.execution = Some(exec);
        }
    }

    /// Move the focused slot's execution out (single-step path spawns a wait
    /// task that owns it).
    fn take_focused_execution(&mut self) -> Option<AgentExecution> {
        self.active_steps
            .first_mut()
            .and_then(|s| s.execution.take())
    }

    /// Resume `spec` from persisted state. Calls `confirm_resume` on the
    /// frontend if the workflow hash has drifted.
    ///
    /// `WorkflowSpec::state_root` says where the state file lives; `None`
    /// keeps it under the session's git root, the same place `exec workflow`
    /// has always kept it. (`resume_with_state_root` is gone — the root is a
    /// field of the spec now, F-34.)
    pub async fn resume(
        session: &Session,
        spec: WorkflowSpec,
        deps: WorkflowEngineDeps,
    ) -> Result<Self, EngineError> {
        let WorkflowSpec {
            workflow,
            work_item_context,
            state_root,
        } = spec;
        let WorkflowEngineDeps {
            mut frontend,
            agent_factory,
        } = deps;
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let store = state_store_for(session, state_root);
        let workflow_name = workflow_name_for(&workflow);
        let work_item_number = work_item_context.as_ref().map(|c| c.number);
        let saved = store.load(work_item_number, &workflow_name)?;

        let workflow_hash = compute_workflow_hash(&workflow);
        let mut state = match saved {
            Some(saved) => {
                if saved.schema_version > WORKFLOW_STATE_SCHEMA_VERSION {
                    return Err(EngineError::UnsupportedWorkflowSchemaVersion {
                        found: saved.schema_version,
                        supported: WORKFLOW_STATE_SCHEMA_VERSION,
                    });
                }
                if saved.workflow_hash != workflow_hash {
                    let mismatch = ResumeMismatch {
                        workflow_name: workflow_name.clone(),
                        saved_hash: saved.workflow_hash.clone(),
                        current_hash: workflow_hash.clone(),
                        message: "workflow source has changed since the saved run".into(),
                    };
                    if !frontend.confirm_resume(&mismatch)? {
                        return Err(EngineError::WorkflowResumeIncompatible(
                            "user declined to resume against drifted workflow".into(),
                        ));
                    }
                }
                saved
            }
            None => WorkflowState::new(
                workflow_name,
                &workflow.steps,
                workflow_hash,
                work_item_number,
            ),
        };

        // Drop step entries the workflow no longer defines. A saved state is
        // matched to the workflow by hash, and the user may have accepted the
        // drift prompt above against a file that since lost or renamed a step.
        // Such an entry can never be launched — `next_ready` reads the DAG, not
        // `step_states` — but `is_complete()` reads `step_states`, so leaving a
        // non-terminal orphan behind means the run can never finish: it would
        // end on "no ready steps remaining" instead. Pruning is safe because
        // the DAG is the only thing that decides what actually runs.
        let orphans = state.retain_steps_in(&dag);
        if !orphans.is_empty() {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: format!(
                    "The saved run has steps this workflow no longer defines: {}. Dropping them.",
                    orphans.join(", "),
                ),
            });
        }

        let interrupted = state.interrupted_running_steps();
        if !interrupted.is_empty() {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: format!(
                    "Interrupted steps detected (prior crash?): {}. Resetting to Pending.",
                    interrupted.join(", "),
                ),
            });
            for name in &interrupted {
                state.set_status(name, StepState::Pending);
            }
        }

        // Steps the saved run left `Failed` or `Cancelled` are reset the same
        // way, for the same reason: they are terminal but not *done*.
        //
        // A run that ended on a failure — or was aborted, which cancels every
        // remaining step — saves a state in which every step is terminal.
        // `is_complete()` reads that as finished, so resuming it without this
        // reset would report instant success and run nothing (WI-0115 §2).
        // Succeeded and Skipped steps are untouched, so a resume still picks up
        // where the previous run genuinely got to.
        let unrecovered = state.unrecovered_steps();
        if !unrecovered.is_empty() {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: format!(
                    "Previous run left these steps unfinished: {}. Resetting to Pending.",
                    unrecovered.join(", "),
                ),
            });
            for name in &unrecovered {
                state.set_status(name, StepState::Pending);
            }
        }

        Ok(Self::assemble(
            session,
            workflow,
            dag,
            state,
            store,
            work_item_context,
            WorkflowEngineDeps {
                frontend,
                agent_factory,
            },
        ))
    }

    /// The struct literal `new` and `resume` share. Both had their own copy
    /// before F-34, and a field added to one could silently miss the other.
    fn assemble(
        session: &Session,
        workflow: Workflow,
        dag: WorkflowDag,
        state: WorkflowState,
        state_store: WorkflowStateStore,
        work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
        deps: WorkflowEngineDeps,
    ) -> Self {
        let WorkflowEngineDeps {
            mut frontend,
            agent_factory,
        } = deps;
        let workflow_context_permission =
            workflow_context_permission_from_overlay_strings(workflow.overlays.as_deref());
        let effective_config = session.effective_config();
        let max_concurrent = effective_config.effective_max_concurrent_agents();
        tracing::debug!(
            ?max_concurrent,
            "workflow_engine resolved max_concurrent_agents"
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.attach_engine(EngineHandles::requests(tx));
        Self {
            session: session.clone(),
            workflow,
            dag,
            state,
            state_store,
            effective_config,
            frontend,
            agent_factory,
            active_steps: Vec::new(),
            max_concurrent,
            current_step_name: None,
            current_step_agent: None,
            current_step_model: None,
            work_item_context,
            workflow_context_permission,
            yolo: false,
            abort_on_failure_triggered: false,
            last_exit_info: None,
            auth_retries_used: HashSet::new(),
            auto_retried_steps: HashSet::new(),
            engine_rx: Some(rx),
            session_mirror: None,
        }
    }

    /// Mirror this run's summary into `session`'s `SessionState` after every
    /// persist. See [`WorkflowEngine::session_mirror`].
    pub fn mirror_into_session(mut self, session: Arc<tokio::sync::RwLock<Session>>) -> Self {
        self.session_mirror = Some(session);
        self
    }

    pub fn state(&self) -> &WorkflowState {
        &self.state
    }

    /// Drive every step until the workflow finishes, the user pauses, or a
    /// step fails terminally.
    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError> {
        let completed_count = self.state.completed_steps.len();
        let total_count = self.workflow.steps.len();
        if completed_count > 0 {
            self.msg_info(format!(
                "Resuming workflow '{}' ({}/{} steps completed)",
                self.state.workflow_name, completed_count, total_count,
            ));
        } else {
            self.msg_info(format!(
                "Starting workflow '{}' ({} steps)",
                self.state.workflow_name, total_count,
            ));
        }

        let initial_progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&initial_progress);

        loop {
            if self.state.is_complete() {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);
                self.msg_success(format!(
                    "Workflow '{}' completed successfully",
                    self.state.workflow_name,
                ));
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                return Ok(outcome);
            }

            // Determine the current parallel group: every step whose
            // dependencies are already satisfied (source-file order). When more
            // than one is ready and the concurrency cap is not pinned to 1, run
            // them through the parallel-group path; otherwise fall back to the
            // single-step interactive path (behaviourally identical to the
            // pre-WI-0096 sequential engine).
            let ready = self.next_ready_steps()?;
            let use_parallel = ready.len() > 1 && self.max_concurrent != Some(1);

            if use_parallel {
                match self.run_parallel_group(ready).await? {
                    GroupOutcome::Ended(wo) => return Ok(wo),
                    GroupOutcome::Drained { failed } => {
                        // One board (or one unattended retry) per failed step,
                        // in exit order. Recovering the first failure must not
                        // leave its peers to be silently relaunched.
                        for (name, exit_code) in failed {
                            match self.handle_group_step_failure(&name, exit_code).await? {
                                IterationOutcome::Continue => {}
                                IterationOutcome::Ended(wo) => return Ok(wo),
                            }
                        }
                        continue;
                    }
                }
            }

            match self.run_single_step_iteration().await? {
                IterationOutcome::Continue => continue,
                IterationOutcome::Ended(wo) => return Ok(wo),
            }
        }
    }
}

/// Container path where the failure file is visible when a writable
/// `context(workflow)` overlay is active (already mounted read-write).
const PHASE_FAILURE_OVERLAY_CONTAINER_PATH: &str = "/awman/context/workflow";
/// Container path for the one-off read-only remediation mount used when no
/// writable `context(workflow)` overlay is active.
const PHASE_FAILURE_EPHEMERAL_CONTAINER_PATH: &str = "/awman/remediation";
/// Per-stream truncation cap (~100 KB). Only the tail is retained since the
/// most recent output is the most relevant to a failure.
const TEARDOWN_STREAM_TRUNCATE_BYTES: usize = 100 * 1024;

/// What a whole shell phase did: whether an `abort_on_failure` step stopped
/// it, and whether any step failed at all.
///
/// This was teardown's `(bool, bool)` return before F-34 — setup returned
/// `Result<(), _>` and signalled an abort with an `Err`. One phase runner,
/// one outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseOutcome {
    /// An `abort_on_failure` step failed and stopped the phase.
    pub aborted: bool,
    /// At least one step failed, whether or not it aborted the phase.
    pub any_failed: bool,
}

/// Outcome of one phase shell step: whether it failed, plus the
/// captured stdout/stderr (populated on failure, fed to the remediation
/// agent's failure file). Kept deliberately narrow rather
/// than storing a full `ExecOutput` in `PhaseStepStatus`.
struct PhaseStepOutcome {
    failed: bool,
    stdout: String,
    stderr: String,
}

impl PhaseStepOutcome {
    fn succeeded() -> Self {
        Self {
            failed: false,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn failed(stdout: String, stderr: String) -> Self {
        Self {
            failed: true,
            stdout,
            stderr,
        }
    }
}

/// The captured output of a failed phase command, passed into
/// `launch_on_failure_agent` so it can materialize the remediation failure
/// file. Carries the phase so the file is named for it — setup and teardown
/// share one run directory and may declare steps with the same name.
struct PhaseFailureContext<'a> {
    kind: PhaseKind,
    step_name: &'a str,
    stdout: &'a str,
    stderr: &'a str,
}

/// Resolved artifacts after successfully writing the phase-failure file.
struct PhaseFailureArtifacts {
    /// Container path of the directory holding the file.
    container_path: &'static str,
    /// The written file's name (`<setup|teardown>-failure-<sanitized>.txt`).
    filename: String,
    /// Which phase the failed step belongs to, for the prompt preamble.
    kind: PhaseKind,
    /// The original (unsanitized) step name, for the prompt preamble.
    step_name: String,
    /// A one-off `host:container:ro` overlay to add to the synthetic step, or
    /// `None` when the writable `context(workflow)` overlay already mounts it.
    extra_overlay: Option<String>,
}

impl PhaseFailureArtifacts {
    /// Prepend the fixed system-prompt preamble pointing the agent at the
    /// failure file to the user's remediation prompt.
    fn prepend_preamble(&self, user_prompt: &str) -> String {
        format!(
            "The full output (stdout and stderr) of the failed {phase} step \"{step}\" has been\n\
             written to {path}/{file}.\n\
             Read that file first to understand the failure before attempting a fix.\n\
             \n\
             ---\n\
             \n\
             {user_prompt}",
            phase = self.kind.label(),
            step = self.step_name,
            path = self.container_path,
            file = self.filename,
        )
    }
}

fn workflow_context_permission_from_overlay_strings(
    overlays: Option<&[String]>,
) -> Option<OverlayPermission> {
    let overlays = overlays?;
    for entry in overlays {
        for token in entry.split(',') {
            let token = token.trim();
            let Some(inner) = token
                .strip_prefix("context(")
                .and_then(|s| s.strip_suffix(')'))
            else {
                continue;
            };
            let mut parts = inner.splitn(2, ':');
            let scope = parts.next().unwrap_or("").trim();
            if scope != "workflow" {
                continue;
            }
            return match parts.next().map(str::trim).unwrap_or("rw") {
                "ro" => Some(OverlayPermission::ReadOnly),
                _ => Some(OverlayPermission::ReadWrite),
            };
        }
    }
    None
}

/// Sanitize a step name into a safe filename component: non-alphanumeric
/// characters (including `/`, `\`, `..`, spaces, and shell metacharacters)
/// become `-`, and the result is truncated to 64 characters.
fn sanitize_step_name_for_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(64);
    if out.is_empty() {
        out.push_str("step");
    }
    out
}

/// Retain the last [`TEARDOWN_STREAM_TRUNCATE_BYTES`] of a stream, prefixing a
/// truncation notice when content was dropped. Truncation respects UTF-8
/// character boundaries.
fn truncate_stream(content: &str) -> String {
    if content.len() <= TEARDOWN_STREAM_TRUNCATE_BYTES {
        return content.to_string();
    }
    let start = content.len() - TEARDOWN_STREAM_TRUNCATE_BYTES;
    // Advance to the next char boundary so we never slice mid-codepoint.
    let start = (start..content.len())
        .find(|&i| content.is_char_boundary(i))
        .unwrap_or(content.len());
    format!(
        "[... output truncated, showing last {} KB ...]\n{}",
        TEARDOWN_STREAM_TRUNCATE_BYTES / 1024,
        &content[start..]
    )
}

/// Format the teardown-failure file body in the fixed Feature B layout,
/// substituting `(empty)` for blank streams and truncating oversized output.
fn format_phase_failure_file(step_name: &str, stdout: &str, stderr: &str) -> String {
    let render = |s: &str| -> String {
        if s.is_empty() {
            "(empty)".to_string()
        } else {
            truncate_stream(s)
        }
    };
    format!(
        "=== FAILED COMMAND: {step_name} ===\n\
         \n\
         --- STDOUT ---\n\
         {stdout}\n\
         \n\
         --- STDERR ---\n\
         {stderr}\n",
        step_name = step_name,
        stdout = render(stdout),
        stderr = render(stderr),
    )
}

/// The state store for a run: rooted at `state_root` when the caller named
/// one, otherwise at the session's git root.
fn state_store_for(
    session: &Session,
    state_root: Option<std::path::PathBuf>,
) -> WorkflowStateStore {
    match state_root {
        Some(root) => WorkflowStateStore::at_git_root(root),
        None => WorkflowStateStore::new(session),
    }
}
/// Hash a workflow's steps + title to detect drift.
fn compute_workflow_hash(workflow: &Workflow) -> String {
    let json = serde_json::to_string(workflow).unwrap_or_default();
    let h = ring::digest::digest(&ring::digest::SHA256, json.as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn workflow_name_for(workflow: &Workflow) -> String {
    workflow.title.as_deref().unwrap_or("workflow").to_string()
}

#[cfg(test)]
mod tests;
