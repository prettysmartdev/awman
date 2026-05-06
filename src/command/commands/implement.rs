//! `ImplementCommand` — top-level `amux implement WORK_ITEM`.
//!
//! Per spec §6.1, `implement` MUST remain a top-level command. Internally
//! the command may delegate to `ExecWorkflowCommand` (constructing a
//! synthetic single-step workflow when `--workflow` is absent).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::exec_workflow::WorkflowSummary;
use crate::command::commands::implement_prompts::render_default_prompt;
use crate::command::commands::mount_scope::{MountScope, MountScopeFrontend};
use crate::command::commands::{collect_all_overlay_specs, parse_overlay_spec};
use crate::command::commands::worktree_lifecycle::{WorktreeLifecycle, WorktreeLifecycleFrontend};
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::data::workflow_definition::{Workflow, WorkflowFormat, WorkflowStep};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{ContainerExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::engine::workflow::WorkflowEngine;

#[derive(Debug, Clone)]
pub struct ImplementCommandFlags {
    pub work_item: String,
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub workflow: Option<PathBuf>,
    pub worktree: bool,
    pub mount_ssh: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub overlay: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImplementOutcome {
    pub work_item: String,
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
    pub worktree_used: bool,
    pub workflow_used: Option<String>,
    pub synthetic_prompt: Option<String>,
}

/// Per-command frontend supertrait: identical I/O and lifecycle surface to
/// `ExecWorkflowCommandFrontend`, but with an implement-specific summary call.
#[async_trait]
pub trait ImplementCommandFrontend:
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
    fn set_pty_active(&mut self, active: bool);
    fn report_implement_summary(&mut self, summary: &WorkflowSummary);
}

pub struct ImplementCommand {
    flags: ImplementCommandFlags,
    engines: Engines,
}

impl ImplementCommand {
    pub fn new(flags: ImplementCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &ImplementCommandFlags {
        &self.flags
    }
}

// ─── WorkflowProxy ───────────────────────────────────────────────────────────

struct ImplementWorkflowProxy(Arc<Mutex<Box<dyn ImplementCommandFrontend>>>);

impl UserMessageSink for ImplementWorkflowProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.lock().unwrap().write_message(msg);
    }
    fn replay_queued(&mut self) {
        self.0.lock().unwrap().replay_queued();
    }
}

impl WorkflowFrontend for ImplementWorkflowProxy {
    fn user_choose_next_action(
        &mut self,
        state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.0.lock().unwrap().user_choose_next_action(state, available)
    }
    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        self.0.lock().unwrap().confirm_resume(mismatch)
    }
    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        self.0.lock().unwrap().user_choose_after_step_failure(step, exit)
    }
    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.0.lock().unwrap().report_step_status(step, status);
    }
    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput) {
        self.0.lock().unwrap().report_step_output(step, output);
    }
    fn report_step_stuck(&mut self, step: &WorkflowStep) {
        self.0.lock().unwrap().report_step_stuck(step);
    }
    fn report_step_unstuck(&mut self, step: &WorkflowStep) {
        self.0.lock().unwrap().report_step_unstuck(step);
    }
    fn yolo_countdown_tick(
        &mut self,
        remaining: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.0.lock().unwrap().yolo_countdown_tick(remaining)
    }
    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.0.lock().unwrap().report_workflow_completed(outcome);
    }
}

// ─── ContainerFrontendProxy ──────────────────────────────────────────────────

struct ImplementContainerFrontendProxy(Arc<Mutex<Box<dyn ImplementCommandFrontend>>>);

impl UserMessageSink for ImplementContainerFrontendProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.lock().unwrap().write_message(msg);
    }
    fn replay_queued(&mut self) {
        self.0.lock().unwrap().replay_queued();
    }
}

#[async_trait]
impl ContainerFrontend for ImplementContainerFrontendProxy {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        self.0.lock().unwrap().write_stdout(bytes)
    }
    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        self.0.lock().unwrap().write_stderr(bytes)
    }
    async fn read_stdin(&mut self, buf: &mut [u8]) -> Result<usize, EngineError> {
        // Inherit-stdio mode owns the host TTY directly during the container
        // run; this proxy is only consulted when the backend explicitly pipes
        // stdin through us. Read from the host's stdin via spawn_blocking so
        // we don't block the async runtime.
        let len = buf.len();
        let bytes = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut local = vec![0u8; len];
            match std::io::stdin().read(&mut local) {
                Ok(n) => {
                    local.truncate(n);
                    Ok::<Vec<u8>, std::io::Error>(local)
                }
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| EngineError::Container(format!("stdin task: {e}")))?
        .map_err(|e| EngineError::Container(format!("read stdin: {e}")))?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn report_status(
        &mut self,
        status: crate::engine::container::frontend::ContainerStatus,
    ) {
        self.0.lock().unwrap().report_status(status);
    }
    fn report_progress(
        &mut self,
        progress: crate::engine::container::frontend::ContainerProgress,
    ) {
        self.0.lock().unwrap().report_progress(progress);
    }
    fn resize_pty(&mut self, cols: u16, rows: u16) {
        self.0.lock().unwrap().resize_pty(cols, rows);
    }
    fn take_container_io(&mut self) -> Option<crate::engine::container::frontend::ContainerIo> {
        self.0.lock().unwrap().take_container_io()
    }
}

// ─── CommandLayerFactory ─────────────────────────────────────────────────────

struct ImplementCommandLayerFactory {
    shared: Arc<Mutex<Box<dyn ImplementCommandFrontend>>>,
    engines: Engines,
    flags: Arc<ImplementCommandFlags>,
    directory_overlays: Vec<crate::engine::overlay::DirectorySpec>,
}

impl ContainerExecutionFactory for ImplementCommandLayerFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<crate::engine::container::instance::ContainerExecution, EngineError> {
        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allowed_tools: vec![],
            disallowed_tools: vec![],
            initial_prompt: Some(step.prompt_template.clone()),
            allow_docker: self.flags.allow_docker,
            mount_ssh: self.flags.mount_ssh,
            non_interactive: self.flags.non_interactive,
            model: runtime.step_model.clone(),
            env_passthrough: Some(session.effective_config().env_passthrough()),
            directory_overlays: self.directory_overlays.clone(),
        };
        let options = self
            .engines
            .agent_engine
            .build_options(session, &runtime.step_agent, &run_opts)?;
        let instance = self.engines.runtime.build(options)?;
        let proxy = ImplementContainerFrontendProxy(Arc::clone(&self.shared));
        instance.run_with_frontend(Box::new(proxy))
    }

    fn inject_prompt(
        &self,
        _execution: &crate::engine::container::instance::ContainerExecution,
        _prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        // Documented contract per `ContainerExecutionFactory::inject_prompt`:
        // returning `Ok(None)` tells the workflow engine that this backend
        // doesn't support mid-session stdin re-injection, and the engine then
        // spins up a fresh container for the next step.
        //
        // `AgentMatrix::supports_stdin_injection` is `false` for every shipped
        // agent — none have been verified to keep state when re-prompted on
        // an existing stdin. Once an agent is verified to support this, flip
        // the matrix bit and wire `ContainerExecution::write_stdin` (currently
        // not exposed by Layer 1) on the same change.
        Ok(None)
    }
}

// ─── Command impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Command for ImplementCommand {
    type Frontend = Box<dyn ImplementCommandFrontend>;
    type Outcome = ImplementOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("implement: resolving work item {}", self.flags.work_item),
        });
        let synthetic_prompt = if self.flags.workflow.is_none() {
            Some(render_default_prompt(&self.flags.work_item))
        } else {
            None
        };
        let workflow_used = self.flags.workflow.as_ref().map(|p| p.display().to_string());

        // Load or construct workflow.
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: match &self.flags.workflow {
                Some(path) => format!("implement: loading workflow from {}", path.display()),
                None => "implement: constructing single-step workflow".into(),
            },
        });
        let workflow: Workflow = match &self.flags.workflow {
            Some(path) => match Workflow::load(path) {
                Ok(w) => w,
                Err(e) => {
                    let cmd_err = CommandError::Other(format!("loading workflow: {e}"));
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("implement: failed to load workflow: {e}"),
                    });
                    return Err(cmd_err);
                }
            },
            None => {
                let prompt = render_default_prompt(&self.flags.work_item);
                match Workflow::parse(
                    &format!(
                        "[[steps]]\nname = \"implement\"\nagent = \"claude\"\nprompt_template = {:?}\n",
                        prompt
                    ),
                    WorkflowFormat::Toml,
                ) {
                    Ok(w) => w,
                    Err(e) => {
                        let cmd_err = CommandError::Other(format!("building synthetic workflow: {e}"));
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("implement: failed to build synthetic workflow: {e}"),
                        });
                        return Err(cmd_err);
                    }
                }
            }
        };

        // Confirm mount scope when cwd differs from git root.
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."));
        let git_root_for_scope = self
            .engines
            .git_engine
            .resolve_root(&cwd)
            .unwrap_or_else(|_| cwd.clone());
        let _mount_path = match MountScope::resolve(&cwd, &git_root_for_scope, frontend.as_mut()) {
            Ok(p) => p,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("implement: mount scope resolution failed: {e}"),
                });
                return Err(e);
            }
        };

        // Worktree prepare.

        let worktree_lifecycle = if self.flags.worktree {
            let git_root = match self.engines.git_engine.resolve_root(&cwd) {
                Ok(r) => r,
                Err(e) => {
                    let cmd_err = CommandError::from(e);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("implement: failed to resolve git root for worktree: {cmd_err}"),
                    });
                    return Err(cmd_err);
                }
            };
            let lifecycle = match WorktreeLifecycle::for_work_item(
                Arc::clone(&self.engines.git_engine),
                git_root,
                parse_work_item_number(&self.flags.work_item),
            ) {
                Ok(l) => l,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("implement: worktree lifecycle creation failed: {e}"),
                    });
                    return Err(e);
                }
            };
            match lifecycle.prepare(&mut *frontend).await {
                Ok(_) => {}
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("implement: worktree prepare failed: {e}"),
                    });
                    return Err(e);
                }
            }
            Some(lifecycle)
        } else {
            None
        };

        // Parse CLI overlay specs before any async work so errors surface early.
        let cli_overlays = match self
            .flags
            .overlay
            .iter()
            .map(|s| {
                parse_overlay_spec(s).map_err(|reason| CommandError::InvalidOverlaySpec {
                    spec: s.clone(),
                    reason,
                })
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("implement: invalid overlay spec: {e}"),
                });
                return Err(e);
            }
        };

        frontend.set_pty_active(true);

        let shared: Arc<Mutex<Box<dyn ImplementCommandFrontend>>> =
            Arc::new(Mutex::new(frontend));

        let flags_arc = Arc::new(self.flags.clone());

        let git_root_for_session = match Arc::clone(&self.engines.git_engine).resolve_root(&cwd) {
            Ok(r) => r,
            Err(e) => {
                let cmd_err = CommandError::from(e);
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("implement: failed to resolve git root for session: {cmd_err}"),
                });
                return Err(cmd_err);
            }
        };
        let session = match Session::open_at_git_root(
            cwd,
            git_root_for_session,
            crate::data::session::SessionOpenOptions::default(),
        ) {
            Ok(s) => s,
            Err(e) => {
                let cmd_err = CommandError::Other(format!("opening session: {e}"));
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("implement: failed to open session: {e}"),
                });
                return Err(cmd_err);
            }
        };

        // Merge CLI overlays with config/env sources now that session is available.
        let directory_overlays = collect_all_overlay_specs(&session, cli_overlays);

        shared.lock().unwrap().write_message(UserMessage {
            level: MessageLevel::Info,
            text: "implement: launching agent container…".into(),
        });

        let (engine_result, step_counts) = {
            let proxy = ImplementWorkflowProxy(Arc::clone(&shared));
            let factory = ImplementCommandLayerFactory {
                shared: Arc::clone(&shared),
                engines: self.engines.clone(),
                flags: Arc::clone(&flags_arc),
                directory_overlays,
            };
            let mut engine = match WorkflowEngine::new(
                &session,
                workflow,
                Box::new(proxy),
                Box::new(factory),
                Arc::clone(&self.engines.git_engine),
                Arc::clone(&self.engines.overlay_engine),
            ) {
                Ok(eng) => eng,
                Err(e) => {
                    let cmd_err = CommandError::from(e);
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("implement: workflow engine creation failed: {cmd_err}"),
                    });
                    return Err(cmd_err);
                }
            };
            let result = engine.run_to_completion().await;
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

        // Reclaim exclusive frontend ownership.
        let mut frontend = Arc::try_unwrap(shared)
            .unwrap_or_else(|_| panic!("no other Arc references after engine block"))
            .into_inner()
            .unwrap();

        frontend.set_pty_active(false);

        if matches!(
            &engine_result,
            Err(_) | Ok(WorkflowOutcome::Failed { .. }) | Ok(WorkflowOutcome::Aborted)
        ) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "implement: step failed".into(),
            });
        } else {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "implement: step completed successfully".into(),
            });
        }

        frontend.replay_queued();

        let had_error = matches!(
            engine_result,
            Err(_) | Ok(WorkflowOutcome::Failed { .. }) | Ok(WorkflowOutcome::Aborted)
        );
        let exit_code = match &engine_result {
            Ok(WorkflowOutcome::Failed { exit_code, .. }) => Some(*exit_code),
            _ => None,
        };
        frontend.report_implement_summary(&WorkflowSummary {
            steps_completed: step_counts.0,
            steps_failed: step_counts.1.max(if had_error { 1 } else { 0 }),
        });

        if let Some(lifecycle) = worktree_lifecycle {
            match lifecycle.finalize(&mut *frontend, had_error).await {
                Ok(()) => {}
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("implement: worktree finalize failed: {e}"),
                    });
                    return Err(e);
                }
            }
            frontend.replay_queued();
        }

        match engine_result {
            Ok(_) => {}
            Err(e) => {
                let cmd_err = CommandError::from(e);
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("implement: workflow engine failed: {cmd_err}"),
                });
                return Err(cmd_err);
            }
        }

        Ok(ImplementOutcome {
            work_item: self.flags.work_item,
            agent: self.flags.agent,
            exit_code,
            worktree_used: self.flags.worktree,
            workflow_used,
            synthetic_prompt,
        })
    }
}

/// Parse a work item string like "0001" into a u32.
/// Falls back to 0 if parsing fails (graceful degradation).
fn parse_work_item_number(s: &str) -> u32 {
    s.trim_start_matches('0').parse::<u32>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_work_item_number_handles_leading_zeros() {
        assert_eq!(parse_work_item_number("0001"), 1);
        assert_eq!(parse_work_item_number("0042"), 42);
        assert_eq!(parse_work_item_number("100"), 100);
    }

    #[test]
    fn parse_work_item_number_returns_zero_on_non_numeric() {
        assert_eq!(parse_work_item_number("abc"), 0);
        assert_eq!(parse_work_item_number(""), 0);
    }

    #[test]
    fn implement_without_workflow_sets_synthetic_prompt() {
        let prompt = render_default_prompt("0042");
        assert!(
            prompt.contains("0042"),
            "synthetic prompt must contain the work item number"
        );
        assert!(!prompt.is_empty(), "synthetic prompt must not be empty");
    }

    #[test]
    fn implement_flags_worktree_false_by_default() {
        let flags = ImplementCommandFlags {
            work_item: "0001".into(),
            non_interactive: false,
            plan: false,
            allow_docker: false,
            workflow: None,
            worktree: false,
            mount_ssh: false,
            yolo: false,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
        };
        assert!(!flags.worktree);
    }

    #[test]
    fn implement_yolo_without_workflow_should_not_imply_worktree() {
        // Dispatch enforces: yolo + workflow → worktree; yolo without workflow → no worktree.
        let flags = ImplementCommandFlags {
            work_item: "0001".into(),
            non_interactive: false,
            plan: false,
            allow_docker: false,
            workflow: None,
            worktree: false,
            mount_ssh: false,
            yolo: true,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
        };
        assert!(flags.yolo);
        assert!(!flags.worktree, "yolo without workflow must NOT imply worktree");
    }
}
