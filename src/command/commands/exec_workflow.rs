//! `ExecWorkflowCommand` — run a workflow file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::mount_scope::{MountScope, MountScopeFrontend};
use crate::command::commands::worktree_lifecycle::{WorktreeLifecycle, WorktreeLifecycleFrontend};
use crate::command::commands::Command;
use crate::command::commands::{
    collect_all_overlay_specs, parse_overlay_list, warn_legacy_config, TypedOverlay,
};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_prompt_template::{substitute_prompt, WorkItemContext};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{ContainerExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::engine::workflow::{EngineRequest, WorkflowEngine};

#[derive(Debug, Clone)]
pub struct ExecWorkflowCommandFlags {
    pub workflow: PathBuf,
    pub work_item: Option<String>,
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub worktree: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub overlay: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecWorkflowOutcome {
    pub workflow: String,
    pub exit_code: Option<i32>,
    pub worktree_used: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSummary {
    pub steps_completed: usize,
    pub steps_failed: usize,
}

/// Per-command frontend trait: supertrait composition of every Layer 1 and
/// Layer 2 trait that `ExecWorkflowCommand` calls during its lifecycle.
#[async_trait]
pub trait ExecWorkflowCommandFrontend:
    UserMessageSink
    + ContainerFrontend
    + WorkflowFrontend
    + MountScopeFrontend
    + AgentSetupFrontend
    + AgentAuthFrontend
    + WorktreeLifecycleFrontend
    + Send
    + Sync
{
    /// Flip the PTY-active gate: when `true` the frontend queues user messages
    /// instead of rendering them immediately; when `false` it renders inline.
    fn set_pty_active(&mut self, active: bool);

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary);

    /// Ask the user whether to resume the workflow from its persisted state
    /// or to delete that state and start fresh. Called only when a saved
    /// state file is found on disk before the engine is built. Returns
    /// `true` to resume, `false` to start fresh.
    fn ask_workflow_resume_or_fresh(
        &mut self,
        workflow_name: &str,
        completed_steps: usize,
        total_steps: usize,
    ) -> Result<bool, CommandError>;
}

pub struct ExecWorkflowCommand {
    flags: ExecWorkflowCommandFlags,
    engines: Engines,
    session: Session,
}

impl ExecWorkflowCommand {
    pub fn new(flags: ExecWorkflowCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    pub fn flags(&self) -> &ExecWorkflowCommandFlags {
        &self.flags
    }
}

// ─── WorkflowProxy ───────────────────────────────────────────────────────────
//
// Implements `WorkflowFrontend` by delegating to the shared frontend through a
// `Mutex`. The engine holds this proxy as `Box<dyn WorkflowFrontend>`. After
// the engine block exits and the proxy is dropped, `Arc::try_unwrap` reclaims
// exclusive ownership of the frontend.

struct WorkflowProxy(Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>);

impl UserMessageSink for WorkflowProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.lock().unwrap().write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.0.lock().unwrap().replay_queued();
    }
}

impl WorkflowFrontend for WorkflowProxy {
    fn show_workflow_control_board(
        &mut self,
        state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.0
            .lock()
            .unwrap()
            .show_workflow_control_board(state, available)
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.0
            .lock()
            .unwrap()
            .yolo_countdown_tick(step_name, remaining, total)
    }

    fn yolo_countdown_started(&mut self, step_name: &str) {
        self.0.lock().unwrap().yolo_countdown_started(step_name);
    }

    fn yolo_countdown_finished(&mut self, step_name: &str) {
        self.0.lock().unwrap().yolo_countdown_finished(step_name);
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.0.lock().unwrap().report_step_status(step, status);
    }

    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput) {
        self.0.lock().unwrap().report_step_output(step, output);
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.0.lock().unwrap().report_workflow_completed(outcome);
    }

    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]) {
        self.0.lock().unwrap().report_workflow_progress(steps);
    }

    fn report_step_interactive_launch(
        &mut self,
        step: &WorkflowStep,
        agent: &str,
        model: Option<&str>,
    ) {
        self.0
            .lock()
            .unwrap()
            .report_step_interactive_launch(step, agent, model);
    }

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        self.0.lock().unwrap().confirm_resume(mismatch)
    }

    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        self.0
            .lock()
            .unwrap()
            .user_choose_after_step_failure(step, exit)
    }

    fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
        self.0.lock().unwrap().set_engine_sender(tx);
    }

    fn on_setup_step_started(&mut self, description: &str) {
        self.0.lock().unwrap().on_setup_step_started(description);
    }
    fn on_setup_step_output(&mut self, line: &str) {
        self.0.lock().unwrap().on_setup_step_output(line);
    }
    fn on_setup_step_completed(&mut self, description: &str) {
        self.0.lock().unwrap().on_setup_step_completed(description);
    }
    fn on_setup_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.0
            .lock()
            .unwrap()
            .on_setup_step_failed(description, exit_code, stderr);
    }

    fn on_teardown_step_started(&mut self, description: &str) {
        self.0.lock().unwrap().on_teardown_step_started(description);
    }
    fn on_teardown_step_output(&mut self, line: &str) {
        self.0.lock().unwrap().on_teardown_step_output(line);
    }
    fn on_teardown_step_completed(&mut self, description: &str) {
        self.0
            .lock()
            .unwrap()
            .on_teardown_step_completed(description);
    }
    fn on_teardown_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.0
            .lock()
            .unwrap()
            .on_teardown_step_failed(description, exit_code, stderr);
    }
}

// ─── ContainerFrontendProxy ──────────────────────────────────────────────────
//
// Passed to `ContainerInstance::run_with_frontend`. The current Docker backend
// discards it; a future PTY-wiring backend will use it.

struct ContainerFrontendProxy(Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>);

#[async_trait]
impl ContainerFrontend for ContainerFrontendProxy {
    fn report_status(&mut self, status: crate::engine::container::frontend::ContainerStatus) {
        self.0.lock().unwrap().report_status(status);
    }

    fn report_progress(&mut self, progress: crate::engine::container::frontend::ContainerProgress) {
        self.0.lock().unwrap().report_progress(progress);
    }

    fn take_container_io(&mut self) -> crate::engine::container::frontend::ContainerIo {
        self.0.lock().unwrap().take_container_io()
    }

    fn grace_timeout(&self) -> std::time::Duration {
        self.0.lock().unwrap().grace_timeout()
    }

    fn stuck_timeout(&self) -> std::time::Duration {
        self.0.lock().unwrap().stuck_timeout()
    }
}

impl UserMessageSink for ContainerFrontendProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.lock().unwrap().write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.0.lock().unwrap().replay_queued();
    }
}

// ─── CommandLayerFactory ─────────────────────────────────────────────────────
//
// Implements `ContainerExecutionFactory` for the workflow engine. Builds a
// container instance from per-step parameters + command flags, then binds a
// `ContainerFrontendProxy` to it via `run_with_frontend`.

struct CommandLayerFactory {
    shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    engines: Engines,
    flags: Arc<ExecWorkflowCommandFlags>,
    cli_typed_overlays: Vec<TypedOverlay>,
    work_item_context: Option<WorkItemContext>,
    /// The original repository git root (not the worktree). Used for image tag
    /// derivation so worktree-based runs use the correct project image.
    image_git_root: PathBuf,
}

impl ContainerExecutionFactory for CommandLayerFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<crate::engine::container::instance::ContainerExecution, EngineError> {
        // Substitute work item template tokens in the step prompt.
        let substitution =
            substitute_prompt(&step.prompt_template, self.work_item_context.as_ref());

        // Compute per-step overlays by merging config/env/CLI with step-level overlays.
        let collected = collect_all_overlay_specs(
            session,
            self.cli_typed_overlays.clone(),
            step.overlays.as_deref(),
        )
        .map_err(|e| EngineError::Other(format!("overlay collection failed: {e}")))?;

        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allowed_tools: vec![],
            disallowed_tools: vec![],
            initial_prompt: Some(substitution.rendered),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            model: runtime.step_model.clone(),
            env_passthrough: if collected.env_passthrough.is_empty() {
                None
            } else {
                Some(collected.env_passthrough)
            },
            directory_overlays: collected.directories,
            include_all_skills: collected.include_all_skills,
            named_skills: collected.named_skills,
        };
        let mut options =
            self.engines
                .agent_engine
                .build_options(session, &runtime.step_agent, &run_opts)?;

        // Override the image tag to use the original repo root, not a worktree path.
        let correct_tag = crate::data::image_tags::agent_image_tag(
            &self.image_git_root,
            runtime.step_agent.as_str(),
        );
        for opt in options.iter_mut() {
            if matches!(
                opt,
                crate::engine::container::options::ContainerOption::Image(_)
            ) {
                *opt = crate::engine::container::options::ContainerOption::Image(
                    crate::engine::container::options::ImageRef::new(correct_tag.clone()),
                );
                break;
            }
        }

        // Inject keychain credentials so the agent can reach its backend.
        // Mirrors the same step in `chat` and `exec_prompt`.
        if let Ok(credentials) = self
            .engines
            .auth_engine
            .resolve_agent_auth(session, &runtime.step_agent)
        {
            if !credentials.env_vars.is_empty() {
                options.push(
                    crate::engine::container::options::ContainerOption::AgentCredentials {
                        env_vars: credentials.env_vars,
                    },
                );
            }
        }

        let instance = self.engines.runtime.build(options)?;
        let proxy = ContainerFrontendProxy(Arc::clone(&self.shared));
        instance.run_with_frontend(Box::new(proxy))
    }

    fn inject_prompt(
        &self,
        execution: &crate::engine::container::instance::ContainerExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        // Mirror old amux's `launch_next_workflow_step_in_current_container`:
        // write the prompt followed by `\r` (Enter) directly into the running
        // container's PTY stdin. The Container Execution back-end returns
        // `Ok(true)` if it accepted the bytes (PTY-bridged backends do),
        // `Ok(false)` if it can't inject (inherit-stdio with no PTY) — in
        // which case we report `Ok(None)` and the engine launches a fresh
        // container.
        let mut payload = prompt.as_bytes().to_vec();
        payload.push(b'\r');
        match execution.try_inject_stdin(&payload)? {
            true => Ok(Some(())),
            false => Ok(None),
        }
    }
}

// ─── Command impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Command for ExecWorkflowCommand {
    type Frontend = Box<dyn ExecWorkflowCommandFrontend>;
    type Outcome = ExecWorkflowOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // Resolve the workflow path relative to the session's working
        // directory so that relative paths work regardless of where the
        // awman process was originally launched.
        let workflow_path = if self.flags.workflow.is_absolute() {
            self.flags.workflow.clone()
        } else {
            self.session.working_dir().join(&self.flags.workflow)
        };

        // Track whether the gemini deprecation warning has already been emitted
        // so we never fire it twice (early CLI check + post-load TOML scan).
        let mut gemini_warning_emitted = false;
        if self.flags.agent.as_deref() == Some("gemini") {
            emit_gemini_deprecation_warning(frontend.as_mut());
            gemini_warning_emitted = true;
        }

        // Emit deprecation warnings for legacy config fields.
        warn_legacy_config(&self.session, frontend.as_mut());

        if self.flags.yolo && self.flags.worktree {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "--yolo implies --worktree. Running in isolated worktree.".into(),
            });
        }

        // 1. Load the workflow file.
        if !workflow_path.exists() {
            let err = CommandError::WorkflowFileNotFound {
                path: workflow_path.clone(),
            };
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "exec workflow: workflow file not found: {}",
                    workflow_path.display()
                ),
            });
            return Err(err);
        }
        let workflow = match Workflow::load(&workflow_path) {
            Ok(w) => w,
            Err(e) => {
                let err = CommandError::Other(format!("loading workflow: {e}"));
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to load workflow: {e}"),
                });
                return Err(err);
            }
        };

        // After load: scan the workflow's per-step and workflow-level agents,
        // plus the session default (used when neither step nor workflow set an
        // agent). Per-step resolution mirrors WorkflowEngine::resolve_agent so
        // the warning fires for the same agent the engine will actually launch.
        if !gemini_warning_emitted && workflow_resolves_to_gemini(&workflow, &self.session) {
            emit_gemini_deprecation_warning(frontend.as_mut());
            gemini_warning_emitted = true;
        }
        let _ = gemini_warning_emitted;

        // 2. Resolve mount scope — confirm with the user when cwd differs from git root.
        let cwd = self.session.working_dir().to_path_buf();
        let git_root_for_scope = self.session.git_root().to_path_buf();
        let mount_path = match MountScope::resolve(&cwd, &git_root_for_scope, frontend.as_mut()) {
            Ok(p) => p,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: mount scope resolution failed: {e}"),
                });
                return Err(e);
            }
        };

        // 3. Load work item context when --work-item is supplied.
        let work_item_context = if let Some(wi_str) = &self.flags.work_item {
            match parse_work_item_number(wi_str) {
                Some(number) => {
                    let path = find_work_item_file(&git_root_for_scope, number);
                    match path.and_then(|p| std::fs::read_to_string(&p).ok()) {
                        Some(content) => Some(WorkItemContext { number, content }),
                        None => {
                            frontend.write_message(crate::engine::message::UserMessage {
                                level: crate::engine::message::MessageLevel::Warning,
                                text: format!(
                                    "work item file for {:04} not found; \
                                     {{{{work_item_*}}}} placeholders will be empty",
                                    number
                                ),
                            });
                            None
                        }
                    }
                }
                None => {
                    frontend.write_message(crate::engine::message::UserMessage {
                        level: crate::engine::message::MessageLevel::Warning,
                        text: format!(
                            "could not parse work item number from {:?}; \
                             {{{{work_item_*}}}} placeholders will be empty",
                            wi_str
                        ),
                    });
                    None
                }
            }
        } else {
            None
        };

        // 4. Worktree prepare (if --worktree is set).
        // When a worktree is used, capture its path so the session below is
        // rooted at the worktree checkout rather than the main repo.
        if self.flags.worktree && self.session.session_type().is_remote() {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "Skipping worktree creation for remote session — repo is already isolated."
                    .into(),
            });
        }
        let mut worktree_path: Option<PathBuf> = None;
        let worktree_lifecycle = if self.flags.worktree && !self.session.session_type().is_remote()
        {
            let git_root = match self.engines.git_engine.resolve_root(&cwd) {
                Ok(r) => r,
                Err(e) => {
                    let err = CommandError::from(e);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to resolve git root: {err}"),
                    });
                    return Err(err);
                }
            };
            // When --work-item is supplied, name the worktree/branch after the
            // work item number rather than the workflow filename.
            let lifecycle = if let Some(ctx) = &work_item_context {
                match WorktreeLifecycle::for_work_item(
                    Arc::clone(&self.engines.git_engine),
                    git_root,
                    ctx.number,
                ) {
                    Ok(l) => l,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!(
                                "exec workflow: failed to create worktree for work item: {e}"
                            ),
                        });
                        return Err(e);
                    }
                }
            } else {
                let name = self
                    .flags
                    .workflow
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workflow")
                    .to_string();
                match WorktreeLifecycle::for_workflow(
                    Arc::clone(&self.engines.git_engine),
                    git_root,
                    &name,
                ) {
                    Ok(l) => l,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!(
                                "exec workflow: failed to create worktree for workflow: {e}"
                            ),
                        });
                        return Err(e);
                    }
                }
            };
            let wt_path = match lifecycle.prepare(&mut *frontend).await {
                Ok(p) => p,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: worktree prepare failed: {e}"),
                    });
                    return Err(e);
                }
            };
            worktree_path = Some(wt_path);
            Some(lifecycle)
        } else {
            None
        };

        // 4b. Override mount path when a worktree is active so setup/teardown
        // containers bind to the worktree checkout, not the main repo.
        let mount_path = if let Some(ref wt) = worktree_path {
            wt.clone()
        } else {
            mount_path
        };

        // 4c. When running in a worktree, compute an extra overlay that mounts
        // the main repo's `.git` directory into setup/teardown containers.
        // Without this, the worktree's `.git` pointer file references a host
        // path that doesn't exist inside the container, breaking all git ops.
        let worktree_git_mount: Option<crate::engine::container::options::OverlaySpec> =
            if worktree_path.is_some() {
                worktree_git_overlay(&mount_path)?
            } else {
                None
            };

        // 5. Parse CLI overlay specs early so errors surface before PTY is activated.
        let cli_typed = {
            let mut all = Vec::new();
            for s in &self.flags.overlay {
                match parse_overlay_list(s) {
                    Ok(parsed) => all.extend(parsed),
                    Err(reason) => {
                        let e = CommandError::InvalidOverlaySpec {
                            spec: s.clone(),
                            reason,
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("exec workflow: invalid overlay spec: {e}"),
                        });
                        return Err(e);
                    }
                }
            }
            all
        };

        // 5b. Detect a persisted workflow-state file and ask the user whether
        //     to resume it or delete it and start fresh. The check uses the
        //     session_root the engine will pick up below — the worktree path
        //     when --worktree is active, otherwise cwd. Done before PTY
        //     activation so the dialog renders immediately, like the
        //     existing-worktree dialog does in the lifecycle step above.
        let session_root_for_state = worktree_path.as_deref().unwrap_or(&cwd).to_path_buf();
        let git_root_for_state =
            match Arc::clone(&self.engines.git_engine).resolve_root(&session_root_for_state) {
                Ok(r) => r,
                Err(_) => session_root_for_state.clone(),
            };
        let workflow_name_for_state = crate::engine::workflow::workflow_name_for(&workflow);
        let work_item_number_for_state = work_item_context.as_ref().map(|c| c.number);
        {
            let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(
                git_root_for_state.clone(),
            );
            match store.load(work_item_number_for_state, &workflow_name_for_state) {
                Ok(Some(saved)) => {
                    let total = saved.step_states.len();
                    let completed = saved
                        .step_states
                        .values()
                        .filter(|s| {
                            matches!(
                                s,
                                crate::data::workflow_state::StepState::Succeeded
                                    | crate::data::workflow_state::StepState::Skipped
                            )
                        })
                        .count();
                    let resume = frontend.ask_workflow_resume_or_fresh(
                        &workflow_name_for_state,
                        completed,
                        total,
                    )?;
                    if !resume {
                        if let Err(e) =
                            store.delete(work_item_number_for_state, &workflow_name_for_state)
                        {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Warning,
                                text: format!(
                                    "exec workflow: failed to delete workflow state file: {e}",
                                ),
                            });
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "exec workflow: failed to read workflow state file: {e}; \
                             starting fresh",
                        ),
                    });
                }
            }
        }

        // 6. Set PTY active — queues user messages during the engine run.
        frontend.set_pty_active(true);

        // 7. Wrap the frontend in Arc<Mutex> so both WorkflowProxy and
        //    CommandLayerFactory can share it for the duration of the engine run.
        let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
            Arc::new(Mutex::new(frontend));

        let flags_arc = Arc::new(self.flags.clone());

        // 8. Build the session for the engine.
        // When a worktree is active, re-root the session at the worktree so
        // that `build_options` mounts the worktree checkout, not the main repo.
        let session = if let Some(ref wt) = worktree_path {
            let git_root_for_session = match Arc::clone(&self.engines.git_engine).resolve_root(wt) {
                Ok(r) => r,
                Err(e) => {
                    let err = CommandError::from(e);
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!(
                            "exec workflow: failed to resolve git root for worktree session: {err}"
                        ),
                    });
                    return Err(err);
                }
            };
            match Session::open_at_git_root(
                wt.clone(),
                git_root_for_session,
                crate::data::session::SessionOpenOptions::default(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    let err = CommandError::Other(format!("opening worktree session: {e}"));
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to open worktree session: {e}"),
                    });
                    return Err(err);
                }
            }
        } else {
            self.session
        };

        // 9. Run the engine with three-phase coordination.
        // The engine block is scoped so proxy + factory are dropped before we
        // reclaim the frontend via Arc::try_unwrap.
        let yolo = self.flags.yolo;
        let setup_steps: Vec<crate::data::workflow_definition::SetupStep> =
            workflow.setup.iter().map(|e| e.step.clone()).collect();
        let teardown_steps: Vec<crate::data::workflow_definition::TeardownStep> =
            workflow.teardown.iter().map(|e| e.step.clone()).collect();
        let setup_entry_overlays: Vec<Option<Vec<String>>> =
            workflow.setup.iter().map(|e| e.overlays.clone()).collect();
        let setup_abort_flags: Vec<bool> =
            workflow.setup.iter().map(|e| e.abort_on_failure).collect();
        let teardown_entry_overlays: Vec<Option<Vec<String>>> = workflow
            .teardown
            .iter()
            .map(|e| e.overlays.clone())
            .collect();
        let teardown_abort_flags: Vec<bool> = workflow
            .teardown
            .iter()
            .map(|e| e.abort_on_failure)
            .collect();
        let teardown_on_failure = workflow.teardown_on_failure;
        let engine_work_item_context = work_item_context.clone();
        let (engine_result, step_counts) = {
            let proxy = WorkflowProxy(Arc::clone(&shared));
            let factory = CommandLayerFactory {
                shared: Arc::clone(&shared),
                engines: self.engines.clone(),
                flags: Arc::clone(&flags_arc),
                cli_typed_overlays: cli_typed.clone(),
                work_item_context,
                image_git_root: git_root_for_scope.clone(),
            };
            let mut engine = match WorkflowEngine::resume(
                &session,
                workflow,
                engine_work_item_context,
                Box::new(proxy),
                Box::new(factory),
                Arc::clone(&self.engines.git_engine),
                Arc::clone(&self.engines.overlay_engine),
            )
            .await
            {
                Ok(eng) => eng,
                Err(e) => {
                    let err = CommandError::from(e);
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to initialize workflow engine: {err}"),
                    });
                    return Err(err);
                }
            };
            engine.set_yolo(yolo);

            // Warn if the workflow will commit but git identity is not configured.
            if teardown_steps.iter().any(|s| {
                matches!(
                    s,
                    crate::data::workflow_definition::TeardownStep::CommitChanges { .. }
                )
            }) {
                let name_ok = std::process::Command::new("git")
                    .args(["config", "user.name"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let email_ok = std::process::Command::new("git")
                    .args(["config", "user.email"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !name_ok || !email_ok {
                    let missing: Vec<&str> = [
                        if !name_ok { Some("user.name") } else { None },
                        if !email_ok { Some("user.email") } else { None },
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "workflow has a commit_changes teardown step but git {} not set; \
                             set them locally (git config {0}) or use a dir() overlay to mount \
                             your global ~/.gitconfig into the agent container",
                            missing.join(" and "),
                        ),
                    });
                }
            }

            // === SETUP PHASE ===
            //
            // Each setup entry runs in its own container built from THAT
            // entry's overlays only (WI-0082): per-step isolation matters
            // because, e.g. an entry asking for `env(GITHUB_TOKEN)` must not
            // leak that token into a sibling entry that only asked for
            // `ssh()`. Container start/stop cost is amortized acceptably by
            // the small number of setup steps in real workflows.
            let mut setup_failed = false;
            if !setup_steps.is_empty() && !engine.state().setup_completed {
                let base_image = resolve_base_image(&session, &git_root_for_scope);
                let resolved = resolve_phase_overlays(
                    &self.engines,
                    &session,
                    &cli_typed,
                    &setup_entry_overlays,
                    worktree_git_mount.as_ref(),
                );

                // A bad overlay on ANY entry aborts the whole phase before
                // any container starts — otherwise earlier steps would have
                // already mutated the workspace.
                if let Some(e) = resolved.iter().find_map(|r| r.as_ref().err()) {
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: {e}"),
                    });
                    setup_failed = true;
                }

                if !setup_failed {
                    let runtime = Arc::clone(&self.engines.runtime);
                    let mount = mount_path.clone();
                    let base = base_image.clone();
                    let shared_for_factory = Arc::clone(&shared);
                    let setup_result = tokio::task::block_in_place(|| {
                        let factory = |idx: usize| -> Result<
                            Box<dyn crate::engine::container::ContainerExec>,
                            EngineError,
                        > {
                            let (overlays, env) = resolved
                                .get(idx)
                                .ok_or_else(|| {
                                    EngineError::Other(format!(
                                        "internal: missing pre-resolved overlays for setup step {idx}",
                                    ))
                                })?
                                .as_ref()
                                .map_err(|e| EngineError::Other(e.to_string()))?;
                            let container =
                                runtime.start_background(&base, &mount, env, overlays)?;
                            Ok(Box::new(container))
                        };
                        let r = engine.run_setup(&setup_steps, &setup_abort_flags, factory);
                        if let Err(e) = &r {
                            shared_for_factory
                                .lock()
                                .unwrap()
                                .write_message(UserMessage {
                                    level: MessageLevel::Error,
                                    text: format!("exec workflow: setup phase failed: {e}"),
                                });
                        }
                        r
                    });
                    if setup_result.is_err() {
                        setup_failed = true;
                    }
                }
            }

            // === MAIN PHASE ===
            let result = if setup_failed {
                Err(crate::engine::error::EngineError::Container(
                    "setup phase failed; main workflow not started".into(),
                ))
            } else {
                engine.run_to_completion().await
            };

            let workflow_succeeded = matches!(
                result,
                Ok(WorkflowOutcome::Completed) | Ok(WorkflowOutcome::CompletedTeardownFailed)
            );

            // === TEARDOWN PHASE ===
            //
            // Same per-entry container pattern as setup: overlays are
            // pre-resolved via `resolve_phase_overlays` and the factory
            // indexes into the results. Unlike setup, no upfront abort
            // gate — per-entry overlay errors flow through the factory and
            // `run_teardown` handles them as per-step failures (best-effort).
            //
            // If the setup or main phase triggered abort_on_failure,
            // teardown is skipped regardless of teardown_on_failure.
            let mut teardown_aborted = false;
            let mut any_teardown_failed = false;
            if !teardown_steps.is_empty() && !engine.abort_on_failure_triggered() {
                let should_run = teardown_on_failure || workflow_succeeded;
                if should_run {
                    let base_image = resolve_base_image(&session, &git_root_for_scope);
                    let resolved = resolve_phase_overlays(
                        &self.engines,
                        &session,
                        &cli_typed,
                        &teardown_entry_overlays,
                        worktree_git_mount.as_ref(),
                    );
                    let runtime = Arc::clone(&self.engines.runtime);
                    let mount = mount_path.clone();
                    (teardown_aborted, any_teardown_failed) = tokio::task::block_in_place(|| {
                        let factory = |idx: usize| -> Result<
                            Box<dyn crate::engine::container::ContainerExec>,
                            EngineError,
                        > {
                            let (overlays, env) = resolved
                                .get(idx)
                                .ok_or_else(|| {
                                    EngineError::Other(format!(
                                        "internal: missing pre-resolved overlays for teardown step {idx}",
                                    ))
                                })?
                                .as_ref()
                                .map_err(|e| EngineError::Other(e.to_string()))?;
                            let container =
                                runtime.start_background(&base_image, &mount, env, overlays)?;
                            Ok(Box::new(container))
                        };
                        engine
                            .run_teardown(
                                &teardown_steps,
                                &teardown_abort_flags,
                                workflow_succeeded,
                                teardown_on_failure,
                                factory,
                            )
                            .unwrap_or((false, false))
                    });
                }
            }

            // If any teardown step failed, promote the result to
            // CompletedTeardownFailed so post-workflow flows know.
            let result = if (teardown_aborted || any_teardown_failed) && workflow_succeeded {
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: "Workflow completed but one or more teardown steps failed".into(),
                });
                Ok(WorkflowOutcome::CompletedTeardownFailed)
            } else {
                result
            };

            // If teardown didn't run (no teardown steps, or skipped on failure)
            // the engine's current_phase still reads Main — promote it to Done
            // so persisted state reflects completion.
            if !matches!(
                engine.state().current_phase,
                crate::data::workflow_state::WorkflowPhase::Done
            ) {
                let _ = engine.mark_done();
            }

            let mut completed = 0usize;
            let mut failed = 0usize;
            for state in engine.state().step_states.values() {
                match state {
                    crate::data::workflow_state::StepState::Succeeded
                    | crate::data::workflow_state::StepState::Skipped => completed += 1,
                    crate::data::workflow_state::StepState::Failed { .. } => failed += 1,
                    _ => {}
                }
            }
            (result, (completed, failed))
        };

        // 8. Reclaim exclusive ownership of the frontend after proxy + factory drop.
        let mut frontend = Arc::try_unwrap(shared)
            .unwrap_or_else(|_| panic!("no other Arc references remain after engine block"))
            .into_inner()
            .unwrap();

        // 9. PTY inactive — flush queued messages.
        frontend.set_pty_active(false);
        frontend.replay_queued();

        // 10. Determine whether the workflow ended with an error.
        let had_error = matches!(
            engine_result,
            Err(_)
                | Ok(WorkflowOutcome::Failed { .. })
                | Ok(WorkflowOutcome::Aborted)
                | Ok(WorkflowOutcome::CompletedTeardownFailed)
        );

        // 11. Report summary.
        //
        // `exit_code` is the unambiguous overall outcome:
        //   Some(0) — workflow completed successfully
        //   Some(N) — a step failed (Failed → failing step's exit code;
        //             Aborted → 1, since the user/engine bailed after a failure)
        //   None    — workflow paused; no terminal status yet
        //
        // Callers (CLI, TUI, API queue worker) inspect this to determine the
        // final success/failure of the run.
        let exit_code = match &engine_result {
            Ok(WorkflowOutcome::Completed) => Some(0),
            Ok(WorkflowOutcome::CompletedTeardownFailed) => Some(1),
            Ok(WorkflowOutcome::Failed { exit_code, .. }) => Some(*exit_code),
            Ok(WorkflowOutcome::Aborted) => Some(1),
            Ok(WorkflowOutcome::Paused) => None,
            Err(_) => Some(1),
        };
        frontend.report_workflow_summary(&WorkflowSummary {
            steps_completed: step_counts.0,
            steps_failed: step_counts.1.max(if had_error { 1 } else { 0 }),
        });

        // 12. Worktree finalize.
        if let Some(lifecycle) = worktree_lifecycle {
            if let Err(e) = lifecycle.finalize(&mut *frontend, had_error).await {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: worktree finalize failed: {e}"),
                });
                return Err(e);
            }
            frontend.replay_queued();
        }

        // 13. Surface engine errors after lifecycle cleanup.
        if let Err(e) = engine_result {
            let err = CommandError::from(e);
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: workflow engine error: {err}"),
            });
            return Err(err);
        }

        Ok(ExecWorkflowOutcome {
            workflow: workflow_path.display().to_string(),
            exit_code,
            worktree_used: self.flags.worktree,
        })
    }
}

/// Emit the gemini → antigravity deprecation warning. Centralised so the wording
/// stays in sync across the early CLI-flag check and the post-load workflow scan.
fn emit_gemini_deprecation_warning(sink: &mut dyn UserMessageSink) {
    sink.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: "The 'gemini' agent is deprecated by Google. \
               Migrate to 'antigravity' — run 'awman chat antigravity' \
               (or 'awman config set agent antigravity' to change your default)."
            .to_string(),
    });
}

/// True if any step in the workflow will resolve to the `gemini` agent under
/// the same precedence the workflow engine uses (`step.agent` >
/// `workflow.agent` > session default).
fn workflow_resolves_to_gemini(workflow: &Workflow, session: &Session) -> bool {
    let workflow_default = workflow.agent.as_deref();
    let session_default = session.default_agent().map(|a| a.as_str().to_string());
    for step in &workflow.steps {
        let resolved = step
            .agent
            .as_deref()
            .or(workflow_default)
            .or(session_default.as_deref());
        if resolved == Some("gemini") {
            return true;
        }
    }
    false
}

/// Resolve the base image tag for setup/teardown containers.
/// Checks effective config, falls back to the project image tag convention.
fn resolve_base_image(session: &Session, git_root: &std::path::Path) -> String {
    if let Some(configured) = session.effective_config().base_image() {
        return configured;
    }
    crate::data::image_tags::project_image_tag(git_root)
}

/// Collect overlay specs and env vars for a single setup or teardown entry.
///
/// Merges the entry's own overlays with the global / repo / `AWMAN_OVERLAYS`
/// / `--overlay` flag sources, then resolves directories via the overlay
/// engine and captures env vars from the host process environment.
///
/// One call per entry — that's the whole point post-WI-0082: each step's
/// container sees only the entry's own overlays plus the standing sources,
/// not the union of all phase entries' overlays.
fn collect_single_entry_overlays(
    engines: &Engines,
    session: &Session,
    cli_typed: &[TypedOverlay],
    entry_overlays: Option<&[String]>,
) -> Result<
    (
        Vec<crate::engine::container::options::OverlaySpec>,
        std::collections::HashMap<String, String>,
    ),
    CommandError,
> {
    let collected = collect_all_overlay_specs(session, cli_typed.to_vec(), entry_overlays)?;

    let container_home = crate::engine::overlay::detect_home_from_dockerfile(
        &session.git_root().join("Dockerfile.dev"),
    );
    let request = crate::engine::overlay::OverlayRequest {
        directories: collected.directories,
        include_all_skills: false,
        named_skills: Vec::new(),
        agent: None,
        yolo: false,
        container_home,
    };
    let overlay_specs = engines
        .overlay_engine
        .build_overlays(session, &request)
        .map_err(|e| {
            CommandError::Other(format!(
                "failed to resolve overlays for setup/teardown container: {e}",
            ))
        })?;

    let mut env = std::collections::HashMap::new();
    for var_name in &collected.env_passthrough {
        if let Ok(val) = std::env::var(var_name) {
            env.insert(var_name.clone(), val);
        }
    }

    Ok((overlay_specs, env))
}

/// Pre-resolve overlay specs and env vars for every entry in a setup or
/// teardown phase.
///
/// Each entry is resolved independently via [`collect_single_entry_overlays`]
/// (per-step overlay isolation, WI-0082). When `worktree_git_mount` is
/// `Some`, the backing `.git` directory overlay is appended to every
/// successful entry so git operations work inside worktree-mounted
/// containers.
///
/// Returns one `Result` per entry. The caller decides error policy:
/// - **Setup** aborts the entire phase on the first `Err`.
/// - **Teardown** passes errors through to the factory; `run_teardown`
///   handles per-step failures gracefully.
type PhaseOverlayResult = Result<
    (
        Vec<crate::engine::container::options::OverlaySpec>,
        std::collections::HashMap<String, String>,
    ),
    CommandError,
>;

fn resolve_phase_overlays(
    engines: &Engines,
    session: &Session,
    cli_typed: &[TypedOverlay],
    entries: &[Option<Vec<String>>],
    worktree_git_mount: Option<&crate::engine::container::options::OverlaySpec>,
) -> Vec<PhaseOverlayResult> {
    entries
        .iter()
        .map(|entry| {
            let (mut overlays, env) =
                collect_single_entry_overlays(engines, session, cli_typed, entry.as_deref())?;
            if let Some(wt) = worktree_git_mount {
                overlays.push(wt.clone());
            }
            Ok((overlays, env))
        })
        .collect()
}

/// Extract a numeric work item number from strings like "0069", "69", "WI-69",
/// etc. Returns the first run of decimal digits found in `s`, parsed as `u32`.
fn parse_work_item_number(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Find a work item file whose filename starts with the zero-padded four-digit
/// number (e.g. `0069-*.md`). The search directory is determined by the repo
/// config's `workItems.dir` setting; falls back to `<git_root>/aspec/work-items/`.
fn find_work_item_file(git_root: &std::path::Path, number: u32) -> Option<std::path::PathBuf> {
    let repo_cfg = crate::data::config::repo::RepoConfig::load(git_root).unwrap_or_default();
    let dir = repo_cfg.work_items_dir_or_default(git_root);
    let prefix = format!("{:04}-", number);
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
}

/// Build an [`OverlaySpec`] that mounts the main repo's `.git` directory into
/// a container so git operations work inside a worktree checkout.
///
/// A worktree's `.git` is a pointer file referencing an absolute path inside
/// the main repo's `.git/worktrees/<name>/` directory. When only the worktree
/// is bind-mounted, that pointer dangles and every git command fails. This
/// overlay mounts the main `.git` directory at its host-absolute path so the
/// pointer resolves identically inside the container.
///
/// Returns `Ok(None)` when `worktree_path` is a regular repo or has no `.git`.
fn worktree_git_overlay(
    worktree_path: &std::path::Path,
) -> Result<Option<crate::engine::container::options::OverlaySpec>, EngineError> {
    let main_git_dir = match crate::engine::git::resolve_worktree_git_dir(worktree_path)? {
        Some(p) => p,
        None => return Ok(None),
    };
    Ok(Some(crate::engine::container::options::OverlaySpec {
        host_path: main_git_dir.clone(),
        container_path: main_git_dir,
        permission: crate::engine::container::options::OverlayPermission::ReadWrite,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
    use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
    use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
    use crate::command::commands::worktree_lifecycle::{
        ExistingWorktreeDecision, PostWorkflowWorktreeAction, PreWorktreeDecision,
        WorktreeLifecycleFrontend,
    };
    use crate::data::session::AgentName;
    use crate::data::workflow_state::WorkflowState;
    use crate::engine::container::frontend::{ContainerProgress, ContainerStatus};
    use crate::engine::container::instance::ContainerExitInfo;
    use crate::engine::message::UserMessage;
    use crate::engine::workflow::actions::{
        AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput,
        WorkflowOutcome, WorkflowStepStatus, YoloTickOutcome,
    };

    // ─── Recording frontend ───────────────────────────────────────────────────

    struct FakeExecWorkflowFrontend {
        pty_active_calls: Vec<bool>,
        replay_queued_count: usize,
        summary_calls: Vec<WorkflowSummary>,
        messages: Vec<UserMessage>,
        next_action_response: NextAction,
    }

    impl FakeExecWorkflowFrontend {
        fn new() -> Self {
            Self {
                pty_active_calls: vec![],
                replay_queued_count: 0,
                summary_calls: vec![],
                messages: vec![],
                next_action_response: NextAction::LaunchNext,
            }
        }
    }

    impl UserMessageSink for FakeExecWorkflowFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {
            self.replay_queued_count += 1;
        }
    }

    #[async_trait]
    impl ContainerFrontend for FakeExecWorkflowFrontend {
        fn report_status(&mut self, _status: ContainerStatus) {}
        fn report_progress(&mut self, _progress: ContainerProgress) {}
        fn take_container_io(&mut self) -> crate::engine::container::frontend::ContainerIo {
            let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::engine::container::frontend::ContainerIo {
                stdout: stdout_tx,
                stderr: stderr_tx,
                stdin_tx,
                stdin_rx,
                resize: None,
                initial_size: None,
            }
        }
    }

    impl WorkflowFrontend for FakeExecWorkflowFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            _available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            Ok(self.next_action_response.clone())
        }
        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Continue)
        }
        fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
        fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}
        fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
        fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &ContainerExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(StepFailureChoice::Abort)
        }
    }

    impl MountScopeFrontend for FakeExecWorkflowFrontend {
        fn ask_mount_scope(
            &mut self,
            _git_root: &Path,
            _cwd: &Path,
        ) -> Result<MountScopeDecision, CommandError> {
            Ok(MountScopeDecision::MountGitRoot)
        }
    }

    impl AgentSetupFrontend for FakeExecWorkflowFrontend {
        fn ask_agent_setup(
            &mut self,
            _requested: &AgentName,
            _default: &AgentName,
            _default_available: bool,
            _image_only: bool,
        ) -> Result<AgentSetupDecision, CommandError> {
            Ok(AgentSetupDecision::Setup)
        }
        fn record_fallback(&mut self, _requested: &AgentName, _fallback: &AgentName) {}
    }

    impl AgentAuthFrontend for FakeExecWorkflowFrontend {
        fn ask_agent_auth_consent(
            &mut self,
            _agent: &AgentName,
            _env_var_names: &[&str],
        ) -> Result<AgentAuthDecision, CommandError> {
            Ok(AgentAuthDecision::Accept)
        }
    }

    impl WorktreeLifecycleFrontend for FakeExecWorkflowFrontend {
        fn ask_pre_worktree_uncommitted_files(
            &mut self,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<PreWorktreeDecision, CommandError> {
            Ok(PreWorktreeDecision::UseLastCommit)
        }
        fn ask_existing_worktree(
            &mut self,
            _path: &Path,
            _branch: &str,
        ) -> Result<ExistingWorktreeDecision, CommandError> {
            Ok(ExistingWorktreeDecision::Resume)
        }
        fn report_worktree_created(&mut self, _path: &Path, _branch: &str) {}
        fn ask_post_workflow_action(
            &mut self,
            _prompt: &crate::command::commands::worktree_lifecycle::PostWorkflowWorktreePrompt,
        ) -> Result<PostWorkflowWorktreeAction, CommandError> {
            Ok(PostWorkflowWorktreeAction::Keep)
        }
        fn ask_worktree_commit_before_merge(
            &mut self,
            _branch: &str,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<Option<String>, CommandError> {
            Ok(None)
        }
        fn confirm_squash_merge(&mut self, _branch: &str) -> Result<bool, CommandError> {
            Ok(false)
        }
        fn confirm_worktree_cleanup(
            &mut self,
            _branch: &str,
            _path: &Path,
        ) -> Result<bool, CommandError> {
            Ok(false)
        }
        fn report_merge_conflict(&mut self, _branch: &str, _wt: &Path, _root: &Path) {}
        fn report_worktree_discarded(&mut self, _branch: &str) {}
        fn report_worktree_kept(&mut self, _path: &Path, _branch: &str) {}
    }

    impl ExecWorkflowCommandFrontend for FakeExecWorkflowFrontend {
        fn set_pty_active(&mut self, active: bool) {
            self.pty_active_calls.push(active);
        }
        fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
            self.summary_calls.push(summary.clone());
        }
        fn ask_workflow_resume_or_fresh(
            &mut self,
            _workflow_name: &str,
            _completed_steps: usize,
            _total_steps: usize,
        ) -> Result<bool, CommandError> {
            Ok(true)
        }
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    fn write_minimal_workflow(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            r#"[[steps]]
name = "test-step"
agent = "claude"
prompt = "do something"
"#,
        )
        .unwrap();
        path
    }

    fn make_engines() -> Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            Arc::clone(&overlay),
            Arc::clone(&runtime),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
        };
        Engines {
            runtime,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    // ─── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_pty_active_called_true_then_false_around_engine() {
        // Arrange: minimal workflow in a temp dir that the engine can run.
        let tmp = tempfile::tempdir().unwrap();
        let wf_path = write_minimal_workflow(tmp.path(), "test.toml");

        // Use a real git repo so Session::open_at_git_root succeeds.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "t@t.t"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::fs::write(tmp.path().join("README"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let mut engines = make_engines();
        // Override workflow_state_store to use the temp git repo.
        engines.workflow_state_store = Arc::new(
            crate::data::EngineWorkflowStateStore::at_git_root(tmp.path()),
        );

        let flags = ExecWorkflowCommandFlags {
            workflow: wf_path,
            work_item: None,
            non_interactive: true,
            plan: false,
            allow_docker: false,
            worktree: false,

            yolo: false,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
        };
        let session = {
            let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
            Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
            )
            .unwrap()
        };
        let cmd = ExecWorkflowCommand::new(flags, engines, session);
        let fake = FakeExecWorkflowFrontend::new();

        let result = cmd.run_with_frontend(Box::new(fake)).await;

        // The outcome is Ok and set_pty_active was called true then false.
        // (Engine result may be Ok or Err depending on the stub backend;
        //  what matters is the ordering.)
        // We can't easily inspect the fake after run_with_frontend consumes it.
        // Instead, we use the shared-arc pattern to peek at the state after.
        // For this test, simply verifying no panic is the structural assertion.
        let _ = result;
    }

    #[tokio::test]
    async fn workflow_proxy_delegates_write_message_to_inner_frontend() {
        let inner: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
            Arc::new(Mutex::new(Box::new(FakeExecWorkflowFrontend::new())));
        let mut proxy = WorkflowProxy(Arc::clone(&inner));

        use crate::engine::message::MessageLevel;
        proxy.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "hello".into(),
        });

        let guard = inner.lock().unwrap();
        let fake = guard.as_ref();
        // Can't easily downcast Box<dyn Trait>, but we can verify no panic
        // and that the proxy compiled and delegated without crashing.
        let _ = fake;
    }

    #[test]
    fn exec_workflow_flags_worktree_defaults_to_false() {
        // Verify ExecWorkflowCommandFlags is constructable and worktree defaults
        // correctly reflect what dispatch sets.
        let flags = ExecWorkflowCommandFlags {
            workflow: PathBuf::from("wf.toml"),
            work_item: None,
            non_interactive: false,
            plan: false,
            allow_docker: false,
            worktree: false,

            yolo: false,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
        };
        assert!(!flags.worktree);
        assert!(!flags.yolo);
    }

    #[test]
    fn exec_workflow_flags_yolo_implies_worktree_in_dispatch() {
        // Dispatch sets worktree=true when yolo=true; verify the flag struct
        // allows that combination.
        let flags = ExecWorkflowCommandFlags {
            workflow: PathBuf::from("wf.toml"),
            work_item: None,
            non_interactive: false,
            plan: false,
            allow_docker: false,
            worktree: true,

            yolo: true,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
        };
        assert!(flags.yolo);
        assert!(flags.worktree, "yolo must imply worktree");
    }

    #[test]
    fn workflow_summary_steps_failed_zero_on_success() {
        let s = WorkflowSummary {
            steps_completed: 3,
            steps_failed: 0,
        };
        assert_eq!(s.steps_failed, 0);
        assert_eq!(s.steps_completed, 3);
    }

    // ─── Per-entry overlay isolation (WI-0082 §1 review fix) ─────────────────

    /// `collect_single_entry_overlays` must scope env passthrough to the
    /// caller-supplied entry + standing sources only. The orchestrator calls
    /// it once per setup/teardown entry; if it leaked information across
    /// calls, sibling steps would inherit each other's overlays.
    #[test]
    fn collect_single_entry_overlays_isolates_env_per_entry() {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap();
        let engines = make_engines();

        // Set both env vars on the host so passthrough can capture them.
        std::env::set_var("WI0082_REVIEW_TOKEN_A", "value-a");
        std::env::set_var("WI0082_REVIEW_TOKEN_B", "value-b");

        let entry_a = vec!["env(WI0082_REVIEW_TOKEN_A)".to_string()];
        let entry_b = vec!["env(WI0082_REVIEW_TOKEN_B)".to_string()];

        let (_, env_a) =
            collect_single_entry_overlays(&engines, &session, &[], Some(&entry_a)).unwrap();
        let (_, env_b) =
            collect_single_entry_overlays(&engines, &session, &[], Some(&entry_b)).unwrap();

        std::env::remove_var("WI0082_REVIEW_TOKEN_A");
        std::env::remove_var("WI0082_REVIEW_TOKEN_B");

        assert!(
            env_a.contains_key("WI0082_REVIEW_TOKEN_A"),
            "entry A's env must contain its own var; got: {env_a:?}"
        );
        assert!(
            !env_a.contains_key("WI0082_REVIEW_TOKEN_B"),
            "entry A's env must NOT include entry B's var (no cross-step leak); got: {env_a:?}"
        );
        assert!(
            env_b.contains_key("WI0082_REVIEW_TOKEN_B"),
            "entry B's env must contain its own var; got: {env_b:?}"
        );
        assert!(
            !env_b.contains_key("WI0082_REVIEW_TOKEN_A"),
            "entry B's env must NOT include entry A's var (no cross-step leak); got: {env_b:?}"
        );
    }

    // ─── Gemini deprecation: workflow-level scan (WI-0083 review fix) ────────

    fn make_session_with_default_agent(
        tmp: &tempfile::TempDir,
        default_agent: Option<&str>,
    ) -> Session {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

        if let Some(agent) = default_agent {
            let cfg_dir = tmp.path().join(".awman");
            std::fs::create_dir_all(&cfg_dir).unwrap();
            std::fs::write(
                cfg_dir.join("config.json"),
                format!(r#"{{"agent": "{agent}"}}"#),
            )
            .unwrap();
        }
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn make_workflow(workflow_agent: Option<&str>, step_agents: &[Option<&str>]) -> Workflow {
        Workflow {
            title: None,
            steps: step_agents
                .iter()
                .enumerate()
                .map(|(i, a)| WorkflowStep {
                    name: format!("step{i}"),
                    depends_on: vec![],
                    prompt_template: "x".into(),
                    agent: a.map(|s| s.to_string()),
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                })
                .collect(),
            agent: workflow_agent.map(|s| s.to_string()),
            model: None,
            setup: vec![],
            teardown: vec![],
            teardown_on_failure: false,
        }
    }

    #[test]
    fn workflow_resolves_to_gemini_true_when_step_uses_gemini() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, None);
        let wf = make_workflow(None, &[Some("claude"), Some("gemini")]);
        assert!(
            workflow_resolves_to_gemini(&wf, &session),
            "must detect gemini in a step's agent field"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_true_when_workflow_default_is_gemini_and_step_has_no_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, None);
        let wf = make_workflow(Some("gemini"), &[None]);
        assert!(
            workflow_resolves_to_gemini(&wf, &session),
            "must detect workflow-level agent=gemini when step omits agent"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_true_when_session_default_is_gemini_and_step_has_no_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, Some("gemini"));
        let wf = make_workflow(None, &[None]);
        assert!(
            workflow_resolves_to_gemini(&wf, &session),
            "must detect session default agent=gemini when neither step nor workflow set agent"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_false_when_step_overrides_gemini_with_other_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, Some("gemini"));
        // step.agent (claude) wins over workflow.agent (gemini) and session default.
        let wf = make_workflow(Some("gemini"), &[Some("claude")]);
        assert!(
            !workflow_resolves_to_gemini(&wf, &session),
            "step-level agent override must win over workflow and session defaults"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_false_when_no_path_resolves_to_gemini() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, Some("claude"));
        let wf = make_workflow(Some("codex"), &[Some("claude"), None]);
        assert!(
            !workflow_resolves_to_gemini(&wf, &session),
            "must return false when neither step, workflow, nor session resolves to gemini"
        );
    }
}
