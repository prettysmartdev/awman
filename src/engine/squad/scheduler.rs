//! The squad tick loop (Layer 1).
//!
//! [`SquadScheduler`] wakes on a fixed 30-second cadence — independent of any
//! task's own interval — re-reads global config, asks the store which
//! tasks are due (a decision taken *wholly in SQL*, never re-derived
//! here), and dispatches each onto a bounded task set. For every due task
//! it opens an `squad_runs` row, delegates the actual evaluation to a
//! [`TaskEvaluator`], and records the terminal status. Repeatedly-failing
//! tasks grow an exponential backoff so a persistent auth or rate-limit
//! error does not re-fire every tick.
//!
//! The scheduler shares no code with the API server's `QueueWorker`: their work
//! models differ (claim-next-queued vs. select-by-elapsed-interval) and the
//! honest shared surface is ~20 lines of poll loop, duplicated deliberately.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::data::config::global::task_squad_config;
use crate::data::config::{EnvSnapshot, GlobalConfig};
use crate::data::fs::SquadRunPaths;
use crate::data::fs::{RunDetail, RunId, RunStatus, SquadPaths, Task, TaskStore};
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::container::naming::parse_squad_task_slug;
use crate::engine::squad::env_state::DaemonEnvState;

use super::evaluator::{EvaluationOutcome, EvaluationRequest, RunProgress, TaskEvaluator};

/// A leader's resolved identity for one task, as the Layer 2 evaluator
/// reported it. The scheduler logs this instead of re-deriving it (F-46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLeader {
    pub agent: String,
    pub model: Option<String>,
}

/// Records the live workflow-state path on the run row the moment the generated
/// workflow starts, and the leader's resolved agent/model as soon as Layer 2
/// has chosen it. The store stays the scheduler's to write.
struct StoreRunProgress {
    store: Arc<TaskStore>,
    resolved_leaders: Arc<Mutex<HashMap<String, ResolvedLeader>>>,
}

impl RunProgress for StoreRunProgress {
    fn workflow_started(
        &self,
        run_id: &RunId,
        workflow_path: &std::path::Path,
        state_path: &std::path::Path,
    ) {
        if let Err(error) =
            self.store
                .set_workflow_state_path(run_id, Some(workflow_path), Some(state_path))
        {
            tracing::warn!("squad: failed to record workflow state path: {error}");
        }
    }

    fn leader_resolved(&self, task: &str, agent: &str, model: Option<&str>) {
        self.resolved_leaders.lock().unwrap().insert(
            task.to_string(),
            ResolvedLeader {
                agent: agent.to_string(),
                model: model.map(str::to_string),
            },
        );
    }
}

/// The scheduler's wake cadence. Independent of any task's interval: a
/// task with a 5-minute interval is *considered* every 30s and *selected*
/// once its interval has elapsed (the interval check lives in the SQL).
pub const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// The exponential-backoff ceiling for a repeatedly-failing task.
const MAX_BACKOFF_SECS: u64 = 6 * 60 * 60; // 6 hours

/// A snapshot of the scheduler's liveness, read by `GET /v1/status`.
#[derive(Debug, Clone, Default)]
pub struct SchedulerStatus {
    /// When the last tick ran.
    pub last_tick: Option<DateTime<Utc>>,
    /// How many ticks have run since the scheduler started.
    pub tick_count: u64,
    /// How many evaluations are executing right now.
    pub in_flight: usize,
    /// One cancellation handle per in-flight evaluation, keyed by task id.
    /// `squad cancel` fires it; the evaluation removes its own entry when it
    /// ends, however it ends.
    pub cancellations: HashMap<String, RunCancellation>,
}

/// The handle `squad cancel` uses to stop one in-flight evaluation.
#[derive(Debug, Clone)]
pub struct RunCancellation {
    /// The run this handle stops, so a finishing run never removes the handle
    /// of a newer run for the same task.
    pub run_id: RunId,
    token: CancellationToken,
}

impl SchedulerStatus {
    /// Cancel the in-flight run of the task with id `task_id`, returning the
    /// run it cancelled, or `None` when nothing is running for that task.
    ///
    /// Only signals: the evaluation itself records the run as `canceled` and
    /// stops the task's containers, because it is the one holding the
    /// evaluation to drop.
    pub fn cancel_run(&self, task_id: &str) -> Option<RunId> {
        let cancellation = self.cancellations.get(task_id)?;
        cancellation.token.cancel();
        Some(cancellation.run_id.clone())
    }
}

/// The always-on task scheduler.
pub struct SquadScheduler {
    store: Arc<TaskStore>,
    paths: SquadPaths,
    evaluator: Arc<dyn TaskEvaluator>,
    env: EnvSnapshot,
    status: Arc<Mutex<SchedulerStatus>>,
    /// Consecutive-failure counts per task id, driving backoff growth.
    /// In-memory only: a daemon restart resets the count to zero (the
    /// conservative choice — restart is a fresh chance), while the last
    /// `backoff_until` persists in SQLite and keeps the task parked until
    /// it elapses.
    failure_counts: Arc<Mutex<HashMap<String, u32>>>,
    /// The production daemon's runtime, used to report currently-running
    /// containers on the scheduler tick and to stop a canceled run's
    /// containers. It is optional so deterministic
    /// scheduler unit tests do not need a container runtime fixture.
    runtime: Option<Arc<dyn AgentRuntimeEngine>>,
    /// The daemon's payload-environment state (WI 0116 §4), consulted once per
    /// run to record which declared `env()` names had no value when the run
    /// opened. Optional so deterministic scheduler unit tests need no daemon.
    env_state: Option<Arc<DaemonEnvState>>,
    /// The wake cadence. Always [`TICK_INTERVAL`] in production; tests shorten
    /// it so multi-tick behaviour (the concurrency bound, live config re-read)
    /// is observable without a 30-second wait.
    tick_interval: Duration,
    /// Per-task leader identity as Layer 2 resolved it, reported through
    /// `RunProgress::leader_resolved` (F-46). The running-agent log reads
    /// this instead of re-parsing `defaultLeader`/`agentsToModels`.
    resolved_leaders: Arc<Mutex<HashMap<String, ResolvedLeader>>>,
}

impl SquadScheduler {
    pub fn new(
        store: Arc<TaskStore>,
        paths: SquadPaths,
        evaluator: Arc<dyn TaskEvaluator>,
        env: EnvSnapshot,
    ) -> Self {
        Self {
            store,
            paths,
            evaluator,
            env,
            status: Arc::new(Mutex::new(SchedulerStatus::default())),
            failure_counts: Arc::new(Mutex::new(HashMap::new())),
            runtime: None,
            env_state: None,
            tick_interval: TICK_INTERVAL,
            resolved_leaders: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Share the daemon's payload-environment state, so each run records the
    /// declared `env()` names it started without (WI 0116 §4b).
    pub fn with_env_state(mut self, env_state: Arc<DaemonEnvState>) -> Self {
        self.env_state = Some(env_state);
        self
    }

    /// Override the wake cadence. Production never calls this; it exists so a
    /// test can observe more than one tick of a genuinely running scheduler.
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Enable the production-only running-container summary. This remains a
    /// tick-side observation, not another polling loop.
    pub fn with_runtime(mut self, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// A shared handle to the scheduler's status, cloned before `run` consumes
    /// the scheduler so the daemon's HTTP layer can read liveness.
    pub fn status_handle(&self) -> Arc<Mutex<SchedulerStatus>> {
        Arc::clone(&self.status)
    }

    /// Run the tick loop until `shutdown` is cancelled, then drain in-flight
    /// evaluations before returning.
    pub async fn run(self, shutdown: CancellationToken) {
        let mut tasks: JoinSet<()> = JoinSet::new();
        loop {
            // Reap any finished evaluations so the set does not grow unbounded.
            while tasks.try_join_next().is_some() {}

            self.tick(&mut tasks);

            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(self.tick_interval) => {}
            }
        }

        // Stop accepting new work; let the in-flight evaluations finish.
        while tasks.join_next().await.is_some() {}
    }

    /// One tick: re-read config, select due tasks, dispatch each.
    fn tick(&self, tasks: &mut JoinSet<()>) {
        // Re-read config every tick — never cached at startup — so guidance /
        // agentsToModels / defaultLeader / maxConcurrentEvaluations edits take
        // effect on the next tick with no restart. A malformed config that
        // fails to load falls back to defaults rather than stalling the loop.
        let cfg = GlobalConfig::load_with(&self.env).unwrap_or_default();
        let squad = cfg.squad.unwrap_or_default();
        let max_concurrent = squad.max_concurrent_evaluations_or_default().max(1);

        self.log_running_agents();

        let now = Utc::now();
        {
            // The tick count and timestamp are still recorded — `squad status`
            // reports both — but the tick itself is deliberately not logged.
            // A line every 30 seconds forever buried the lines that say
            // something actually happened. `log_running_agents` above is the
            // user-facing summary, and it emits nothing when nothing is
            // running.
            let mut status = self.status.lock().expect("scheduler status poisoned");
            status.last_tick = Some(now);
            status.tick_count += 1;
        }

        // The whole admission predicate — active, off-backoff, interval
        // elapsed, and not already running — is enforced in SQL. Never
        // re-derive it here.
        let due = match self.store.due_for_evaluation(now) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!("squad: due_for_evaluation failed: {error}");
                return;
            }
        };
        if due.is_empty() {
            return;
        }

        // `maxConcurrentEvaluations` bounds the scheduler's *whole* in-flight
        // set, not one tick's fan-out: capacity is what the cap leaves after
        // evaluations still running from earlier ticks. Re-reading the cap each
        // tick is what makes a config edit take effect without a restart.
        //
        // This is a dispatch bound, not a second admission predicate — the
        // four admission rules stay wholly in `due_for_evaluation`'s SQL.
        let capacity = max_concurrent.saturating_sub(self.in_flight());
        if capacity == 0 {
            tracing::info!(
                max_concurrent,
                due = due.len(),
                "squad: at the concurrency cap; deferring due tasks to a later tick"
            );
            return;
        }
        let deferred = due.len().saturating_sub(capacity);
        if deferred > 0 {
            tracing::info!(
                deferred,
                capacity,
                "squad: dispatching up to the concurrency cap this tick"
            );
        }

        for task in due.into_iter().take(capacity) {
            // The run row is opened *before* the task is spawned, so the
            // task is excluded from the very next tick's SQL selection by
            // its own `running` row. Opening it inside the task would let a
            // queued evaluation be selected a second time.
            let started_at = Utc::now();
            let task_dir = match self.paths.task_dir(&task.name) {
                Ok(dir) => dir,
                Err(error) => {
                    tracing::warn!(
                        "squad: cannot resolve task dir for {:?}: {error}",
                        task.name
                    );
                    continue;
                }
            };
            // Snapshotted here, at run start, and never rewritten: the point is
            // to explain the environment this run's containers actually started
            // with, so a push landing mid-run must not retro-edit the record.
            // A run with unmet names *proceeds* — every existing task was
            // created under the old silent-drop behaviour and some legitimately
            // treat a variable as optional, so failing them closed would be a
            // regression dressed as a fix. The gain is that the omission is
            // visible instead of silent.
            let unmet_env = match &self.env_state {
                Some(state) => {
                    let unmet = state.unmet_for_task(&task);
                    state.note_unmet_at_run_start(&task.name, &unmet);
                    unmet
                }
                None => Vec::new(),
            };
            let run_id = match self
                .store
                .start_run_with_env(&task.id, None, started_at, &unmet_env)
            {
                Ok(run_id) => run_id,
                Err(error) => {
                    tracing::warn!("squad: failed to open run row for {:?}: {error}", task.name);
                    continue;
                }
            };
            let run_log_dir = match SquadRunPaths::new(&task_dir, &run_id)
                .and_then(|paths| paths.prepare().map(|dir| dir.to_path_buf()))
            {
                Ok(dir) => dir,
                Err(error) => {
                    let detail = RunDetail {
                        error: Some(format!("preparing per-container log directory: {error}")),
                        ..Default::default()
                    };
                    let _ = self
                        .store
                        .finish_run(&run_id, RunStatus::Failed, &detail, Utc::now());
                    tracing::warn!(
                        task = %task.name,
                        run_id = %run_id,
                        error = %error,
                        "squad: failed to prepare per-run log directory"
                    );
                    continue;
                }
            };
            tracing::info!(
                task = %task.name,
                run_id = %run_id,
                log_dir = %run_log_dir.display(),
                "squad task selected for evaluation"
            );
            // WI 0110: a task may carry its own `config.json` beside its
            // workspace. It is read here, per task and per tick, and layered
            // over the global block, so an edited task config takes effect on
            // the next tick exactly as an edited global one does. A malformed
            // task file fails that task's run with a named error rather than
            // being ignored — it is scoped to one task, so one run can say what
            // is wrong with it.
            let effective = match task_squad_config(&self.paths, &task.name, &squad) {
                Ok(effective) => effective,
                Err(error) => {
                    let detail = RunDetail {
                        error: Some(format!("reading the task's config.json: {error}")),
                        ..Default::default()
                    };
                    let _ = self
                        .store
                        .finish_run(&run_id, RunStatus::Failed, &detail, Utc::now());
                    tracing::warn!(
                        task = %task.name,
                        run_id = %run_id,
                        error = %error,
                        "squad: failed to read the task's config.json"
                    );
                    continue;
                }
            };
            adjust_in_flight(&self.status, 1);
            let cancel = CancellationToken::new();
            self.status
                .lock()
                .expect("scheduler status poisoned")
                .cancellations
                .insert(
                    task.id.clone(),
                    RunCancellation {
                        run_id: run_id.clone(),
                        token: cancel.clone(),
                    },
                );

            let runtime = self.runtime.clone();
            let resolved_leaders = Arc::clone(&self.resolved_leaders);
            let store = Arc::clone(&self.store);
            let evaluator = Arc::clone(&self.evaluator);
            let status = Arc::clone(&self.status);
            let failures = Arc::clone(&self.failure_counts);
            let guidance = effective.guidance.clone();
            let agents_to_models = effective.agents_to_models.clone();
            let default_leader = effective.default_leader.clone();
            tasks.spawn(async move {
                evaluate_task(EvaluateArgs {
                    store,
                    evaluator,
                    status,
                    failures,
                    task,
                    task_dir,
                    run_log_dir,
                    run_id,
                    guidance,
                    agents_to_models,
                    default_leader,
                    cancel,
                    runtime,
                    resolved_leaders,
                })
                .await;
            });
        }
    }

    /// How many evaluations are executing right now.
    fn in_flight(&self) -> usize {
        self.status
            .lock()
            .expect("scheduler status poisoned")
            .in_flight
    }

    /// Log one line per live squad container. An empty runtime snapshot emits
    /// nothing at all — including immediately after the last agent exits — so
    /// the daemon log stays a lifecycle record rather than a heartbeat.
    fn log_running_agents(&self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let handles = match runtime
            .list_running_with_name_prefix(crate::engine::container::naming::SQUAD_NAME_PREFIX)
        {
            Ok(handles) => handles,
            Err(error) => {
                tracing::warn!(error = %error, "squad: failed to list running containers");
                return;
            }
        };
        for summary in self.running_agent_summaries(&handles, Utc::now()) {
            tracing::info!(
                task = %summary.task,
                container = %summary.container,
                agent = %summary.agent,
                model = ?summary.model,
                image = %summary.image,
                elapsed_secs = summary.elapsed_secs,
                "squad running agent"
            );
        }
    }
    /// Turn a live container snapshot into the summary lines the daemon log
    /// emits.
    ///
    /// A method rather than a free function, and it reads the leader identity
    /// Layer 2 *reported* (`RunProgress::leader_resolved`) rather than
    /// re-deriving it (F-46). Before, this parsed `defaultLeader`'s
    /// `agent::model` spelling and reached into `agentsToModels` to pick a
    /// model — a copy of Layer 2's resolution policy that could only
    /// approximate it, and a per-agent default in Layer 1.
    ///
    /// Separated from the logging so the "nothing running emits nothing / one
    /// entry per running container" rule is testable without a container
    /// runtime: an empty snapshot yields an empty vector, and the caller
    /// therefore logs nothing at all rather than a heartbeat.
    pub fn running_agent_summaries(
        &self,
        handles: &[crate::data::session::AgentHandle],
        now: DateTime<Utc>,
    ) -> Vec<RunningAgentSummary> {
        let resolved = self.resolved_leaders.lock().unwrap();
        handles
            .iter()
            .map(|handle| {
                let task_name = parse_squad_task_slug(&handle.name).unwrap_or("unknown");
                let reported = resolved.get(task_name);
                // The task's own columns are the fallback: they are the
                // request, recorded in Layer 0, not a default this layer
                // invented. A run whose leader has not reported yet — the
                // window between container launch and resolution — says so.
                let configured = self.store.get(task_name).ok().flatten();
                let agent = reported
                    .map(|r| r.agent.as_str())
                    .or_else(|| configured.as_ref().and_then(|task| task.agent.as_deref()))
                    .unwrap_or("unresolved")
                    .to_string();
                let model = reported
                    .and_then(|r| r.model.clone())
                    .or_else(|| configured.as_ref().and_then(|task| task.model.clone()));
                RunningAgentSummary {
                    task: task_name.to_string(),
                    container: handle.name.clone(),
                    agent,
                    model,
                    image: handle.image_tag.clone(),
                    elapsed_secs: now
                        .signed_duration_since(handle.started_at)
                        .num_seconds()
                        .max(0),
                }
            })
            .collect()
    }
}

/// One line of the periodic running-agents summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAgentSummary {
    pub task: String,
    pub container: String,
    pub agent: String,
    pub model: Option<String>,
    pub image: String,
    pub elapsed_secs: i64,
}

/// Bundled arguments for one task's evaluation task.
struct EvaluateArgs {
    store: Arc<TaskStore>,
    evaluator: Arc<dyn TaskEvaluator>,
    status: Arc<Mutex<SchedulerStatus>>,
    failures: Arc<Mutex<HashMap<String, u32>>>,
    task: Task,
    task_dir: std::path::PathBuf,
    run_log_dir: std::path::PathBuf,
    /// The `running` row the tick already committed for this evaluation.
    run_id: RunId,
    guidance: Option<Vec<String>>,
    agents_to_models: Option<HashMap<String, Vec<String>>>,
    default_leader: Option<String>,
    /// Fired by `squad cancel`; also registered in `status.cancellations`.
    cancel: CancellationToken,
    /// Used to stop the task's containers when the run is cancelled.
    runtime: Option<Arc<dyn AgentRuntimeEngine>>,
    /// Where the evaluator's resolved leader identity is recorded (F-46).
    resolved_leaders: Arc<Mutex<HashMap<String, ResolvedLeader>>>,
}

/// Open a run row, delegate evaluation, record the terminal status, and adjust
/// backoff. Every store failure is logged and swallowed — one task's bad
/// tick must never take down the daemon.
async fn evaluate_task(args: EvaluateArgs) {
    let EvaluateArgs {
        store,
        evaluator,
        status,
        failures,
        task,
        task_dir,
        run_log_dir,
        run_id,
        guidance,
        agents_to_models,
        default_leader,
        cancel,
        runtime,
        resolved_leaders,
    } = args;

    // The tick already incremented `in_flight`, registered the cancellation
    // handle and opened the run row; this guard undoes the first two even if
    // the evaluation panics or returns early.
    let _guard = InFlightGuard {
        status: Arc::clone(&status),
        task_id: task.id.clone(),
        run_id: run_id.clone(),
    };

    let evaluation = evaluator.evaluate(EvaluationRequest {
        task: task.clone(),
        run_id: run_id.clone(),
        task_dir,
        run_log_dir: run_log_dir.clone(),
        guidance,
        agents_to_models,
        default_leader,
        progress: Arc::new(StoreRunProgress {
            store: Arc::clone(&store),
            resolved_leaders: Arc::clone(&resolved_leaders),
        }),
    });
    // `biased`: once a cancel has been asked for it wins, even if the
    // evaluation would also be ready — stopping its containers below would
    // otherwise surface as a failed run racing the cancellation.
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        outcome = evaluation => Some(outcome),
    };
    let Some(outcome) = outcome else {
        finish_canceled_run(&store, &task, &run_id, &run_log_dir, runtime).await;
        return;
    };

    let (run_status, detail, failed) = classify(&outcome, &run_log_dir);
    let finished_at = Utc::now();
    if failed {
        // WI 0112 Part 5: a failed run is an error line that says why and
        // where to look, even when the failure happened before any step ran
        // (so no per-step line carries a `log_path`).
        tracing::error!(
            task = %task.name,
            run_id = %run_id,
            status = ?run_status,
            outcome = ?outcome_name(&outcome),
            error = detail.error.as_deref().unwrap_or("(no detail)"),
            log_dir = %run_log_dir.display(),
            "squad task run finished"
        );
    } else {
        tracing::info!(
            task = %task.name,
            run_id = %run_id,
            status = ?run_status,
            outcome = ?outcome_name(&outcome),
            "squad task run finished"
        );
    }
    if let Err(error) = store.finish_run(&run_id, run_status, &detail, finished_at) {
        tracing::warn!(
            "squad: failed to record run outcome for {:?}: {error}",
            task.name
        );
    }

    if failed {
        let attempt = {
            let mut counts = failures.lock().expect("failure counts poisoned");
            let entry = counts.entry(task.id.clone()).or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        let until = finished_at
            + chrono::Duration::seconds(backoff_secs(task.interval_secs, attempt) as i64);
        if let Err(error) = store.set_backoff(&task.name, Some(until)) {
            tracing::warn!("squad: failed to set backoff for {:?}: {error}", task.name);
        }
        tracing::info!(
            task = %task.name,
            run_id = %run_id,
            attempt,
            backoff_until = %until,
            "squad task backed off after failed run"
        );
    } else {
        // A non-failing terminal status resets the streak and clears any
        // backoff the task was carrying.
        failures
            .lock()
            .expect("failure counts poisoned")
            .remove(&task.id);
        if task.backoff_until.is_some() {
            if let Err(error) = store.set_backoff(&task.name, None) {
                tracing::warn!(
                    "squad: failed to clear backoff for {:?}: {error}",
                    task.name
                );
            }
        }
    }
}

fn outcome_name(outcome: &EvaluationOutcome) -> &'static str {
    match outcome {
        EvaluationOutcome::NotTriggered { .. } => "not_triggered",
        EvaluationOutcome::WorkflowExecuted { .. } => "workflow_executed",
        EvaluationOutcome::Failed { .. } => "failed",
    }
}

/// Map an [`EvaluationOutcome`] onto the persisted run status, its detail row,
/// and whether it counts as a failure for backoff purposes.
///
/// WI 0112 Part 6: a generated workflow that exited non-zero is a failed run.
/// That covers a step container exiting non-zero on its own, an aborting
/// setup step, any teardown failure, an `abort_on_failure` abort, or an
/// engine error — everything `exec workflow` itself reports as failure. A
/// step the yolo countdown killed never reaches here as a failure: the engine
/// marks it succeeded and moves on, so the overall exit code stays 0. The
/// workflow paths stay on the row either way, so the state file is still
/// reachable from a failed run. `run_log_dir` is named in the error text.
fn classify(
    outcome: &EvaluationOutcome,
    run_log_dir: &std::path::Path,
) -> (RunStatus, RunDetail, bool) {
    match outcome {
        EvaluationOutcome::NotTriggered { reason } => (
            RunStatus::NotTriggered,
            RunDetail {
                reason: reason.clone(),
                ..Default::default()
            },
            false,
        ),
        EvaluationOutcome::WorkflowExecuted {
            reason,
            workflow_path,
            workflow_state_path,
            exit_code,
        } => {
            let workflow_failed = matches!(exit_code, Some(code) if *code != 0);
            (
                if workflow_failed {
                    RunStatus::Failed
                } else {
                    RunStatus::WorkflowExecuted
                },
                RunDetail {
                    workflow_path: Some(workflow_path.clone()),
                    workflow_state_path: workflow_state_path.clone(),
                    error: exit_code.filter(|code| *code != 0).map(|code| {
                        format!(
                            "generated workflow exited with code {code}; see {}",
                            run_log_dir.display()
                        )
                    }),
                    reason: reason.clone(),
                },
                workflow_failed,
            )
        }
        EvaluationOutcome::Failed { error, reason } => (
            RunStatus::Failed,
            RunDetail {
                workflow_path: None,
                workflow_state_path: None,
                error: Some(error.clone()),
                reason: reason.clone(),
            },
            true,
        ),
    }
}

/// `now + min(interval * 2^attempt, 6h)`, saturating so a large attempt count
/// can never overflow.
fn backoff_secs(interval_secs: u64, attempt: u32) -> u64 {
    let factor = 2u64.checked_pow(attempt).unwrap_or(u64::MAX);
    interval_secs.saturating_mul(factor).min(MAX_BACKOFF_SECS)
}

fn adjust_in_flight(status: &Arc<Mutex<SchedulerStatus>>, delta: isize) {
    let mut status = status.lock().expect("scheduler status poisoned");
    if delta >= 0 {
        status.in_flight = status.in_flight.saturating_add(delta as usize);
    } else {
        status.in_flight = status.in_flight.saturating_sub((-delta) as usize);
    }
}

/// Record a cancelled run and stop every container still running for its task.
///
/// The evaluation future has already been dropped by the time this runs, so
/// nothing is left to start new containers; the ones already started — the
/// leader and any generated-workflow steps, all carrying the task's squad name
/// — are stopped here. A cancel is the user's decision, not a failure, so the
/// task's backoff and failure streak are left exactly as they were.
async fn finish_canceled_run(
    store: &TaskStore,
    task: &Task,
    run_id: &RunId,
    run_log_dir: &std::path::Path,
    runtime: Option<Arc<dyn AgentRuntimeEngine>>,
) {
    tracing::info!(
        task = %task.name,
        run_id = %run_id,
        status = ?RunStatus::Canceled,
        "squad task run canceled"
    );
    if let Err(error) = store.finish_run(
        run_id,
        RunStatus::Canceled,
        &RunDetail {
            // A leader that already wrote its verdict before the cancel keeps
            // its reason on the record.
            reason: crate::data::fs::RunVerdict::read_from(run_log_dir)
                .ok()
                .and_then(|verdict| verdict.reason),
            ..Default::default()
        },
        Utc::now(),
    ) {
        tracing::warn!(
            "squad: failed to record canceled run for {:?}: {error}",
            task.name
        );
    }
    let Some(runtime) = runtime else {
        return;
    };
    let task_name = task.name.clone();
    let stopped = tokio::task::spawn_blocking(move || stop_task_containers(&*runtime, &task_name))
        .await
        .unwrap_or(0);
    tracing::info!(
        task = %task.name,
        run_id = %run_id,
        stopped,
        "squad stopped the canceled run's containers"
    );
}

/// Stop every running container that belongs to `task_name`, returning how
/// many were stopped. Failures are logged and skipped so one stubborn
/// container never leaves the rest running.
///
/// Matches on the parsed task slug rather than the bare name prefix: task
/// `deploy`'s prefix is also a prefix of task `deploy-preview`'s containers.
fn stop_task_containers(runtime: &dyn AgentRuntimeEngine, task_name: &str) -> usize {
    let prefix = format!(
        "{}{task_name}-",
        crate::engine::container::naming::SQUAD_NAME_PREFIX
    );
    let handles = match runtime.list_running_with_name_prefix(&prefix) {
        Ok(handles) => handles,
        Err(error) => {
            tracing::warn!(task = %task_name, error = %error, "squad: failed to list containers to cancel");
            return 0;
        }
    };
    let mut stopped = 0;
    for handle in handles
        .iter()
        .filter(|handle| parse_squad_task_slug(&handle.name) == Some(task_name))
    {
        match runtime.stop(handle) {
            Ok(()) => stopped += 1,
            Err(error) => tracing::warn!(
                task = %task_name,
                container = %handle.name,
                error = %error,
                "squad: failed to stop a canceled run's container"
            ),
        }
    }
    stopped
}

/// Restores `in_flight` and removes the run's cancellation handle on drop, so
/// a panic or early return in an evaluation task never leaks either.
struct InFlightGuard {
    status: Arc<Mutex<SchedulerStatus>>,
    task_id: String,
    run_id: RunId,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        adjust_in_flight(&self.status, -1);
        let mut status = self.status.lock().expect("scheduler status poisoned");
        if status
            .cancellations
            .get(&self.task_id)
            .is_some_and(|cancellation| cancellation.run_id == self.run_id)
        {
            status.cancellations.remove(&self.task_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        // interval 60s: 120, 240, 480, … up to the 6h ceiling.
        assert_eq!(backoff_secs(60, 1), 120);
        assert_eq!(backoff_secs(60, 2), 240);
        assert_eq!(backoff_secs(60, 3), 480);
        assert_eq!(backoff_secs(60, 100), MAX_BACKOFF_SECS);
    }

    #[test]
    fn backoff_never_overflows() {
        assert_eq!(backoff_secs(u64::MAX, 63), MAX_BACKOFF_SECS);
        assert_eq!(backoff_secs(86_400, u32::MAX), MAX_BACKOFF_SECS);
    }

    fn executed(exit_code: Option<i32>) -> EvaluationOutcome {
        EvaluationOutcome::WorkflowExecuted {
            reason: Some("new issues".to_string()),
            workflow_path: "/c/workflow.toml".into(),
            workflow_state_path: Some("/state.json".into()),
            exit_code,
        }
    }

    #[test]
    fn classify_maps_each_outcome() {
        let log_dir = std::path::Path::new("/runs/r1");
        let (status, detail, failed) = classify(
            &EvaluationOutcome::NotTriggered {
                reason: Some("nothing new".to_string()),
            },
            log_dir,
        );
        assert_eq!(status, RunStatus::NotTriggered);
        assert!(!failed);
        assert!(detail.error.is_none());
        assert_eq!(detail.reason.as_deref(), Some("nothing new"));

        let (status, detail, failed) = classify(&executed(Some(0)), log_dir);
        assert_eq!(status, RunStatus::WorkflowExecuted);
        assert!(!failed);
        assert!(detail.error.is_none());
        assert_eq!(detail.reason.as_deref(), Some("new issues"));
        assert_eq!(
            detail.workflow_path.as_deref(),
            Some("/c/workflow.toml".as_ref())
        );
        assert_eq!(
            detail.workflow_state_path.as_deref(),
            Some("/state.json".as_ref())
        );

        let (status, detail, failed) = classify(
            &EvaluationOutcome::Failed {
                error: "boom".to_string(),
                reason: Some("3 new issues".to_string()),
            },
            log_dir,
        );
        assert_eq!(status, RunStatus::Failed);
        assert!(failed);
        assert_eq!(detail.error.as_deref(), Some("boom"));
        assert_eq!(detail.reason.as_deref(), Some("3 new issues"));
    }

    /// WI 0112 Part 6: a generated workflow that exited non-zero is a failed
    /// run that backs the task off, and its paths stay on the row.
    #[test]
    fn classify_turns_a_non_zero_workflow_exit_into_a_failed_run() {
        let log_dir = std::path::Path::new("/runs/r1");
        for code in [1, 2, 137] {
            let (status, detail, failed) = classify(&executed(Some(code)), log_dir);
            assert_eq!(status, RunStatus::Failed, "exit {code}");
            assert!(failed, "exit {code} backs off");
            assert_eq!(
                detail.error.as_deref(),
                Some(format!("generated workflow exited with code {code}; see /runs/r1").as_str())
            );
            assert_eq!(
                detail.workflow_path.as_deref(),
                Some("/c/workflow.toml".as_ref()),
                "the workflow path survives a failed classification"
            );
            assert_eq!(
                detail.workflow_state_path.as_deref(),
                Some("/state.json".as_ref())
            );
        }
    }

    /// A paused workflow (no exit code) cannot happen unattended, but the
    /// mapping must not call it a failure if it ever did.
    #[test]
    fn classify_treats_a_workflow_without_an_exit_code_as_executed() {
        let (status, detail, failed) = classify(&executed(None), std::path::Path::new("/r"));
        assert_eq!(status, RunStatus::WorkflowExecuted);
        assert!(!failed);
        assert!(detail.error.is_none());
    }

    // ── The periodic running-agents summary (WI 0106 §3b) ─────────────────

    fn summary_store(tmp: &std::path::Path) -> TaskStore {
        let db = crate::data::fs::DataPaths::at_root(tmp.join("data")).db_path();
        let store = TaskStore::open(&db).unwrap();
        store.migrate().unwrap();
        store
    }

    /// A scheduler over `store` with no runtime — enough to exercise the
    /// running-agent summary, which only reads the store and the reported
    /// leader identities.
    fn summary_scheduler(store: Arc<TaskStore>, tmp: &std::path::Path) -> SquadScheduler {
        SquadScheduler::new(
            store,
            SquadPaths::from_root(tmp.join("squad")),
            Arc::new(NoopEvaluator),
            EnvSnapshot::empty(),
        )
    }

    /// An evaluator that is never invoked by the summary tests.
    struct NoopEvaluator;

    #[async_trait::async_trait]
    impl TaskEvaluator for NoopEvaluator {
        async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
            EvaluationOutcome::NotTriggered { reason: None }
        }
    }

    fn handle(
        name: &str,
        image: &str,
        started_at: DateTime<Utc>,
    ) -> crate::data::session::AgentHandle {
        crate::data::session::AgentHandle {
            id: format!("id-{name}"),
            image_tag: image.to_string(),
            name: name.to_string(),
            started_at,
        }
    }

    /// Nothing running means nothing logged — including on the tick right
    /// after the last container exits, since the snapshot is queried live and
    /// an empty snapshot produces no summary lines at all.
    #[test]
    fn the_running_agents_summary_is_empty_when_nothing_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let scheduler = summary_scheduler(Arc::new(summary_store(tmp.path())), tmp.path());
        assert!(
            scheduler
                .running_agent_summaries(&[], Utc::now())
                .is_empty(),
            "an empty container snapshot must produce no summary lines"
        );
    }

    /// One entry per running container, carrying the task, container name,
    /// agent/model, and elapsed running time.
    #[test]
    fn the_running_agents_summary_reports_one_entry_per_running_container() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(summary_store(tmp.path()));
        let now = Utc::now();
        let mut configured = Task {
            id: "id-triage".into(),
            name: "issue-triage".into(),
            description: "d".into(),
            repo_scope: tmp.path().to_path_buf(),
            mount_scope: crate::data::fs::MountScope::Directory,
            overlays: Vec::new(),
            interval_secs: 21_600,
            status: crate::data::fs::TaskStatus::Active,
            agent: Some("codex".into()),
            model: Some("gpt-5".into()),
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        };
        store.create(&configured).unwrap();
        configured.id = "id-nightly".into();
        configured.name = "nightly-sweep".into();
        configured.agent = None;
        configured.model = None;
        store.create(&configured).unwrap();

        let handles = [
            handle(
                "awman-squad-issue-triage-0123abcd",
                "awman-ws-codex:latest",
                now - chrono::Duration::seconds(90),
            ),
            handle(
                "awman-squad-nightly-sweep-89abcdef",
                "awman-ws-claude:latest",
                now - chrono::Duration::seconds(5),
            ),
        ];

        let scheduler = summary_scheduler(Arc::clone(&store), tmp.path());
        // The leader resolution Layer 2 reports for the second task. The
        // first task's own `agent`/`model` columns still speak for it (F-46:
        // the fallback is the recorded request, not a re-derived default).
        scheduler.resolved_leaders.lock().unwrap().insert(
            "nightly-sweep".to_string(),
            ResolvedLeader {
                agent: "claude".into(),
                model: Some("opus".into()),
            },
        );

        let summaries = scheduler.running_agent_summaries(&handles, now);

        assert_eq!(summaries.len(), 2, "one entry per running container");
        assert_eq!(
            summaries[0],
            RunningAgentSummary {
                task: "issue-triage".into(),
                container: "awman-squad-issue-triage-0123abcd".into(),
                agent: "codex".into(),
                model: Some("gpt-5".into()),
                image: "awman-ws-codex:latest".into(),
                elapsed_secs: 90,
            }
        );
        // The second task configures neither agent nor model, so the summary
        // reports what Layer 2 resolved for it.
        assert_eq!(summaries[1].task, "nightly-sweep");
        assert_eq!(summaries[1].agent, "claude");
        assert_eq!(summaries[1].model.as_deref(), Some("opus"));
        assert_eq!(summaries[1].elapsed_secs, 5);
    }
}
