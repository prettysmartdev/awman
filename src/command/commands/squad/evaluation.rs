//! `LocalTaskEvaluator` — the Layer-2 half of task evaluation.
//!
//! The scheduler (Layer 1) decides *when* a task is evaluated and owns its
//! run row. This module decides *what happens* when one is: resolve the leader
//! agent and model, assemble the leader prompt, launch the leader in a
//! container through [`SquadAgentLauncher`], drive the shared WI-0092
//! [`WorkflowRepairLoop`], and — when the task triggered — execute the
//! generated workflow through the same `ExecWorkflowCommand` path
//! `awman exec workflow` uses, with worktree isolation forced on.
//!
//! It lives at Layer 2 because every one of those steps is a Layer-2 concern
//! (config precedence, workflow validation, command execution). Layer 1 only
//! ever calls the [`TaskEvaluator`] trait.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::command::commands::dynamic_repair::{RepairDecision, WorkflowRepairLoop};
use crate::command::commands::exec_workflow::{
    build_effective_agents_to_models, ensure_agent_image_with_build_output,
    format_agents_with_models, format_available_agents, validate_generated_workflow,
    BuildOutputTarget, ExecWorkflowCommand, ExecWorkflowCommandFlags, ExecWorkflowCommandFrontend,
    LeaderSpec,
};
use crate::command::commands::mount_scope::MountScopeDecision;
use crate::command::commands::squad::overlay_summary::overlay_inventory;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::commands::{
    collect_all_overlay_specs, parse_overlay_list, CollectedOverlays, Command,
};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::dynamic_workflow_assets::build_squad_leader_prompt;
use crate::data::fs::task_store::{MountScope, Task};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions};
use crate::data::EngineWorkflowStateStore;
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::container::options::OverlayPermission;
use crate::engine::container::options::YoloMode;
use crate::engine::overlay::{ContextOverlay, ContextScope};
use crate::engine::squad::{
    read_verdict, EvaluationOutcome, EvaluationRequest, LeaderRunSpec, SquadAgentLauncher,
    TaskEvaluator, RUN_DIR_CONTAINER_PATH, VERDICT_FILE_NAME,
};

/// Where the task directory is mounted inside the leader's container.
/// It is the `context(workflow)` mount point, so the leader sees exactly the
/// layout `exec workflow --dynamic`'s leader sees.
pub const TASK_DIR_CONTAINER_PATH: &str = "/awman/context/workflow";

/// The frontends an unattended squad run needs.
///
/// Frontends are Layer 3; the evaluator is Layer 2 and therefore accepts them
/// through this seam rather than constructing them. The daemon supplies an
/// implementation that never prompts and never blocks.
pub trait SquadRunFrontends: Send + Sync {
    /// A frontend for one leader/repair agent launch. `label` is the repair
    /// loop's attempt label (`leader`, `leader-repair-1`, …).
    fn leader_frontend(
        &self,
        task: &str,
        run_id: &crate::data::fs::RunId,
        run_log_dir: &Path,
        label: &str,
    ) -> Result<Box<dyn AgentFrontend>, CommandError>;

    /// A frontend for executing the generated workflow. `mount_scope` is the
    /// scope captured when the task was created; the frontend must answer
    /// the mount-scope question with it and never widen it.
    fn workflow_frontend(
        &self,
        task: &str,
        run_id: &crate::data::fs::RunId,
        run_log_dir: &Path,
        mount_scope: MountScopeDecision,
    ) -> Result<Box<dyn ExecWorkflowCommandFrontend>, CommandError>;
}

/// The leader agent and model one evaluation will run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLeader {
    pub agent: String,
    pub model: Option<String>,
}

/// Every agent a directory-workspace task might launch a container for, in a
/// stable order: the task's own agent, the configured default leader, the
/// session default, and the whole configured agent pool.
///
/// The pool matters as much as the leader: the leader's *generated workflow*
/// picks its step agents from that listing, and every one of them needs a
/// Dockerfile in the task root to validate and to build.
pub fn directory_workspace_agents(
    task: &Task,
    default_leader: Option<&str>,
    agents_to_models: Option<&std::collections::HashMap<String, Vec<String>>>,
    session_default_agent: Option<&str>,
) -> Vec<String> {
    let mut agents: Vec<String> = Vec::new();
    let mut push = |candidate: Option<String>| {
        if let Some(value) = candidate {
            if !value.is_empty() && !agents.contains(&value) {
                agents.push(value);
            }
        }
    };
    push(task.agent.clone());
    push(default_leader.map(|spec| {
        spec.split_once("::")
            .map(|(agent, _)| agent)
            .unwrap_or(spec)
            .to_string()
    }));
    push(session_default_agent.map(str::to_string));
    if let Some(pool) = agents_to_models {
        let mut names: Vec<&String> = pool.keys().collect();
        names.sort();
        for name in names {
            push(Some(name.clone()));
        }
    }
    agents
}

/// What one run's leader verdict tells the evaluator to do.
///
/// The mapping is the load-bearing half of WI 0106 §6d: `workflow.toml`'s mere
/// presence is no longer evidence of anything, because the durable workspace
/// keeps one across runs and the leader may legitimately reuse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictDecision {
    /// Validate whatever `workflow.toml` is now on disk — freshly written or
    /// reused from an earlier run — and execute it.
    RunGeneratedWorkflow { reason: Option<String> },
    /// The task's triggering condition was not met this run.
    NotTriggered { reason: Option<String> },
    /// The run did not follow the verdict protocol at all.
    Failed(String),
}

/// Map a run's verdict-file read to the evaluator's outcome.
///
/// * absent or unparseable → [`VerdictDecision::Failed`]. A missing verdict is
///   a broken run (the leader ignored the protocol, crashed, or was killed),
///   never evidence of a legitimately-unmet condition, so it backs off and
///   alerts like any other evaluation failure rather than going silent.
/// * `{"triggered": false}` → [`VerdictDecision::NotTriggered`], **even when a
///   `workflow.toml` from an earlier triggered run is still on disk**.
/// * `{"triggered": true}` → [`VerdictDecision::RunGeneratedWorkflow`].
pub fn decide_from_verdict(
    verdict: Result<crate::engine::squad::RunVerdict, crate::engine::squad::VerdictError>,
) -> VerdictDecision {
    match verdict {
        Err(error) => VerdictDecision::Failed(error.to_string()),
        Ok(verdict) if verdict.triggered => VerdictDecision::RunGeneratedWorkflow {
            reason: verdict.reason,
        },
        Ok(verdict) => VerdictDecision::NotTriggered {
            reason: verdict.reason,
        },
    }
}

/// The error text for a leader that exited non-zero on its own, or `None`
/// when the exit is not a failure (WI 0112 Part 6).
///
/// The rule: a container that exits non-zero *on its own* fails the run; a
/// container the yolo countdown killed does not. `killed_by_countdown` is the
/// launcher's word on which of those a 137 was — the exit code alone cannot
/// tell them apart.
pub fn leader_exit_failure(
    exit_code: i32,
    killed_by_countdown: bool,
    log_path: &Path,
) -> Option<String> {
    if exit_code == 0 || killed_by_countdown {
        return None;
    }
    Some(format!(
        "leader agent exited with code {exit_code} (log: {})",
        log_path.display()
    ))
}

/// The reason recorded when the countdown killed a leader that never wrote a
/// verdict.
pub const LEADER_IDLE_KILL_REASON: &str =
    "leader idle: killed by the yolo countdown before writing a verdict";

/// Decide a first-attempt leader run from its exit *and* its verdict (WI
/// 0112 Part 6), in this order:
///
/// 1. non-zero exit that was not the countdown's kill → `Failed`, whatever
///    the verdict says — a crashing leader is a real error even after a
///    valid `triggered: true`;
/// 2. countdown kill with no verdict on disk → `NotTriggered` with
///    [`LEADER_IDLE_KILL_REASON`], no backoff — the leader went idle before
///    doing anything, which is not a failure;
/// 3. otherwise the verdict decides exactly as [`decide_from_verdict`], with
///    a protocol failure's message suffixed by the leader's log path.
pub fn decide_leader_outcome(
    exit_code: i32,
    killed_by_countdown: bool,
    verdict: Result<crate::engine::squad::RunVerdict, crate::engine::squad::VerdictError>,
    log_path: &Path,
) -> VerdictDecision {
    if let Some(error) = leader_exit_failure(exit_code, killed_by_countdown, log_path) {
        return VerdictDecision::Failed(error);
    }
    if killed_by_countdown && matches!(verdict, Err(crate::engine::squad::VerdictError::Missing(_)))
    {
        return VerdictDecision::NotTriggered {
            reason: Some(LEADER_IDLE_KILL_REASON.to_string()),
        };
    }
    match decide_from_verdict(verdict) {
        VerdictDecision::Failed(error) => {
            VerdictDecision::Failed(format!("{error} (log: {})", log_path.display()))
        }
        decision => decision,
    }
}

/// Resolve the leader agent/model for one task.
///
/// Precedence, highest first:
/// 1. the task's own `agent` / `model` columns;
/// 2. `squad.defaultLeader` (`agent::model`) and `squad.agentsToModels`;
/// 3. the session's resolved `default_agent` (flags → repo → global config).
///
/// `agentsToModels` is a default *pool*, not an allowlist: a task-level
/// agent that is absent from it still wins. When an agent is chosen without a
/// model, `agentsToModels` supplies that agent's first configured model.
pub fn resolve_leader(
    task: &Task,
    default_leader: Option<&str>,
    agents_to_models: Option<&std::collections::HashMap<String, Vec<String>>>,
    session_default_agent: Option<&str>,
) -> Result<ResolvedLeader, CommandError> {
    let fallback = default_leader.map(LeaderSpec::parse).transpose()?;

    let agent = task
        .agent
        .clone()
        .or_else(|| fallback.as_ref().map(|spec| spec.agent.clone()))
        .or_else(|| session_default_agent.map(str::to_string))
        .ok_or_else(|| {
            CommandError::Other(format!(
                "task {:?} has no agent: set the task's agent, \
                 squad.defaultLeader, or a global defaultAgent",
                task.name
            ))
        })?;

    // The model follows whichever level supplied it, then the configured pool
    // for the chosen agent.
    let model = task.model.clone().or_else(|| {
        fallback
            .as_ref()
            .filter(|spec| spec.agent == agent)
            .map(|spec| spec.model.clone())
            .or_else(|| {
                agents_to_models
                    .and_then(|map| map.get(&agent))
                    .and_then(|models| models.first().cloned())
            })
    });

    Ok(ResolvedLeader { agent, model })
}

/// Evaluates one task end to end.
pub struct LocalTaskEvaluator {
    engines: Engines,
    env: EnvSnapshot,
    frontends: Arc<dyn SquadRunFrontends>,
}

impl LocalTaskEvaluator {
    pub fn new(engines: Engines, env: EnvSnapshot, frontends: Arc<dyn SquadRunFrontends>) -> Self {
        Self {
            engines,
            env,
            frontends,
        }
    }

    /// Open a session rooted at the task's captured mount scope. The scope
    /// is read from the stored task and never recomputed or widened.
    ///
    /// For a [`MountScope::Directory`] task the effective root is a plain
    /// directory, so there is no git root to resolve: the session is opened
    /// with the directory standing in as its own root — the same non-git
    /// fallback `Session::open_at_git_root` already serves elsewhere. For a
    /// repository-backed task the git root is resolved for real, so a
    /// repository that has been deleted, moved, or de-initialised since the
    /// task was created fails loudly here rather than silently degrading into
    /// a direct mount of a path that is no longer a repository.
    fn open_session(&self, task: &Task) -> Result<Session, CommandError> {
        let repo_scope = task.repo_scope.clone();
        let (working_dir, git_root) = match task.mount_scope {
            MountScope::Directory => {
                if !repo_scope.is_dir() {
                    return Err(CommandError::Other(format!(
                        "task {:?} is bound to {}, which no longer exists or is not a directory",
                        task.name,
                        repo_scope.display()
                    )));
                }
                (repo_scope.clone(), repo_scope)
            }
            MountScope::GitRoot => {
                let git_root = self
                    .engines
                    .git_engine
                    .resolve_root(&repo_scope)
                    .map_err(CommandError::from)?;
                (git_root.clone(), git_root)
            }
            MountScope::Cwd => {
                let git_root = self
                    .engines
                    .git_engine
                    .resolve_root(&repo_scope)
                    .map_err(CommandError::from)?;
                (repo_scope, git_root)
            }
        };
        Session::open_at_git_root(
            working_dir,
            git_root,
            SessionOpenOptions {
                env: Some(self.env.clone()),
                ..Default::default()
            },
        )
        .map_err(|error| CommandError::Other(format!("opening squad task session: {error}")))
    }

    /// Give a plain-directory task root the image definitions an ordinary
    /// repository root already has, and build the project base image the agent
    /// images layer on top of.
    ///
    /// The file writes are create-if-missing (see
    /// [`ensure_directory_workspace_project`]), so nothing already in the
    /// durable workspace is touched. The base-image build is the same
    /// `image_exists` → `build_image` pattern
    /// [`ensure_agent_image`] uses one layer up; agent images themselves are
    /// still built by `ensure_agent_image`, not here.
    fn prepare_directory_workspace_images(
        &self,
        root: &Path,
        agents: &[String],
        sink: &mut dyn UserMessageSink,
        build_logs: &mut SquadBuildLogs,
    ) -> Result<(), CommandError> {
        crate::engine::squad::ensure_directory_workspace_project(root, agents)
            .map_err(CommandError::from)?;

        let runtime = self
            .engines
            .require_container_runtime()
            .map_err(CommandError::from)?;
        let project_tag = crate::data::image_tags::project_image_tag(root);
        if runtime.image_exists(&project_tag) {
            return Ok(());
        }
        let dockerfile = crate::data::RepoDockerfilePaths::new(root).project_dockerfile();
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("Building base image for the task workspace ({project_tag})…"),
        });
        build_logs.begin(&project_tag);
        let result = runtime
            .build_image(&project_tag, &dockerfile, root, false, &mut |line: &str| {
                build_logs.line(line);
            })
            .map_err(|error| {
                CommandError::Other(format!(
                    "failed to build the task workspace's base image from {}: {error}",
                    dockerfile.display()
                ))
            });
        build_logs.finish(
            &project_tag,
            result.as_ref().err().map(|e| e.to_string()).as_deref(),
        );
        result
    }

    async fn evaluate_inner(
        &self,
        request: &EvaluationRequest,
    ) -> Result<EvaluationOutcome, CommandError> {
        // Defence in depth: the real refusal happens at daemon startup and at
        // task creation, so reaching here under a sandbox tier means
        // something is mis-wired.
        require_container_tier(&self.engines)?;

        let task = &request.task;
        let session = self.open_session(task)?;
        let git_root = session.git_root().to_path_buf();

        let mut sink = TracingSink::new(&task.name);
        // Raw container image build output goes to per-build files under
        // `<squad root>/builds/<task>/`, never into the daemon log; the daemon
        // log gets one lifecycle line per build naming the file instead.
        let mut build_logs = SquadBuildLogs::new(&self.env, &task.name, &request.run_id)?;

        // A directory-workspace task's root is a plain directory, not a
        // repository, so it carries none of the image definitions
        // `ensure_agent_image` builds from. Fill the missing ones in from the
        // bundled templates (create-if-missing — never overwriting anything
        // already there) *before* Dockerfile discovery, so the agent listing
        // the leader sees and the workflow validation that follows both see the
        // same set. Without this a default-workspace task — the default
        // creation path — fails on every run before its leader launches.
        if !task.mount_scope.is_git_repo() {
            self.prepare_directory_workspace_images(
                &git_root,
                &directory_workspace_agents(
                    task,
                    request.default_leader.as_deref(),
                    request.agents_to_models.as_ref(),
                    session.default_agent().map(|a| a.as_str()),
                ),
                &mut sink,
                &mut build_logs,
            )?;
        }

        let dockerfiles = crate::data::RepoDockerfilePaths::new(&git_root);
        let available_agents = dockerfiles.discover_agent_dockerfiles();

        // The agent listing the leader sees: the configured pool where one is
        // set (validated against the repo's Dockerfiles), else discovery.
        let agents_section = match request
            .agents_to_models
            .as_ref()
            .filter(|map| !map.is_empty())
        {
            Some(map) => {
                let mut warnings = Vec::new();
                let effective =
                    build_effective_agents_to_models(map, &available_agents, &mut warnings)?;
                for warning in warnings {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: warning,
                    });
                }
                format_agents_with_models(&effective)
            }
            None => format_available_agents(&available_agents),
        };

        let leader = resolve_leader(
            task,
            request.default_leader.as_deref(),
            request.agents_to_models.as_ref(),
            session.default_agent().map(|a| a.as_str()),
        )?;
        let agent = AgentName::new(leader.agent.clone()).map_err(CommandError::Data)?;
        // WI 0110: build an image for every agent in the task's effective pool,
        // not just the leader's. The leader picks its generated workflow's step
        // agents from the listing it was shown, so an unbuilt pool agent would
        // otherwise cost a build in the middle of the run — or, for a pool
        // configured after the workspace was scaffolded, fail validation. The
        // leader's own image is built first so it starts as soon as it can.
        //
        // `ensure_agent_image*` is an `image_exists` fast path, so a pool whose
        // images are already built adds nothing but a runtime query per agent.
        let mut pool_agents = vec![agent.as_str().to_string()];
        if let Some(map) = request.agents_to_models.as_ref() {
            let mut names: Vec<&String> = map.keys().collect();
            names.sort();
            for name in names {
                if !pool_agents.contains(name) && available_agents.iter().any(|(a, _)| a == name) {
                    pool_agents.push(name.clone());
                }
            }
        }
        for pool_agent in &pool_agents {
            ensure_agent_image_with_build_output(
                &self.engines,
                &git_root,
                &dockerfiles,
                pool_agent,
                &mut sink,
                Some(&mut build_logs),
            )?;
        }

        // The overlay set is collected once, here, and reused for the prompt
        // and for every launch below — so the inventory the leader is shown is
        // literally the set its container is started with, not a second
        // resolution that could drift from it (WI 0117).
        let collected = collect_task_overlays(&session, task)?;

        // `guidance` is additive and never overridden by a task, so it is
        // rendered into every leader prompt regardless of which level supplied
        // the agent and model.
        let prompt = build_squad_leader_prompt(
            &task.name,
            &task.description,
            "/workspace",
            &agents_section,
            &overlay_inventory(&collected),
            &format!("{RUN_DIR_CONTAINER_PATH}/{VERDICT_FILE_NAME}"),
            request.guidance.as_deref(),
        );

        let launcher = SquadAgentLauncher::new(
            Arc::clone(&self.engines.agent_engine),
            Arc::clone(&self.engines.runtime),
        );
        let resolved_credentials = self
            .engines
            .auth_engine
            .resolve_agent_auth(&session, &agent)
            .unwrap_or_default();
        let credentials = if self.engines.runtime.capabilities().kit_declarative
            && matches!(
                resolved_credentials.delivery,
                crate::engine::auth::CredentialDelivery::File(_)
            ) {
            self.engines
                .auth_engine
                .agent_env_credentials(&agent)
                .unwrap_or_default()
        } else {
            resolved_credentials
        };

        let generated_path = request.task_dir.join("workflow.toml");
        let mut repair = WorkflowRepairLoop::new(generated_path.clone(), prompt);

        let workflow = loop {
            let label = repair.label();
            tracing::info!(
                task = %task.name,
                run_id = %request.run_id,
                attempt = %label,
                agent = %agent.as_str(),
                model = ?leader.model,
                "squad evaluation container launching"
            );
            let run_options = self.leader_run_options(
                repair.prompt(),
                leader.model.as_deref(),
                &git_root,
                agent.as_str(),
                &request.task_dir,
                &request.run_log_dir,
                &collected,
            );
            let exit = launcher
                .run_leader(
                    LeaderRunSpec {
                        session: session.clone(),
                        agent: agent.clone(),
                        run_options,
                        credentials: credentials.clone(),
                        task_name: task.name.clone(),
                        task_dir: request.task_dir.clone(),
                    },
                    self.frontends.leader_frontend(
                        &task.name,
                        &request.run_id,
                        &request.run_log_dir,
                        &label,
                    )?,
                )
                .await
                .map_err(CommandError::from)?;
            // The leader's own output lives here (the unattended frontend
            // opened it when the container started); every line about this
            // attempt names it so a failure can be debugged from the log.
            let log_path = request
                .run_log_dir
                .join(format!("{}.log", exit.container_name));
            tracing::info!(
                task = %task.name,
                run_id = %request.run_id,
                attempt = %label,
                exit_code = exit.exit.exit_code,
                log_path = %log_path.display(),
                "squad evaluation container finished"
            );
            if exit.killed_by_countdown {
                tracing::warn!(
                    task = %task.name,
                    run_id = %request.run_id,
                    attempt = %label,
                    log_path = %log_path.display(),
                    "squad evaluation leader was killed by the yolo countdown"
                );
            }

            // WI 0112 Part 6: a leader that exits non-zero on its own fails
            // the run on every attempt, repair attempts included. A countdown
            // kill is exempt — it is the engine's normal auto-advance.
            if let Some(error) =
                leader_exit_failure(exit.exit.exit_code, exit.killed_by_countdown, &log_path)
            {
                tracing::error!(
                    task = %task.name,
                    run_id = %request.run_id,
                    attempt = %label,
                    exit_code = exit.exit.exit_code,
                    log_path = %log_path.display(),
                    "squad evaluation leader failed: {error}"
                );
                return Ok(EvaluationOutcome::Failed { error });
            }

            // The leader's verdict for *this* run is the authority on whether
            // the task triggered. `workflow.toml`'s mere presence is not: the
            // task workspace is durable, so one may be left over from an
            // earlier triggered run, and the leader is explicitly allowed to
            // reuse it rather than rewrite it.
            //
            // Only the first attempt consults the verdict. A repair attempt is
            // by definition re-running a leader that already said "triggered",
            // so its job is to fix validation, not to re-decide.
            if repair.is_first_attempt() {
                match decide_leader_outcome(
                    exit.exit.exit_code,
                    exit.killed_by_countdown,
                    read_verdict(&request.run_log_dir),
                    &log_path,
                ) {
                    // A run that did not follow the protocol is an error, not a
                    // silent "not triggered": it backs off and alerts like any
                    // other evaluation failure rather than going quiet forever.
                    VerdictDecision::Failed(error) => {
                        tracing::error!(
                            task = %task.name,
                            run_id = %request.run_id,
                            log_path = %log_path.display(),
                            "squad evaluation produced no usable verdict: {error}"
                        );
                        return Ok(EvaluationOutcome::Failed { error });
                    }
                    VerdictDecision::NotTriggered { reason } => {
                        tracing::info!(
                            task = %task.name,
                            run_id = %request.run_id,
                            reason = reason.as_deref().unwrap_or("(none given)"),
                            stale_workflow_present = generated_path.exists(),
                            "squad evaluation decided not triggered"
                        );
                        return Ok(EvaluationOutcome::NotTriggered);
                    }
                    VerdictDecision::RunGeneratedWorkflow { reason } => {
                        tracing::info!(
                            task = %task.name,
                            run_id = %request.run_id,
                            reason = reason.as_deref().unwrap_or("(none given)"),
                            "squad evaluation decided triggered"
                        );
                    }
                }
            }

            match repair.record(validate_generated_workflow(
                &generated_path,
                &session,
                &dockerfiles,
            )) {
                RepairDecision::Accepted(workflow) => break *workflow,
                RepairDecision::Exhausted(message) => {
                    return Ok(EvaluationOutcome::Failed { error: message });
                }
                RepairDecision::Retry { attempt, error } => {
                    tracing::warn!(
                        task = %task.name,
                        attempt,
                        "squad: generated workflow failed validation: {error}"
                    );
                }
            }
        };

        tracing::info!(
            task = %task.name,
            run_id = %request.run_id,
            workflow_path = %generated_path.display(),
            "squad workflow generated and validated"
        );

        // Build any missing step-agent images before execution, exactly as the
        // dynamic path does (WI-0092 §9b) — the generated workflow may pick
        // agents other than the leader. Build output goes to the per-build log
        // files, not the daemon log.
        let workflow_agents =
            crate::command::commands::exec_workflow::resolve_and_validate_workflow_agents(
                &workflow,
                &session,
                &dockerfiles,
            )
            .map_err(CommandError::Other)?;
        for step_agent in &workflow_agents {
            ensure_agent_image_with_build_output(
                &self.engines,
                &git_root,
                &dockerfiles,
                step_agent,
                &mut sink,
                Some(&mut build_logs),
            )?;
        }

        // Record the engine's own state file on the run row *before* the
        // workflow starts, so the daemon's workflow route can serve live state.
        let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
        let uses_worktree = task.uses_worktree();
        let state_path = self.workflow_state_path(
            &git_root,
            &workflow_name,
            uses_worktree,
            &request.run_log_dir,
        )?;
        request
            .progress
            .workflow_started(&request.run_id, &generated_path, &state_path);
        tracing::info!(
            task = %task.name,
            run_id = %request.run_id,
            workflow_path = %generated_path.display(),
            workflow_state_path = %state_path.display(),
            "squad workflow starting"
        );

        // Stamp every generated-workflow step container with the same squad
        // identity the evaluation leader carries, so prefix discovery finds the
        // whole task's container set — not just the leader (BLOCKER-1).
        let squad_identity = crate::engine::squad::launcher::SquadContainerIdentity::new(
            task.name.clone(),
            session.id().to_string(),
        );
        let mut command = ExecWorkflowCommand::new(
            squad_workflow_flags(&generated_path, uses_worktree, &task.overlays),
            self.engines.clone(),
            session,
        )
        .with_squad_identity(squad_identity)
        // Every step container also gets the durable task workspace at the
        // stable `context(workflow)` path, so a task's persistent data is
        // reachable from its workflow as well as from its leader.
        .with_task_workspace(request.task_dir.clone());
        if !uses_worktree {
            // No worktree means the engine's session is rooted at the task's
            // own mounted directory, which must survive every run untouched.
            // Keep awman's workflow-state bookkeeping — which is written,
            // rewritten and deleted every run — in this run's directory
            // instead. Must agree with `workflow_state_path` above, which is
            // what the run row records for the daemon's workflow route.
            command = command.with_workflow_state_root(request.run_log_dir.clone());
        }
        let outcome = command
            .run_with_frontend(self.frontends.workflow_frontend(
                &task.name,
                &request.run_id,
                &request.run_log_dir,
                mount_scope_decision(task.mount_scope),
            )?)
            .await?;

        tracing::info!(
            task = %task.name,
            run_id = %request.run_id,
            exit_code = ?outcome.exit_code,
            "squad workflow finished"
        );

        Ok(EvaluationOutcome::WorkflowExecuted {
            workflow_path: generated_path,
            workflow_state_path: Some(state_path),
            exit_code: outcome.exit_code,
        })
    }

    /// The leader's run options: yolo (so an unattended agent never blocks on
    /// a permission prompt), interactive PTY mode (so attach reconnects to the
    /// real agent UI), the durable task workspace mounted read-write at the
    /// `context(workflow)` path, this run's directory mounted read-write so
    /// the leader can write its verdict, and the task's own overlays merged
    /// into the standing global/repo/`AWMAN_OVERLAYS` sources.
    ///
    /// The task workspace is mounted for *both* workspace modes: a
    /// custom-directory task gets its custom folder as the working tree and
    /// still gets `tasks/<name>/workspace/` at the same stable container path,
    /// so task-scoped persistent data survives across runs either way.
    #[allow(clippy::too_many_arguments)]
    fn leader_run_options(
        &self,
        prompt: &str,
        model: Option<&str>,
        image_git_root: &Path,
        agent: &str,
        task_dir: &Path,
        run_log_dir: &Path,
        collected: &CollectedOverlays,
    ) -> AgentRunOptions {
        // The run directory is a structural, always-on mount, not a
        // user-specified overlay: it is how the leader reports its verdict for
        // this run, and its container path is fixed and documented so the
        // prompt can name it outright.
        let mut directory_overlays = collected.directories.clone();
        directory_overlays.push(crate::engine::overlay::DirectorySpec {
            host: run_log_dir.to_string_lossy().into_owned(),
            container: RUN_DIR_CONTAINER_PATH.to_string(),
            permission: OverlayPermission::ReadWrite,
        });

        AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            non_interactive: false,
            initial_prompt: Some(prompt.to_string()),
            model: model.map(str::to_string),
            image_tag_override: Some(crate::data::image_tags::agent_image_tag(
                image_git_root,
                agent,
            )),
            env_passthrough: if collected.env_passthrough.is_empty() {
                None
            } else {
                Some(collected.env_passthrough.clone())
            },
            directory_overlays,
            include_all_skills: collected.include_all_skills,
            named_skills: collected.named_skills.clone(),
            context_overlays: vec![ContextOverlay {
                scope: ContextScope::Workflow,
                host_path: task_dir.to_path_buf(),
                container_path: PathBuf::from(TASK_DIR_CONTAINER_PATH),
                permission: OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        }
    }

    /// The engine's `WorkflowStateStore` file for the run that is about to
    /// start.
    ///
    /// A repository-backed task is worktree-isolated, so the engine's session —
    /// and therefore its state store — is rooted at the workflow's worktree,
    /// which is created and disposed of per run.
    ///
    /// A directory-workspace task has no worktree, and its mounted directory
    /// must survive every run untouched (WI 0106 §6a). The state store
    /// creates, rewrites and — because every scheduled run starts fresh —
    /// *deletes* its file, so it is rooted at this run's own
    /// `runs/<run-id>/` directory instead, which is new every run and
    /// therefore never carries state forward or erases anything the leader or
    /// its workflow wrote.
    fn workflow_state_path(
        &self,
        git_root: &Path,
        workflow_name: &str,
        uses_worktree: bool,
        run_log_dir: &Path,
    ) -> Result<PathBuf, CommandError> {
        if !uses_worktree {
            return Ok(directory_workspace_state_path(run_log_dir, workflow_name));
        }
        let lifecycle =
            crate::command::commands::worktree_lifecycle::WorktreeLifecycle::for_workflow(
                Arc::clone(&self.engines.git_engine),
                git_root.to_path_buf(),
                workflow_name,
            )?;
        let worktree = lifecycle.worktree_path().to_path_buf();
        // A linked worktree is its own git root; before it exists `resolve_root`
        // cannot say so, and the worktree path is the answer either way.
        let state_root = self
            .engines
            .git_engine
            .resolve_root(&worktree)
            .unwrap_or(worktree);
        Ok(EngineWorkflowStateStore::at_git_root(state_root).state_path(None, workflow_name))
    }
}

#[async_trait]
impl TaskEvaluator for LocalTaskEvaluator {
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
        match self.evaluate_inner(&request).await {
            Ok(outcome) => outcome,
            // Never panic and never propagate: the scheduler records the error
            // and grows the task's backoff.
            Err(error) => EvaluationOutcome::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// Where a directory-workspace task's engine workflow-state file lives:
/// inside *this run's* `runs/<run-id>/` directory, never inside the durable
/// task workspace the run mounts.
///
/// The engine writes, rewrites and — since every scheduled squad run starts
/// fresh rather than resuming — deletes this file. Rooting it at the mounted
/// directory would put that create/delete cycle inside a directory WI 0106
/// §6a requires to survive every run untouched. A run-scoped directory is new
/// each run, so it can neither carry stale state forward nor erase anything
/// the leader or its workflow wrote.
fn directory_workspace_state_path(run_log_dir: &Path, workflow_name: &str) -> PathBuf {
    EngineWorkflowStateStore::at_git_root(run_log_dir.to_path_buf()).state_path(None, workflow_name)
}

/// The task's fully-merged overlay set: its own `overlays` on top of the
/// standing global-config, repo-config and `AWMAN_OVERLAYS` sources.
///
/// Task overlays enter through the same slot `--overlay` uses, so they combine
/// under the existing precedence and collision rules rather than a second set
/// invented for squad. The result is used twice — to launch the leader, and to
/// tell the leader in its prompt what it actually has — and both must come from
/// this one resolution so they cannot disagree (WI 0117).
fn collect_task_overlays(
    session: &Session,
    task: &Task,
) -> Result<CollectedOverlays, CommandError> {
    let mut cli_typed = Vec::new();
    for spec in &task.overlays {
        cli_typed.extend(parse_overlay_list(spec).map_err(|reason| {
            CommandError::InvalidOverlaySpec {
                spec: spec.clone(),
                reason,
            }
        })?);
    }
    collect_all_overlay_specs(session, cli_typed, None, None)
}

/// The flags a squad-generated workflow always run with: interactive PTY mode
/// (the daemon supplies a fixed terminal and attach reconnects to it), yolo
/// (never block on an approval prompt), the task's own overlays, and worktree
/// isolation for exactly the tasks that can have one.
///
/// `uses_worktree` comes from the task's captured scope, not from a per-run
/// filesystem probe. A repository-backed task is *always* worktree-isolated, so
/// two runs touching one repo cannot collide. A directory-workspace task never
/// is: there is no repository to branch a worktree from, so the workspace is
/// mounted directly — which takes the worktree lifecycle's existing
/// "no worktree requested" branch rather than a squad-specific bypass.
fn squad_workflow_flags(
    workflow_path: &Path,
    uses_worktree: bool,
    overlays: &[String],
) -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: Some(workflow_path.to_path_buf()),
        work_item: None,
        non_interactive: false,
        plan: false,
        allow_docker: false,
        worktree: uses_worktree,
        yolo: true,
        auto: false,
        agent: None,
        model: None,
        launch_mode: None,
        // The task's overlays enter through the same `--overlay` slot the CLI
        // uses, so every step container combines them with the standing
        // global/repo/env sources under the existing precedence rules.
        overlay: overlays.to_vec(),
        max_concurrent: None,
        issue_source: Default::default(),
        dynamic: false,
        leader: None,
    }
}

/// Translate the stored mount scope into the answer the workflow frontend gives
/// when asked. The scope is captured at creation and is never widened here.
fn mount_scope_decision(scope: MountScope) -> MountScopeDecision {
    match scope {
        MountScope::GitRoot => MountScopeDecision::MountGitRoot,
        MountScope::Cwd => MountScopeDecision::MountCurrentDirOnly,
        // A directory workspace *is* the whole root: its working dir and its
        // "git root" are the same path, so the frontend is never actually
        // asked. Answering "mount the root" keeps the never-widen invariant
        // trivially true either way.
        MountScope::Directory => MountScopeDecision::MountGitRoot,
    }
}

/// Per-build log files for one run's container image builds, plus the
/// lifecycle lines the daemon log carries instead of the raw build stream.
///
/// Files live at `<squad root>/builds/<task>/<run-id>-<n>.log` — one file per
/// build that actually happens (`begin` is only called when an image is
/// missing, so an image-exists fast path never leaves an empty file behind).
pub(crate) struct SquadBuildLogs {
    task: String,
    dir: std::path::PathBuf,
    run_id: String,
    seq: u32,
    current: Option<(std::path::PathBuf, std::fs::File)>,
}

impl SquadBuildLogs {
    pub(crate) fn new(
        env: &EnvSnapshot,
        task: &str,
        run_id: &crate::data::fs::RunId,
    ) -> Result<Self, CommandError> {
        let paths = crate::data::fs::SquadPaths::from_env(env).map_err(CommandError::Data)?;
        let dir = paths.task_builds_dir(task).map_err(CommandError::Data)?;
        std::fs::create_dir_all(&dir).map_err(|error| {
            CommandError::Other(format!(
                "preparing squad build-log directory {}: {error}",
                dir.display()
            ))
        })?;
        Ok(Self {
            task: task.to_string(),
            dir,
            run_id: run_id.as_str().to_string(),
            seq: 0,
            current: None,
        })
    }
}

impl BuildOutputTarget for SquadBuildLogs {
    fn begin(&mut self, image: &str) {
        self.seq += 1;
        let path = self.dir.join(format!("{}-{}.log", self.run_id, self.seq));
        match std::fs::File::create(&path) {
            Ok(file) => {
                tracing::info!(
                    task = %self.task,
                    image,
                    log_path = %path.display(),
                    "squad container image build started"
                );
                self.current = Some((path, file));
            }
            Err(error) => {
                // The build still proceeds; its output is simply dropped
                // rather than rerouted into the daemon log.
                tracing::error!(
                    task = %self.task,
                    image,
                    log_path = %path.display(),
                    error = %error,
                    "squad failed to open container image build log"
                );
            }
        }
    }

    fn line(&mut self, line: &str) {
        use std::io::Write;
        if let Some((_, file)) = self.current.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }

    fn finish(&mut self, image: &str, error: Option<&str>) {
        use std::io::Write;
        let path = self.current.take().map(|(path, mut file)| {
            let _ = file.flush();
            path
        });
        let log_path = path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no log file)".to_string());
        match error {
            None => tracing::info!(
                task = %self.task,
                image,
                log_path,
                "squad container image build finished"
            ),
            Some(error) => tracing::error!(
                task = %self.task,
                image,
                log_path,
                error,
                "squad container image build failed"
            ),
        }
    }
}

/// A `UserMessageSink` for the unattended path: messages go to the daemon log.
struct TracingSink {
    task: String,
}

impl TracingSink {
    fn new(task: &str) -> Self {
        Self {
            task: task.to_string(),
        }
    }
}

impl UserMessageSink for TracingSink {
    fn write_message(&mut self, message: UserMessage) {
        match message.level {
            MessageLevel::Error => {
                tracing::error!(task = %self.task, "{}", message.text)
            }
            MessageLevel::Warning => {
                tracing::warn!(task = %self.task, "{}", message.text)
            }
            _ => tracing::info!(task = %self.task, "{}", message.text),
        }
    }
    fn replay_queued(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    // ── WI 0112 Part 6: the leader's exit and verdict decide together ──────

    fn verdict(
        triggered: bool,
    ) -> Result<crate::engine::squad::RunVerdict, crate::engine::squad::VerdictError> {
        Ok(crate::engine::squad::RunVerdict {
            triggered,
            reason: None,
        })
    }

    fn missing() -> Result<crate::engine::squad::RunVerdict, crate::engine::squad::VerdictError> {
        Err(crate::engine::squad::VerdictError::Missing(PathBuf::from(
            "/run/verdict.json",
        )))
    }

    #[test]
    fn a_clean_exit_defers_to_the_verdict() {
        let log = Path::new("/run/leader.log");
        assert!(matches!(
            decide_leader_outcome(0, false, verdict(true), log),
            VerdictDecision::RunGeneratedWorkflow { .. }
        ));
        assert!(matches!(
            decide_leader_outcome(0, false, verdict(false), log),
            VerdictDecision::NotTriggered { .. }
        ));
        match decide_leader_outcome(0, false, missing(), log) {
            VerdictDecision::Failed(error) => {
                assert!(error.contains("wrote no verdict"), "{error}");
                assert!(error.ends_with("(log: /run/leader.log)"), "{error}");
            }
            other => panic!("a clean exit with no verdict is a protocol failure: {other:?}"),
        }
    }

    #[test]
    fn a_countdown_kill_honours_a_written_verdict_and_is_not_triggered_without_one() {
        let log = Path::new("/run/leader.log");
        assert!(matches!(
            decide_leader_outcome(137, true, verdict(true), log),
            VerdictDecision::RunGeneratedWorkflow { .. }
        ));
        match decide_leader_outcome(137, true, missing(), log) {
            VerdictDecision::NotTriggered { reason } => {
                assert_eq!(reason.as_deref(), Some(LEADER_IDLE_KILL_REASON));
            }
            other => panic!("an idle leader killed by the countdown is not a failure: {other:?}"),
        }
        assert!(leader_exit_failure(137, true, log).is_none());
    }

    #[test]
    fn a_non_zero_exit_that_was_not_the_countdown_fails_whatever_the_verdict_says() {
        let log = Path::new("/run/leader.log");
        for code in [1, 137] {
            for v in [verdict(true), verdict(false), missing()] {
                match decide_leader_outcome(code, false, v, log) {
                    VerdictDecision::Failed(error) => {
                        assert_eq!(
                            error,
                            format!("leader agent exited with code {code} (log: /run/leader.log)")
                        );
                    }
                    other => panic!("exit {code} must fail: {other:?}"),
                }
            }
        }
        assert!(leader_exit_failure(0, false, log).is_none());
        assert!(leader_exit_failure(2, false, log).is_some());
    }

    fn task(agent: Option<&str>, model: Option<&str>) -> Task {
        let now = Utc::now();
        Task {
            id: "id".into(),
            name: "issue-triage".into(),
            description: "when an issue opens, plan it".into(),
            repo_scope: PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            overlays: Vec::new(),
            interval_secs: 300,
            status: crate::data::fs::task_store::TaskStatus::Active,
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        }
    }

    fn pool() -> HashMap<String, Vec<String>> {
        HashMap::from([(
            "codex".to_string(),
            vec!["gpt-5".to_string(), "gpt-5-mini".to_string()],
        )])
    }

    #[test]
    fn a_task_level_agent_and_model_beat_every_default() {
        let resolved = resolve_leader(
            &task(Some("claude"), Some("claude-opus-5")),
            Some("codex::gpt-5"),
            Some(&pool()),
            Some("gemini"),
        )
        .unwrap();
        assert_eq!(resolved.agent, "claude");
        assert_eq!(resolved.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn a_task_level_agent_wins_even_when_absent_from_the_configured_pool() {
        // `agentsToModels` is a default pool, not an allowlist.
        let resolved =
            resolve_leader(&task(Some("claude"), None), None, Some(&pool()), None).unwrap();
        assert_eq!(resolved.agent, "claude");
        assert_eq!(resolved.model, None);
    }

    #[test]
    fn default_leader_beats_the_global_default_agent() {
        let resolved = resolve_leader(
            &task(None, None),
            Some("codex::gpt-5"),
            Some(&pool()),
            Some("gemini"),
        )
        .unwrap();
        assert_eq!(resolved.agent, "codex");
        assert_eq!(resolved.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn the_global_default_agent_is_the_last_resort() {
        let resolved = resolve_leader(&task(None, None), None, None, Some("gemini")).unwrap();
        assert_eq!(resolved.agent, "gemini");
        assert_eq!(resolved.model, None);
    }

    #[test]
    fn agents_to_models_supplies_a_model_for_an_agent_chosen_without_one() {
        let resolved =
            resolve_leader(&task(Some("codex"), None), None, Some(&pool()), None).unwrap();
        assert_eq!(resolved.agent, "codex");
        assert_eq!(resolved.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn a_task_level_model_overrides_the_configured_pool() {
        let resolved = resolve_leader(
            &task(Some("codex"), Some("gpt-5-mini")),
            None,
            Some(&pool()),
            None,
        )
        .unwrap();
        assert_eq!(resolved.model.as_deref(), Some("gpt-5-mini"));
    }

    #[test]
    fn no_agent_at_any_level_is_an_actionable_error() {
        let error = resolve_leader(&task(None, None), None, None, None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("issue-triage"), "{text}");
        assert!(text.contains("squad.defaultLeader"), "{text}");
    }

    #[test]
    fn a_malformed_default_leader_is_rejected() {
        let error =
            resolve_leader(&task(None, None), Some("codex"), None, Some("gemini")).unwrap_err();
        assert!(error.to_string().contains("agent::model"));
    }

    #[test]
    fn guidance_is_rendered_into_the_prompt_whichever_level_supplied_the_agent() {
        let guidance = vec!["always run the tests".to_string()];
        for (default_leader, session_default) in [
            (Some("codex::gpt-5"), None),
            (None, Some("gemini")),
            (None, None),
        ] {
            let task = if default_leader.is_none() && session_default.is_none() {
                task(Some("claude"), None)
            } else {
                task(None, None)
            };
            let leader = resolve_leader(&task, default_leader, None, session_default).unwrap();
            let prompt = build_squad_leader_prompt(
                &task.name,
                &task.description,
                "/workspace",
                &format!("  - {}", leader.agent),
                &Default::default(),
                "/awman/squad/run/verdict.json",
                Some(&guidance),
            );
            assert!(
                prompt.contains("always run the tests"),
                "guidance must be additive regardless of which level chose the agent \
                 (leader {:?})",
                leader.agent
            );
            assert!(prompt.contains("issue-triage"));
        }
    }

    /// WI 0117: the overlay inventory reaches the leader intact, and the
    /// template keeps no unreplaced slot after the new one was added.
    #[test]
    fn the_overlay_inventory_is_substituted_into_the_leader_prompt() {
        let collected = CollectedOverlays {
            directories: vec![crate::engine::overlay::DirectorySpec {
                host: "~/.ssh".into(),
                container: "/root/.ssh".into(),
                permission: OverlayPermission::ReadOnly,
            }],
            env_passthrough: vec!["GITHUB_TOKEN".into()],
            ..Default::default()
        };
        let prompt = build_squad_leader_prompt(
            "issue-triage",
            "when a new issue is opened, draft a plan",
            "/workspace",
            "  - claude",
            &overlay_inventory(&collected),
            "/awman/squad/run/verdict.json",
            None,
        );
        assert!(
            prompt.contains("- `/root/.ssh` (read-only) — from host `~/.ssh`"),
            "the leader must be told which directories it has, got: {prompt}"
        );
        assert!(
            prompt.contains("GITHUB_TOKEN"),
            "the leader must be told which env names its task declares, got: {prompt}"
        );
        assert!(
            prompt.contains("### Host directories") && prompt.contains("### Skills"),
            "the template must supply the inventory's headings, got: {prompt}"
        );
        assert!(
            !prompt.contains("{{"),
            "no template slot may survive substitution, got: {prompt}"
        );
    }

    #[test]
    fn generated_workflows_carry_the_unattended_guardrails_and_the_tasks_overlays() {
        let overlays = vec!["ssh()".to_string(), "env(GITHUB_TOKEN)".to_string()];
        let flags = squad_workflow_flags(Path::new("/c/workflow.toml"), true, &overlays);
        assert!(
            flags.yolo,
            "an unattended agent must never block on a prompt"
        );
        // Squad agents run PTY-backed so attach reaches the real agent UI
        // (WI 0106 §3c), which is the opposite of the pre-0106 piped mode.
        assert!(!flags.non_interactive);
        assert!(!flags.allow_docker);
        assert!(!flags.dynamic, "the workflow is already generated");
        assert_eq!(
            flags.overlay, overlays,
            "a task's overlays reach every step through the same --overlay slot"
        );
        assert_eq!(
            flags.workflow.as_deref(),
            Some(Path::new("/c/workflow.toml"))
        );
    }

    #[test]
    fn worktree_isolation_follows_the_tasks_captured_workspace_kind() {
        // A repository-backed task is always worktree-isolated, so two runs
        // touching one repo cannot collide…
        assert!(
            squad_workflow_flags(Path::new("/c/workflow.toml"), true, &[]).worktree,
            "a repository-backed task must always be worktree-isolated"
        );
        // …and a directory workspace never is: there is no repository to
        // branch a worktree from, so the workspace is mounted directly.
        assert!(
            !squad_workflow_flags(Path::new("/c/workflow.toml"), false, &[]).worktree,
            "a directory-workspace task must never request a worktree"
        );
    }

    #[test]
    fn a_directory_workspace_task_never_uses_a_worktree() {
        let mut task = task(None, None);
        task.mount_scope = MountScope::Directory;
        assert!(!task.uses_worktree());
        for scope in [MountScope::GitRoot, MountScope::Cwd] {
            task.mount_scope = scope;
            assert!(
                task.uses_worktree(),
                "a repository-backed task is worktree-isolated under {scope:?}"
            );
        }
    }

    #[test]
    fn the_captured_mount_scope_is_answered_verbatim_and_never_widened() {
        assert!(matches!(
            mount_scope_decision(MountScope::Cwd),
            MountScopeDecision::MountCurrentDirOnly
        ));
        assert!(matches!(
            mount_scope_decision(MountScope::GitRoot),
            MountScopeDecision::MountGitRoot
        ));
        // A directory workspace is its own root, so there is nothing to widen
        // and the frontend is never actually asked.
        assert!(matches!(
            mount_scope_decision(MountScope::Directory),
            MountScopeDecision::MountGitRoot
        ));
    }

    /// WI 0106 §6a: nothing awman writes on its own behalf may land in — let
    /// alone be deleted from — the durable directory a task mounts. The engine
    /// state file is exactly such a file (written every run, deleted whenever
    /// a run starts fresh, which every scheduled squad run does), so a
    /// directory-workspace run keeps it in that run's own directory.
    #[test]
    fn a_directory_workspaces_engine_state_file_lives_in_the_run_dir_not_the_workspace() {
        let workspace = Path::new("/home/u/.awman/squad/tasks/nightly/workspace");
        let run_dir = Path::new("/home/u/.awman/squad/tasks/nightly/runs/run-1");

        let state_path = directory_workspace_state_path(run_dir, "nightly-sweep");

        assert!(
            state_path.starts_with(run_dir),
            "state must be rooted at the run directory: {}",
            state_path.display()
        );
        assert!(
            !state_path.starts_with(workspace),
            "state must never be written inside the durable task workspace: {}",
            state_path.display()
        );
        // Two runs of the same task never share a state file, so one run can
        // never delete or resume another's.
        assert_ne!(
            state_path,
            directory_workspace_state_path(
                Path::new("/home/u/.awman/squad/tasks/nightly/runs/run-2"),
                "nightly-sweep",
            )
        );
    }

    // ── The verdict → outcome protocol (WI 0106 §6d) ──────────────────────

    /// Write a run-scoped verdict file and read the decision back through the
    /// same `read_verdict` the evaluator uses.
    fn decision_for(body: Option<&str>) -> VerdictDecision {
        let tmp = tempfile::tempdir().unwrap();
        if let Some(body) = body {
            std::fs::write(
                tmp.path().join(crate::engine::squad::VERDICT_FILE_NAME),
                body,
            )
            .unwrap();
        }
        decide_from_verdict(crate::engine::squad::read_verdict(tmp.path()))
    }

    #[test]
    fn a_triggered_verdict_runs_whatever_workflow_is_on_disk() {
        assert_eq!(
            decision_for(Some(r#"{"triggered": true, "reason": "new issue"}"#)),
            VerdictDecision::RunGeneratedWorkflow {
                reason: Some("new issue".into())
            },
            "a fresh triggered verdict proceeds to validate the workflow.toml \
             on disk, whether the leader rewrote it or reused an earlier run's"
        );
    }

    #[test]
    fn an_untriggered_verdict_wins_over_a_stale_workflow_file() {
        // The stale `workflow.toml` lives in the durable workspace and is
        // deliberately *not* consulted: only the verdict decides.
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("workflow.toml"), "from run 1").unwrap();
        assert_eq!(
            decision_for(Some(r#"{"triggered": false, "reason": "quiet"}"#)),
            VerdictDecision::NotTriggered {
                reason: Some("quiet".into())
            }
        );
        assert!(
            workspace.path().join("workflow.toml").exists(),
            "deciding not-triggered must not touch the durable workspace"
        );
    }

    #[test]
    fn a_missing_verdict_is_a_failed_run_not_a_silent_not_triggered() {
        let decision = decision_for(None);
        assert!(
            matches!(decision, VerdictDecision::Failed(_)),
            "a leader that wrote no verdict is a broken run: {decision:?}"
        );
    }

    #[test]
    fn an_unparseable_verdict_is_a_failed_run() {
        let decision = decision_for(Some("{ not json"));
        assert!(
            matches!(decision, VerdictDecision::Failed(_)),
            "an unparseable verdict is a protocol violation: {decision:?}"
        );
    }

    // ── Directory-workspace image sources (the default creation path) ──────

    /// A directory workspace has no repository to inherit Dockerfiles from, so
    /// every agent the run might launch — the leader *and* the pool the
    /// generated workflow picks its steps from — has to be scaffolded.
    #[test]
    fn a_directory_workspace_scaffolds_the_leader_and_the_whole_configured_pool() {
        let task = task(Some("claude"), None);
        let mut pool = HashMap::new();
        pool.insert("codex".to_string(), vec!["gpt-5".to_string()]);
        let agents =
            directory_workspace_agents(&task, Some("opencode::sonnet"), Some(&pool), Some("crush"));
        assert_eq!(agents, vec!["claude", "opencode", "crush", "codex"]);
    }

    #[test]
    fn directory_workspace_agents_never_repeat_a_name() {
        let task = task(Some("claude"), None);
        let mut pool = HashMap::new();
        pool.insert("claude".to_string(), vec!["opus".to_string()]);
        let agents =
            directory_workspace_agents(&task, Some("claude::opus"), Some(&pool), Some("claude"));
        assert_eq!(agents, vec!["claude"]);
    }
}
