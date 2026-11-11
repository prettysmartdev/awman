//! Launching a task's leader agent in a container (Layer 1).
//!
//! [`SquadAgentLauncher`] is the engine-side counterpart to
//! `ExecWorkflowCommand`'s leader drive: it seeds the persistent task
//! directory with the dynamic-workflow reference assets, resolves the agent's
//! container options, stamps squad's identity (container name + the two labels)
//! onto them, and runs the agent through `Arc<dyn AgentRuntimeEngine>`.
//!
//! It deliberately does *not* build the [`AgentRunOptions`] or resolve the
//! leader prompt — those need Layer-2 config resolution and are handed in via
//! [`LeaderRunSpec`]. What is genuinely Layer 1 lives here: filesystem seeding,
//! option resolution, container naming, label attachment, and the
//! build → run → wait cycle.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::data::dynamic_workflow_assets::WORKFLOW_USAGE_MD;
use crate::data::program_status::EffectiveStatus;
use crate::data::session::{AgentName, Session};
use crate::data::RepoDockerfilePaths;
use crate::engine::agent::{AgentEngine, AgentRunOptions};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentInstance, StuckEvent, KILLED_EXIT_CODE,
};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::auth::AgentCredentials;
use crate::engine::container::naming::{generate_squad_container_name, validate_task_slug};
use crate::engine::container::options::ContainerName;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::CountdownKind;
use crate::engine::workflow::program_status::{
    describe_status, drain_pending_reports, final_tracker_status, respond_to_report,
    ReportedResponse,
};
use crate::engine::workflow::timing::{agent_done_countdown_duration, yolo_countdown_duration};

/// The label key carrying the squad evaluation session id — the same key an
/// interactive session uses, so `docker ps` inspection is uniform.
pub const SESSION_LABEL_KEY: &str = "awman.session";
/// The label key carrying the task name, so every container a task's
/// evaluation (and generated workflow) launches is attributable to it.
pub const TASK_LABEL_KEY: &str = "awman.squad.task";

// `run_log_dir` and `prepare_run_log_dir` are now `data::fs::SquadRunPaths`
// (F-38): they were pure path arithmetic plus a `create_dir_all`, which is
// Layer 0 work, and leaving them here kept the launcher's surface free
// functions rather than methods.

/// The container-name and label identity every container a task launches
/// must carry. One implementation, used by the evaluation leader
/// ([`SquadAgentLauncher::run_leader`]) and by every step of the workflow that
/// leader generates (`ExecWorkflowCommand`, via `with_squad_identity`), so a
/// task's whole container set shares one discoverable name prefix and the
/// two attribution labels.
#[derive(Debug, Clone)]
pub struct SquadContainerIdentity {
    pub task_name: String,
    pub session_id: String,
}

impl SquadContainerIdentity {
    pub fn new(task_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            task_name: task_name.into(),
            session_id: session_id.into(),
        }
    }

    /// Stamp a fresh `awman-squad-<slug>-<8 hex>` name and the `awman.session` /
    /// `awman.squad.task` labels onto resolved options. Rejects the sandbox
    /// variant, exactly as `run_leader` did before this was extracted: squad is
    /// container-only, so a mis-wired caller fails loudly rather than launching
    /// an unlabelled, undiscoverable container.
    pub fn stamp(
        &self,
        resolved: ResolvedAgentOptions,
    ) -> Result<ResolvedAgentOptions, EngineError> {
        match resolved {
            ResolvedAgentOptions::Container(mut options) => {
                let container_name = generate_squad_container_name(&self.task_name);
                options.name = Some(ContainerName::new(container_name));
                options
                    .labels
                    .push((SESSION_LABEL_KEY.to_string(), self.session_id.clone()));
                options
                    .labels
                    .push((TASK_LABEL_KEY.to_string(), self.task_name.clone()));
                Ok(ResolvedAgentOptions::Container(options))
            }
            ResolvedAgentOptions::Sandbox(_) => Err(EngineError::Config(
                "squad requires a container runtime; the sandbox tier cannot host \
                 task evaluation"
                    .to_string(),
            )),
        }
    }
}

/// Everything the launcher needs to run one task's leader agent.
///
/// The caller (Layer 2) has already opened `session` rooted at the repo's
/// captured `mount_scope` and resolved `run_options` — including the task
/// directory's context overlay and the assembled leader prompt. The launcher
/// mounts nothing beyond what those two imply: the repo (via the session) and
/// the task directory (via the run's context overlay), never a parent.
pub struct LeaderRunSpec {
    /// Session rooted at the repo mount scope captured when the task was
    /// created. The repo is mounted from `session.git_root()`; the label's
    /// session id is `session.id()`.
    pub session: Session,
    /// The leader agent to run.
    pub agent: AgentName,
    /// Fully-resolved run options (prompt, model, non-interactive, yolo,
    /// task-directory context overlay, image tag override).
    pub run_options: AgentRunOptions,
    /// Resolved credential delivery. File delivery is staged by `AgentEngine`;
    /// env values are never written into the persistent task directory.
    pub credentials: AgentCredentials,
    /// The task name; used both for the container name slug and for the
    /// `awman.squad.task` label.
    pub task_name: String,
    /// The task's persistent context directory, seeded before launch.
    pub task_dir: std::path::PathBuf,
}

/// Runs a task's leader agent in a container.
pub struct SquadAgentLauncher {
    agent_engine: Arc<AgentEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl SquadAgentLauncher {
    pub fn new(agent_engine: Arc<AgentEngine>, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self {
            agent_engine,
            runtime,
        }
    }

    /// Scaffold a plain-directory task root so agent images can be resolved from
    /// it, exactly the way any other awman project root resolves them.
    ///
    /// A repository-backed task inherits its project's `Dockerfile.dev` and
    /// `.awman/Dockerfile.<agent>`, so `ensure_agent_image` has something to build
    /// from. A [`MountScope::Directory`](crate::data::fs::task_store::MountScope)
    /// task's root is a plain directory — the durable task workspace, or a custom
    /// folder that is not a repository — and starts with neither, so without this
    /// the very first evaluation of a default-workspace task fails before the
    /// leader is ever launched.
    ///
    /// The project Dockerfile is create-if-missing. Agent Dockerfiles use the
    /// shared sync rule: custom generated files track approved definitions;
    /// hand-maintained files survive. Unknown or unapproved agents fail here.
    pub async fn ensure_directory_workspace_project(
        root: &Path,
        agents: &[String],
    ) -> Result<(), EngineError> {
        Self::ensure_directory_workspace_project_with_env(
            &crate::data::config::env::Env::from_process(),
            root,
            agents,
        )
        .await
        .map(|_| ())
    }

    pub async fn ensure_directory_workspace_project_with_env(
        env: &crate::data::config::env::EnvSnapshot,
        root: &Path,
        agents: &[String],
    ) -> Result<Vec<String>, EngineError> {
        std::fs::create_dir_all(root).map_err(|e| EngineError::io(root, e))?;
        let paths = RepoDockerfilePaths::new(root);
        let mut definitions = Vec::new();
        for agent in agents {
            let name = AgentName::new(agent)?;
            definitions.push(crate::engine::agent::AgentRegistry::resolve(
                env,
                &name,
                Some(root),
            )?);
        }

        let project = paths.project_dockerfile();
        if !project.exists() {
            std::fs::write(&project, crate::data::templates::project_dockerfile_dev())
                .map_err(|e| EngineError::io(&project, e))?;
        }

        let base_tag = crate::data::image_tags::project_image_tag(root);
        let mut warnings = Vec::new();
        for def in definitions {
            let sync =
                crate::engine::agent::provision_agent_dockerfile(&def, &paths, &base_tag).await?;
            warnings.extend(sync.warnings);
        }
        Ok(warnings)
    }

    /// Drive a launched unattended agent to completion with the same stuck →
    /// yolo countdown → auto-advance machinery a dynamic workflow uses (identical
    /// [`StuckEvent`] semantics and the same [`yolo_countdown_duration`]), minus
    /// the interactive control board — squad has no operator to consult.
    ///
    /// The agent's stuck detector (in the container I/O bridge) publishes `Stuck`
    /// after the PTY goes quiet. That starts a countdown; fresh output cancels it
    /// (`Unstuck`), and expiry kills the container and returns — the unattended
    /// equivalent of the dynamic path's yolo auto-advance.
    ///
    /// An agent that reports its own status over OSC 7501 (WI 0141) is judged
    /// on its reports instead — the detector never sends it `Stuck` — with the
    /// workflow engine's "no user" column
    /// ([`respond_to_report`](crate::engine::workflow::program_status::respond_to_report)):
    /// `done`/`idle` starts the short `AgentDone` countdown whose expiry is a
    /// normal auto-advance; `blocked` starts an `AgentBlocked` countdown
    /// whose expiry kills the agent as a **failure**
    /// (`killed_by_countdown == false`, [`UnattendedExit::reported_problem`]
    /// set). An `error` report stays live until the agent exits or reports
    /// recovery; `working` cancels whichever countdown is running.
    ///
    /// Logging is lifecycle-only: countdown started / cancelled / auto-advanced.
    /// No per-tick messages ever reach the daemon log.
    ///
    /// The result says whether the countdown did the killing (WI 0112 Part 6):
    /// that is the one exit a caller must not count as a failed run, and it is
    /// not recoverable from the exit code alone — a 137 the container produced on
    /// its own (an OOM kill, an external `docker kill`) looks identical.
    pub async fn drive_unattended_agent(
        execution: AgentExecution,
        task: &str,
        label: &str,
    ) -> Result<UnattendedExit, EngineError> {
        drive_unattended_agent_with(execution, task, label, CountdownDurations::current()).await
    }

    /// Seed, launch, and await the leader agent, returning its exit, the
    /// container name it ran as, and whether the yolo countdown killed it
    /// (WI 0112 Part 6: a countdown kill is never a failed run).
    ///
    /// Seeding is idempotent: the task directory is created once and never
    /// recreated per run (`context(global)` semantics). Leader-written files,
    /// including a prior `workflow.toml`, are preserved; only the static
    /// reference assets are rewritten before each launch.
    pub async fn run_leader(
        &self,
        spec: LeaderRunSpec,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<LeaderExit, EngineError> {
        // `naming.rs` documents slug validation as the caller's responsibility.
        // Re-check it here so this primitive is self-defending: today the only
        // insertion path validates at task creation, but a future second
        // one must not be able to defeat the container-name guarantee.
        validate_task_slug(&spec.task_name)
            .map_err(|error| EngineError::Config(error.to_string()))?;

        Self::seed_task_dir(&spec.task_dir)?;

        let resolved = self.agent_engine.resolve_agent_options(
            &spec.session,
            &spec.agent,
            &spec.run_options,
            &spec.credentials,
        )?;

        // Stamp squad's identity onto the resolved options: a deterministic,
        // parseable container name and the two attribution labels. The same
        // identity is applied to every generated-workflow step container, so it
        // lives in exactly one place — [`SquadContainerIdentity::stamp`].
        let identity =
            SquadContainerIdentity::new(spec.task_name.clone(), spec.session.id().to_string());
        let resolved = identity.stamp(resolved)?;

        let instance: Box<dyn AgentInstance> = self.runtime.build(resolved)?;
        let execution = instance.run_with_frontend(frontend)?;
        let container_name = execution.handle().name.clone();
        // The leader runs interactively (PTY) under yolo, so its agent TUI
        // stays open after finishing its work rather than exiting. Drive it
        // through the same stuck → yolo countdown → auto-advance machinery a
        // dynamic workflow's leader uses, or the evaluation would wait on an
        // idle agent forever.
        let UnattendedExit {
            exit,
            killed_by_countdown,
            ..
        } = Self::drive_unattended_agent(execution, &spec.task_name, "leader").await?;
        Ok(LeaderExit {
            exit,
            container_name,
            killed_by_countdown,
        })
    }

    /// Write the dynamic-workflow reference assets into the durable task
    /// workspace.
    ///
    /// **This must never delete or truncate anything the leader or its
    /// workflow wrote.** The task workspace is durable for the task's whole
    /// lifetime (WI 0106 §6a): it is created once and only removed when the
    /// task itself is removed. Before WI 0106 this function deleted
    /// `workflow.toml` before every run, because a leftover file would
    /// otherwise be misread as "this run triggered". That inference is gone —
    /// the leader now reports its verdict explicitly through the per-run
    /// [`verdict`](crate::engine::squad::verdict) file — so the deletion is
    /// gone with it, and the leader is free to reuse a `workflow.toml` it
    /// wrote on an earlier run.
    ///
    /// What remains is idempotently rewriting the *static* workflow reference.
    /// It is documentation shipped with the binary, not task state, so
    /// rewriting it cannot lose anything.
    fn seed_task_dir(dir: &Path) -> Result<(), EngineError> {
        std::fs::create_dir_all(dir).map_err(|e| EngineError::io(dir, e))?;
        let usage = dir.join("workflow-usage.md");
        std::fs::write(&usage, WORKFLOW_USAGE_MD).map_err(|e| EngineError::io(&usage, e))?;
        Ok(())
    }
}

/// How an unattended agent ended: its exit, and whether the yolo countdown
/// was what ended it.
#[derive(Debug, Clone)]
pub struct UnattendedExit {
    /// The container's exit. When the agent reported a problem and then
    /// exited by itself with code 0, the code is rewritten to 1: a reported
    /// `blocked`/`error` outranks the exit code (WI 0141), and callers decide
    /// failure from the code.
    pub exit: AgentExitInfo,
    /// `true` only when [`SquadAgentLauncher::drive_unattended_agent`]'s
    /// countdown expired and
    /// killed the container. An agent that exited on its own — with any
    /// code, 137 included — is `false`. Also `false` when an `AgentBlocked`
    /// or `AgentError` countdown killed it: that is a failure, not an
    /// auto-advance.
    pub killed_by_countdown: bool,
    /// The agent's last reported problem (`blocked`/`error`) when the run
    /// ended on one; `None` otherwise. Its `msg` is untrusted agent text.
    pub reported_problem: Option<EffectiveStatus>,
}

impl UnattendedExit {
    /// An exit the container produced on its own. A pending reported
    /// problem makes it a failure even with code 0.
    fn own_exit(mut exit: AgentExitInfo, reported_problem: Option<EffectiveStatus>) -> Self {
        if reported_problem.is_some() && exit.exit_code == 0 {
            exit.exit_code = 1;
        }
        Self {
            exit,
            killed_by_countdown: false,
            reported_problem,
        }
    }
}

/// What [`SquadAgentLauncher::run_leader`] returns: the leader's exit, the
/// container it ran as (so the evaluator can name the run log it wrote), and
/// whether the countdown killed it.
#[derive(Debug, Clone)]
pub struct LeaderExit {
    pub exit: AgentExitInfo,
    pub container_name: String,
    pub killed_by_countdown: bool,
}

/// The countdown lengths [`drive_unattended_agent_with`] uses; tests shorten
/// them.
#[derive(Debug, Clone, Copy)]
struct CountdownDurations {
    /// Silence (`StuckStep`) and problem (`AgentBlocked`/`AgentError`).
    yolo: Duration,
    /// `AgentDone`.
    agent_done: Duration,
}

impl CountdownDurations {
    fn current() -> Self {
        Self {
            yolo: yolo_countdown_duration(),
            agent_done: agent_done_countdown_duration(),
        }
    }

    fn of(self, kind: CountdownKind) -> Duration {
        match kind {
            CountdownKind::AgentDone => self.agent_done,
            _ => self.yolo,
        }
    }
}

/// [`SquadAgentLauncher::drive_unattended_agent`] with explicit countdown
/// lengths.
async fn drive_unattended_agent_with(
    mut execution: AgentExecution,
    task: &str,
    label: &str,
    durations: CountdownDurations,
) -> Result<UnattendedExit, EngineError> {
    let cancel = execution.cancel_handle();
    let mut stuck_rx = execution.subscribe_stuck();
    let tracker = execution.program_status_tracker();
    let (wait_tx, mut wait_rx) =
        tokio::sync::oneshot::channel::<Result<AgentExitInfo, EngineError>>();
    tokio::spawn(async move {
        let result = execution.wait().await;
        let _ = wait_tx.send(result);
    });

    // The last problem the agent reported, until `working`/`done`/a clear.
    let mut problem: Option<EffectiveStatus> = None;
    // A report a countdown handed back for a fresh decision.
    let mut pending: Option<Option<EffectiveStatus>> = None;

    loop {
        let kind = if let Some(status) = pending.take() {
            match unattended_response(status.as_ref(), &mut problem) {
                Some(kind) => kind,
                None => continue,
            }
        } else {
            tokio::select! {
                biased;
                result = &mut wait_rx => {
                    // A report ready together with the exit still counts.
                    settle_problem(&mut problem, &mut stuck_rx, tracker.as_ref());
                    return result
                        .map_err(|_| EngineError::Other("agent wait task dropped unexpectedly".into()))?
                        .map(|exit| UnattendedExit::own_exit(exit, problem));
                }
                ev = stuck_rx.recv() => match ev {
                    Ok(StuckEvent::Stuck) => CountdownKind::StuckStep,
                    Ok(StuckEvent::Reported(status)) => {
                        match unattended_response(status.as_ref(), &mut problem) {
                            Some(kind) => kind,
                            None => continue,
                        }
                    }
                    Ok(StuckEvent::Unstuck) => continue,
                    Ok(StuckEvent::StartupGraceExpired) => {
                        // The bridge already killed the container; the wait branch
                        // above resolves with its exit on the next loop pass.
                        tracing::warn!(
                            task,
                            agent = label,
                            "squad agent produced no output before its startup grace expired; \
                             container was killed",
                        );
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // No more stuck events can arrive — just wait for exit.
                        let result = (&mut wait_rx).await;
                        settle_problem(&mut problem, &mut stuck_rx, tracker.as_ref());
                        return result
                            .map_err(|_| EngineError::Other("agent wait task dropped unexpectedly".into()))?
                            .map(|exit| UnattendedExit::own_exit(exit, problem));
                    }
                }
            }
        };

        let duration = durations.of(kind);
        log_countdown_started(task, label, kind, duration, problem.as_ref());
        match run_unattended_countdown(
            task,
            label,
            kind,
            duration,
            &mut wait_rx,
            &mut stuck_rx,
            &mut problem,
        )
        .await
        {
            CountdownOutcome::Exited(result) => {
                settle_problem(&mut problem, &mut stuck_rx, tracker.as_ref());
                return result.map(|exit| UnattendedExit::own_exit(exit, problem));
            }
            // Output cancels an `AgentError` countdown only: the error stays
            // recorded until a new report, so an exit without one fails.
            CountdownOutcome::Recovered => {}
            CountdownOutcome::Superseded(status) => pending = Some(status),
            CountdownOutcome::Expired => {
                let fails = matches!(
                    kind,
                    CountdownKind::AgentBlocked | CountdownKind::AgentError
                );
                if fails {
                    tracing::warn!(
                        task,
                        agent = label,
                        "squad countdown expired for task {task:?} ({label}) on a reported \
                         problem; stopping the agent as a failure",
                    );
                } else {
                    tracing::info!(
                        task,
                        agent = label,
                        "squad yolo countdown expired for task {task:?} ({label}); \
                         auto-advancing past the idle agent",
                    );
                }
                if let Some(cancel) = &cancel {
                    let _ = cancel.cancel();
                }
                // The kill makes the real wait resolve; fall back to
                // a synthetic killed exit if the backend errors.
                let now = chrono::Utc::now();
                let exit =
                    (&mut wait_rx)
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(AgentExitInfo {
                            exit_code: KILLED_EXIT_CODE,
                            signal: None,
                            started_at: now,
                            ended_at: now,
                        });
                return Ok(if fails {
                    settle_problem(&mut problem, &mut stuck_rx, tracker.as_ref());
                    UnattendedExit::own_exit(exit, problem)
                } else {
                    UnattendedExit {
                        exit,
                        killed_by_countdown: true,
                        reported_problem: None,
                    }
                });
            }
        }
    }
}

/// Settle the problem as the agent exits: reports still queued (the exit
/// won the biased select), then the execution's tracker, which also holds a
/// report the detector had not published yet.
fn settle_problem(
    problem: &mut Option<EffectiveStatus>,
    stuck_rx: &mut tokio::sync::broadcast::Receiver<StuckEvent>,
    tracker: Option<&crate::engine::agent_runtime::program_status::SharedProgramStatusTracker>,
) {
    let mut record = |status: Option<EffectiveStatus>| {
        *problem = status.filter(|s| s.state.is_problem());
    };
    for status in drain_pending_reports(stuck_rx) {
        record(status);
    }
    if let Some(status) = tracker.and_then(final_tracker_status) {
        record(status);
    }
}

/// The unattended ("no user") response to a report: the countdown to start,
/// or `None` to keep waiting. Records or clears `problem` as it goes.
fn unattended_response(
    status: Option<&EffectiveStatus>,
    problem: &mut Option<EffectiveStatus>,
) -> Option<CountdownKind> {
    *problem = status.filter(|s| s.state.is_problem()).cloned();
    match respond_to_report(status, true, false) {
        ReportedResponse::Countdown(kind) => Some(kind),
        // Unattended never yields a board or a needs-you pause.
        _ => None,
    }
}

fn log_countdown_started(
    task: &str,
    label: &str,
    kind: CountdownKind,
    duration: Duration,
    problem: Option<&EffectiveStatus>,
) {
    let secs = duration.as_secs();
    match kind {
        CountdownKind::AgentDone => tracing::info!(
            task,
            agent = label,
            "squad agent reported done for task {task:?} ({label}); advancing in {secs}s",
        ),
        CountdownKind::AgentBlocked | CountdownKind::AgentError => {
            let what = problem.map(describe_status).unwrap_or_default();
            tracing::warn!(
                task,
                agent = label,
                "squad agent reported {what} for task {task:?} ({label}); failing in {secs}s",
            );
        }
        CountdownKind::StuckStep | CountdownKind::FailureRetry => tracing::info!(
            task,
            agent = label,
            "squad yolo countdown started for task {task:?} ({label}): no output — \
             advancing automatically in {secs}s",
        ),
    }
}

/// How one countdown window ended.
enum CountdownOutcome {
    /// The countdown ran out with the agent still idle — auto-advance (or,
    /// for a problem countdown, fail).
    Expired,
    /// The agent produced output again (or the bridge already killed it);
    /// cancel the countdown and keep waiting.
    Recovered,
    /// A report that calls for a different response; decide it afresh.
    Superseded(Option<EffectiveStatus>),
    /// The agent exited on its own mid-countdown — this IS the run's exit.
    Exited(Result<AgentExitInfo, EngineError>),
}

/// One countdown window of `duration`. A completed agent always wins over a
/// same-instant expiry (`biased` puts the wait branch first), matching the
/// dynamic leader countdown's ordering. Output (`Unstuck`) recovers a silence
/// or `AgentError` countdown; `done`/`blocked` ones end only on a report.
async fn run_unattended_countdown(
    task: &str,
    label: &str,
    kind: CountdownKind,
    duration: Duration,
    wait_rx: &mut tokio::sync::oneshot::Receiver<Result<AgentExitInfo, EngineError>>,
    stuck_rx: &mut tokio::sync::broadcast::Receiver<StuckEvent>,
    problem: &mut Option<EffectiveStatus>,
) -> CountdownOutcome {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            biased;
            result = &mut *wait_rx => {
                return CountdownOutcome::Exited(result.unwrap_or_else(|_| {
                    Err(EngineError::Other("agent wait task dropped unexpectedly".into()))
                }));
            }
            ev = stuck_rx.recv() => match ev {
                Ok(StuckEvent::Reported(status)) => {
                    if respond_to_report(status.as_ref(), true, false)
                        == ReportedResponse::Countdown(kind)
                    {
                        // Same countdown, deadline kept; the new kind or
                        // message is still the one a failure quotes.
                        *problem = status.filter(|s| s.state.is_problem());
                    } else {
                        tracing::info!(
                            task,
                            agent = label,
                            "squad agent reported a new status; countdown cancelled",
                        );
                        return CountdownOutcome::Superseded(status);
                    }
                }
                Ok(StuckEvent::Unstuck)
                    if matches!(kind, CountdownKind::StuckStep | CountdownKind::AgentError) =>
                {
                    tracing::info!(
                        task,
                        agent = label,
                        "squad agent recovered; yolo countdown cancelled",
                    );
                    return CountdownOutcome::Recovered;
                }
                Ok(StuckEvent::Unstuck) | Ok(StuckEvent::Stuck) => {}
                Ok(StuckEvent::StartupGraceExpired) => return CountdownOutcome::Recovered,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return CountdownOutcome::Expired;
                }
            },
            _ = tokio::time::sleep_until(deadline) => return CountdownOutcome::Expired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent that exits on its own is returned as-is — the auto-advance
    /// machinery only ever fires for a stuck-but-alive agent.
    #[tokio::test]
    async fn drive_unattended_agent_returns_a_self_exiting_agents_exit() {
        let now = chrono::Utc::now();
        let handle = crate::data::session::AgentHandle {
            id: "id".into(),
            image_tag: "img".into(),
            name: "awman-squad-t-00000000".into(),
            started_at: now,
        };
        let info = AgentExitInfo {
            exit_code: 3,
            signal: None,
            started_at: now,
            ended_at: now,
        };
        let execution = AgentExecution::finished(handle, info);
        let exit = SquadAgentLauncher::drive_unattended_agent(execution, "t", "leader")
            .await
            .unwrap();
        assert_eq!(exit.exit.exit_code, 3);
        assert!(
            !exit.killed_by_countdown,
            "an agent that exited on its own was not killed by the countdown"
        );
    }

    /// WI 0112 Part 6: a self-produced 137 is *not* a countdown kill. Only the
    /// `Expired` arm of the countdown sets the flag.
    #[tokio::test]
    async fn a_self_produced_137_is_not_reported_as_a_countdown_kill() {
        let now = chrono::Utc::now();
        let handle = crate::data::session::AgentHandle {
            id: "id".into(),
            image_tag: "img".into(),
            name: "awman-squad-t-00000000".into(),
            started_at: now,
        };
        let info = AgentExitInfo {
            exit_code: KILLED_EXIT_CODE,
            signal: Some(9),
            started_at: now,
            ended_at: now,
        };
        let exit = SquadAgentLauncher::drive_unattended_agent(
            AgentExecution::finished(handle, info),
            "t",
            "leader",
        )
        .await
        .unwrap();
        assert_eq!(exit.exit.exit_code, KILLED_EXIT_CODE);
        assert!(!exit.killed_by_countdown);
    }

    #[test]
    fn seed_writes_reference_assets_and_preserves_leader_written_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tasks").join("issue-triage");
        // The durable workspace survives between runs: a `workflow.toml` (and
        // anything else) the leader wrote on an earlier run must still be
        // there afterwards. The leader is explicitly allowed to reuse it.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workflow.toml"), "from a previous run").unwrap();
        std::fs::write(dir.join("leader-notes.md"), "durable state").unwrap();

        SquadAgentLauncher::seed_task_dir(&dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("workflow.toml")).unwrap(),
            "from a previous run",
            "seeding must never delete or rewrite the leader's workflow.toml"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("leader-notes.md")).unwrap(),
            "durable state",
            "seeding must never touch any other leader-written file"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("workflow-usage.md")).unwrap(),
            WORKFLOW_USAGE_MD
        );
        assert!(!dir.join("example-workflow.toml").exists());
    }

    #[test]
    fn seed_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cmd_4a8a08f09d37b73795649038408b5f33");
        SquadAgentLauncher::seed_task_dir(&dir).unwrap();
        // A second seed over an existing directory must not error.
        SquadAgentLauncher::seed_task_dir(&dir).unwrap();
        assert!(dir.join("workflow-usage.md").exists());
    }

    // ── BLOCKER-1: one stamping implementation for every task container ──

    use crate::engine::container::naming::parse_squad_task_slug;

    #[test]
    fn stamp_applies_a_parseable_squad_name_and_both_labels() {
        let identity = SquadContainerIdentity::new("issue-triage", "sess-123");
        let resolved = ResolvedAgentOptions::container(Vec::new()).unwrap();
        let ResolvedAgentOptions::Container(options) = identity.stamp(resolved).unwrap() else {
            panic!("stamp must keep the container variant");
        };
        let name = options.name.expect("stamp must set a container name");
        let name = name.as_str();
        assert!(
            name.starts_with("awman-squad-issue-triage-"),
            "unexpected container name: {name}"
        );
        assert_eq!(
            parse_squad_task_slug(name),
            Some("issue-triage"),
            "the squad name must round-trip back to the task slug"
        );
        assert!(options
            .labels
            .iter()
            .any(|(k, v)| k == SESSION_LABEL_KEY && v == "sess-123"));
        assert!(options
            .labels
            .iter()
            .any(|(k, v)| k == TASK_LABEL_KEY && v == "issue-triage"));
    }

    #[test]
    fn stamp_generates_a_fresh_name_each_call() {
        // Every generated-workflow step gets its own container, so two stamps
        // of the same identity must not collide on the unique suffix.
        let identity =
            SquadContainerIdentity::new("release-notes", "ses_03c7c0ace395d80182db07ae2c30f034");
        let first = match identity
            .stamp(ResolvedAgentOptions::container(Vec::new()).unwrap())
            .unwrap()
        {
            ResolvedAgentOptions::Container(o) => o.name.unwrap().as_str().to_string(),
            _ => unreachable!(),
        };
        let second = match identity
            .stamp(ResolvedAgentOptions::container(Vec::new()).unwrap())
            .unwrap()
        {
            ResolvedAgentOptions::Container(o) => o.name.unwrap().as_str().to_string(),
            _ => unreachable!(),
        };
        assert_ne!(first, second, "each stamp must mint a fresh unique suffix");
    }

    #[test]
    fn stamp_rejects_the_sandbox_variant() {
        let identity =
            SquadContainerIdentity::new("issue-triage", "ses_03c7c0ace395d80182db07ae2c30f034");
        let sandbox = ResolvedAgentOptions::sandbox(Vec::new());
        let error = identity.stamp(sandbox).unwrap_err();
        assert!(
            error.to_string().contains("container runtime"),
            "sandbox refusal must explain the container requirement: {error}"
        );
    }

    // ── Directory-workspace image sources (WI 0106 remediation, B1) ────────

    /// A plain directory task root starts with no `Dockerfile.dev` and no
    /// `.awman/Dockerfile.<agent>`, so `ensure_agent_image` has nothing to
    /// build from and a default-workspace task cannot launch its leader at
    /// all. Scaffolding fills both in from the bundled templates.
    #[tokio::test]
    async fn a_plain_directory_workspace_gains_the_image_sources_it_needs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");

        SquadAgentLauncher::ensure_directory_workspace_project(&root, &["claude".to_string()])
            .await
            .unwrap();

        let paths = RepoDockerfilePaths::new(&root);
        assert!(
            paths.project_dockerfile().exists(),
            "the base image needs a Dockerfile.dev at the task root"
        );
        let agent_dockerfile = paths.agent_dockerfile("claude");
        assert!(agent_dockerfile.exists());
        let body = std::fs::read_to_string(&agent_dockerfile).unwrap();
        assert!(
            !body.contains("{{AWMAN_BASE_IMAGE}}"),
            "the base-image placeholder must be substituted, as it is on the \
             `awman init` path: {body}"
        );
        assert!(
            body.contains(&crate::data::image_tags::project_image_tag(&root)),
            "the agent image must layer on this task root's own base image"
        );
        assert_eq!(
            paths
                .discover_agent_dockerfiles()
                .into_iter()
                .map(|(agent, _)| agent)
                .collect::<Vec<_>>(),
            vec!["claude".to_string()],
            "the scaffolded Dockerfile must be discoverable for the leader's \
             agent listing and for workflow validation"
        );
    }

    /// The durable workspace belongs to the task for its whole lifetime, so
    /// scaffolding is create-if-missing: an edited Dockerfile survives every
    /// later run untouched.
    #[tokio::test]
    async fn scaffolding_never_overwrites_an_existing_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let paths = RepoDockerfilePaths::new(&root);
        std::fs::create_dir_all(paths.awman_dir()).unwrap();
        std::fs::write(paths.project_dockerfile(), "FROM my-own-base\n").unwrap();
        std::fs::write(paths.agent_dockerfile("claude"), "FROM my-own-agent\n").unwrap();

        SquadAgentLauncher::ensure_directory_workspace_project(&root, &["claude".to_string()])
            .await
            .unwrap();
        SquadAgentLauncher::ensure_directory_workspace_project(&root, &["claude".to_string()])
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.project_dockerfile()).unwrap(),
            "FROM my-own-base\n"
        );
        assert_eq!(
            std::fs::read_to_string(paths.agent_dockerfile("claude")).unwrap(),
            "FROM my-own-agent\n"
        );
    }

    /// An agent with no bundled template is left to `ensure_agent_image`'s
    /// existing "agent has no Dockerfile" error rather than being invented.
    #[tokio::test]
    async fn directory_workspace_renders_approved_custom_agents_with_the_session_env() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let home = temp.path().join("global");
        std::fs::create_dir_all(&root).unwrap();
        let env = crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            home.to_str().unwrap(),
        )]);
        crate::engine::agent::custom::install_crush_fixture(&root, &env);
        SquadAgentLauncher::ensure_directory_workspace_project_with_env(
            &env,
            &root,
            &["crush".into()],
        )
        .await
        .unwrap();
        let paths = RepoDockerfilePaths::new(&root);
        let rendered = std::fs::read_to_string(paths.agent_dockerfile("crush")).unwrap();
        assert!(rendered.starts_with(crate::engine::agent::custom::DOCKERFILE_HEADER_PREFIX));
        assert!(rendered.contains(&format!(
            "FROM {}",
            crate::data::image_tags::project_image_tag(&root)
        )));
        std::fs::remove_file(home.join("trusted-agents.json")).unwrap();
        let error = SquadAgentLauncher::ensure_directory_workspace_project_with_env(
            &env,
            &root,
            &["crush".into()],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            EngineError::AgentDefinitionUntrusted { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(paths.agent_dockerfile("crush")).unwrap(),
            rendered
        );
    }

    #[tokio::test]
    async fn an_unknown_agent_surfaces_unknown_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RepoDockerfilePaths::new(tmp.path());
        std::fs::create_dir_all(paths.awman_dir()).unwrap();
        std::fs::write(
            paths.agent_dockerfile("not-a-real-agent"),
            "# awman-generated from repo:deleted\nFROM project\n",
        )
        .unwrap();
        let error = SquadAgentLauncher::ensure_directory_workspace_project(
            tmp.path(),
            &["not-a-real-agent".to_string()],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, EngineError::UnknownAgent { name, .. } if name == "not-a-real-agent")
        );
    }
}

/// WI 0141: the unattended ("no user") mapping of OSC 7501 reports.
#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::data::program_status::ProgramState;
    use crate::engine::agent_runtime::execution::{CancelHandle, ExecutionBackend};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc as std_mpsc;

    /// Runs until the test sends an exit code, or its cancel handle fires
    /// (exit `KILLED_EXIT_CODE`).
    struct Held {
        rx: std_mpsc::Receiver<i32>,
        tx: std_mpsc::Sender<i32>,
        killed: Arc<AtomicBool>,
    }

    impl ExecutionBackend for Held {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            let Held { rx, tx, .. } = *self;
            drop(tx);
            let now = chrono::Utc::now();
            Ok(AgentExitInfo {
                exit_code: rx.recv().unwrap_or(KILLED_EXIT_CODE),
                signal: None,
                started_at: now,
                ended_at: now,
            })
        }
        fn cancel(&self) -> Result<(), EngineError> {
            self.killed.store(true, Ordering::SeqCst);
            let _ = self.tx.send(KILLED_EXIT_CODE);
            Ok(())
        }
        fn cancel_handle(&self) -> CancelHandle {
            let tx = self.tx.clone();
            let killed = self.killed.clone();
            CancelHandle::new(move || {
                killed.store(true, Ordering::SeqCst);
                let _ = tx.send(KILLED_EXIT_CODE);
                Ok(())
            })
        }
    }

    struct Agent {
        events: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
        exit: std_mpsc::Sender<i32>,
        killed: Arc<AtomicBool>,
        task: tokio::task::JoinHandle<Result<UnattendedExit, EngineError>>,
    }

    /// Launch a held agent under `drive_unattended_agent_with` and wait until
    /// it listens for events.
    async fn agent(yolo_ms: u64, done_ms: u64) -> Agent {
        agent_tracked(yolo_ms, done_ms, None).await
    }

    /// Like [`agent`], with the OSC 7501 tracker a byte-stream bridge owns.
    async fn agent_tracked(
        yolo_ms: u64,
        done_ms: u64,
        tracker: Option<crate::engine::agent_runtime::program_status::SharedProgramStatusTracker>,
    ) -> Agent {
        let (tx, rx) = std_mpsc::channel();
        let killed = Arc::new(AtomicBool::new(false));
        let (events, _) = tokio::sync::broadcast::channel(16);
        let events = Arc::new(events);
        let now = chrono::Utc::now();
        let execution = AgentExecution::new(
            crate::data::session::AgentHandle {
                id: "id".into(),
                image_tag: "img".into(),
                name: "awman-squad-t-00000000".into(),
                started_at: now,
            },
            Box::new(Held {
                rx,
                tx: tx.clone(),
                killed: killed.clone(),
            }),
            events.clone(),
            None,
        );
        let execution = match tracker {
            Some(tracker) => execution.with_program_status(tracker),
            None => execution,
        };
        let durations = CountdownDurations {
            yolo: Duration::from_millis(yolo_ms),
            agent_done: Duration::from_millis(done_ms),
        };
        let task = tokio::spawn(async move {
            drive_unattended_agent_with(execution, "t", "leader", durations).await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while events.receiver_count() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "agent never listened"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Agent {
            events,
            exit: tx,
            killed,
            task,
        }
    }

    fn report(state: ProgramState) -> StuckEvent {
        StuckEvent::Reported(Some(EffectiveStatus {
            state,
            kind: None,
            progress: None,
            msg: Some("why".into()),
            app: None,
            id: None,
        }))
    }

    async fn finish(agent: Agent) -> UnattendedExit {
        tokio::time::timeout(Duration::from_secs(5), agent.task)
            .await
            .expect("drive finished")
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn done_and_idle_advance_after_the_agent_done_countdown() {
        for state in [ProgramState::Done, ProgramState::Idle] {
            let a = agent(60_000, 30).await;
            a.events.send(report(state)).unwrap();
            let killed = a.killed.clone();
            let exit = finish(a).await;
            assert!(killed.load(Ordering::SeqCst), "{state:?}");
            assert!(exit.killed_by_countdown, "{state:?}: a normal auto-advance");
            assert!(exit.reported_problem.is_none());
        }
    }

    #[tokio::test]
    async fn blocked_fails_after_its_countdown() {
        let a = agent(30, 60_000).await;
        a.events.send(report(ProgramState::Blocked)).unwrap();
        let killed = a.killed.clone();
        let exit = finish(a).await;
        assert!(killed.load(Ordering::SeqCst));
        assert!(!exit.killed_by_countdown);
        assert_eq!(exit.exit.exit_code, KILLED_EXIT_CODE);
        assert_eq!(
            exit.reported_problem.map(|p| p.state),
            Some(ProgramState::Blocked)
        );
    }

    #[tokio::test]
    async fn error_report_does_not_kill_a_live_agent() {
        let a = agent(30, 60_000).await;
        a.events.send(report(ProgramState::Error)).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!a.killed.load(Ordering::SeqCst));
        a.exit.send(0).unwrap();
        let exit = finish(a).await;
        assert_eq!(exit.exit.exit_code, 1);
        assert_eq!(
            exit.reported_problem.map(|p| p.state),
            Some(ProgramState::Error)
        );
    }

    #[tokio::test]
    async fn a_self_exit_0_after_error_is_a_failure() {
        let a = agent(60_000, 60_000).await;
        a.events.send(report(ProgramState::Error)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        a.exit.send(0).unwrap();
        let exit = finish(a).await;
        assert_eq!(exit.exit.exit_code, 1);
        assert!(!exit.killed_by_countdown);
        assert!(exit.reported_problem.is_some());
    }

    #[tokio::test]
    async fn working_cancels_a_done_countdown() {
        let a = agent(60_000, 150).await;
        a.events.send(report(ProgramState::Done)).unwrap();
        a.events.send(report(ProgramState::Working)).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!a.killed.load(Ordering::SeqCst), "working cancelled it");
        a.exit.send(0).unwrap();
        let exit = finish(a).await;
        assert_eq!(exit.exit.exit_code, 0);
        assert!(!exit.killed_by_countdown);
    }

    /// A reporting `working` agent is never recovered by a silence countdown:
    /// a `Stuck` that raced the first report is superseded by it, and no
    /// countdown kills the busy agent however long it stays quiet.
    #[tokio::test]
    async fn a_working_agent_is_never_killed_by_a_silence_countdown() {
        let a = agent(100, 60_000).await;
        a.events.send(StuckEvent::Stuck).unwrap();
        a.events.send(report(ProgramState::Working)).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!a.killed.load(Ordering::SeqCst));
        a.exit.send(0).unwrap();
        let exit = finish(a).await;
        assert_eq!(exit.exit.exit_code, 0);
        assert!(!exit.killed_by_countdown);
        assert!(exit.reported_problem.is_none());
    }

    /// Review finding: an `error` the detector never published (debounced
    /// or racing the exit) still makes exit 0 a failure.
    #[tokio::test]
    async fn an_unpublished_error_at_exit_is_a_failure() {
        use crate::data::program_status::parse_body;
        use crate::engine::agent_runtime::program_status::{ProgramStatusTracker, ScanEvent};
        let tracker = ProgramStatusTracker::shared();
        let a = agent_tracked(60_000, 60_000, Some(tracker.clone())).await;
        assert!(tracker.lock().unwrap().apply(
            &ScanEvent::Osc(parse_body(b"state=error")),
            std::time::Instant::now()
        ));
        a.exit.send(0).unwrap();
        let exit = finish(a).await;
        assert_eq!(exit.exit.exit_code, 1);
        assert_eq!(
            exit.reported_problem.map(|p| p.state),
            Some(ProgramState::Error)
        );
    }

    /// Review finding: a second report sharing the countdown kind keeps the
    /// deadline but replaces the recorded problem (the reason quoted).
    #[tokio::test]
    async fn a_same_kind_report_replaces_the_recorded_problem() {
        use crate::data::program_status::BlockedKind;
        let a = agent(300, 60_000).await;
        let blocked = |kind, msg: &str| {
            StuckEvent::Reported(Some(EffectiveStatus {
                state: ProgramState::Blocked,
                kind: Some(kind),
                progress: None,
                msg: Some(msg.into()),
                app: None,
                id: None,
            }))
        };
        a.events
            .send(blocked(BlockedKind::Permission, "allow write?"))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        a.events
            .send(blocked(BlockedKind::Auth, "login required"))
            .unwrap();
        let exit = finish(a).await;
        let problem = exit.reported_problem.expect("problem recorded");
        assert_eq!(problem.kind, Some(BlockedKind::Auth));
        assert_eq!(problem.msg.as_deref(), Some("login required"));
    }

    /// Output after an error does not clear the report; exit 0 still fails.
    #[tokio::test]
    async fn output_after_error_keeps_the_error() {
        let a = agent(200, 60_000).await;
        a.events.send(report(ProgramState::Error)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        a.events.send(StuckEvent::Unstuck).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !a.killed.load(Ordering::SeqCst),
            "an error report does not start a kill countdown"
        );
        a.exit.send(0).unwrap();
        let exit = finish(a).await;
        assert_eq!(exit.exit.exit_code, 1);
        assert!(exit.reported_problem.is_some());
    }

    #[tokio::test]
    async fn a_non_reporting_agent_keeps_the_silence_countdown() {
        let a = agent(30, 60_000).await;
        a.events.send(StuckEvent::Stuck).unwrap();
        let exit = finish(a).await;
        assert!(exit.killed_by_countdown);
        assert!(exit.reported_problem.is_none());
    }
}
