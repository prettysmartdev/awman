//! Interactive setup/teardown containers.
//!
//! A setup/teardown shell step normally runs headless: its command is
//! `exec`'d into a background container with no PTY and each output line is
//! streamed through `on_phase_step_output`. When a person is at the frontend
//! (`WorkflowFrontend::supports_interactive_recovery` — the CLI on a TTY and
//! the TUI, never the API server or the squad daemon), the step instead runs
//! in a foreground container the frontend's PTY attaches to, exactly like an
//! agent step. The container's shape, and the transcript it keeps, are the
//! runtime's ([`ContainerRuntime::build_phase_step`]); this module only wires
//! the command's frontend to it and reports the result to the workflow engine.
//!
//! The headless path's contract is kept: the full transcript comes back as
//! the step's [`ExecOutput`], so a failing step's `on_failure` agent still gets
//! the output in its failure file. A PTY merges the command's stdout and
//! stderr into one stream, so the transcript is returned as `stdout` and
//! `stderr` is left empty.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::SharedWorkflowFrontend;
use crate::engine::agent_runtime::background::{AgentExec, ExecOutput};
use crate::engine::agent_runtime::execution::AgentInstance;
use crate::engine::container::options::OverlaySpec;
use crate::engine::container::{ContainerRuntime, PhaseStepContainerSpec};
use crate::engine::error::EngineError;
use crate::engine::workflow::PhaseStepRef;

/// How long to wait, after the container exits, for the runtime to finish
/// reading the last of its output into the transcript. Reading normally
/// ends at the process's EOF; this only bounds a reader that never does.
const TRANSCRIPT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct PhaseContainerWorkspace {
    pub workdir: PathBuf,
    pub env: HashMap<String, String>,
    pub overlays: Vec<OverlaySpec>,
    pub builtin_launch: Option<crate::data::config::builtin_launch::BuiltinLaunchSettings>,
}

/// The setup and teardown factories share this launch boundary. Resolve the
/// session policy before choosing whether the step owns a foreground PTY.
pub(crate) struct PhaseContainerFactory<'a> {
    pub runtime: &'a Arc<ContainerRuntime>,
    pub frontend: &'a SharedWorkflowFrontend,
    pub agent_engine: &'a crate::engine::agent::AgentEngine,
    pub session: &'a crate::data::session::Session,
    pub image: &'a str,
    pub workdir: &'a std::path::Path,
    pub interactive: bool,
}
impl PhaseContainerFactory<'_> {
    pub fn build(
        &self,
        step: &PhaseStepRef,
        env: &HashMap<String, String>,
        overlays: &[OverlaySpec],
    ) -> Result<Box<dyn AgentExec>, EngineError> {
        let launch = self.agent_engine.builtin_launch_settings(self.session)?;
        if self.interactive {
            return Ok(Box::new(InteractivePhaseContainer::new(
                self.runtime.clone(),
                self.frontend.clone(),
                step,
                self.image,
                PhaseContainerWorkspace {
                    workdir: self.workdir.to_path_buf(),
                    env: env.clone(),
                    overlays: overlays.to_vec(),
                    builtin_launch: launch,
                },
            )));
        }
        Ok(Box::new(self.runtime.start_background(
            &crate::engine::container::background::BackgroundLaunchRequest {
                image: self.image,
                workdir: self.workdir,
                env,
                overlays,
                builtin_launch: launch.as_ref(),
            },
        )?))
    }
}

/// One setup/teardown step's container, not yet started. The engine asks the
/// phase's container factory for one of these per step, then calls
/// [`AgentExec::exec_streaming`] with the step's command, which launches the
/// container, blocks until it exits, and returns its transcript.
///
/// Carries the same image, workspace mount, overlays and `env()` passthrough
/// the headless path hands `ContainerRuntime::start_background`.
pub(crate) struct InteractivePhaseContainer {
    runtime: Arc<ContainerRuntime>,
    frontend: SharedWorkflowFrontend,
    step: PhaseStepRef,
    image: String,
    workspace: PhaseContainerWorkspace,
}

impl InteractivePhaseContainer {
    pub(crate) fn new(
        runtime: Arc<ContainerRuntime>,
        frontend: SharedWorkflowFrontend,
        step: &PhaseStepRef,
        image: &str,
        workspace: PhaseContainerWorkspace,
    ) -> Self {
        Self {
            runtime,
            frontend,
            step: step.clone(),
            image: image.to_string(),
            workspace,
        }
    }

    fn run(
        &self,
        command: &str,
        step_env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        let instance = self.runtime.build_phase_step(&PhaseStepContainerSpec {
            builtin_launch: self.workspace.builtin_launch.as_ref(),
            image: &self.image,
            workdir: &self.workspace.workdir,
            overlays: &self.workspace.overlays,
            env: &self.workspace.env,
            command,
            step_env,
        })?;
        self.run_instance(instance)
    }

    pub(crate) fn run_instance(
        &self,
        instance: Box<dyn AgentInstance>,
    ) -> Result<ExecOutput, EngineError> {
        self.frontend
            .lock()
            .unwrap()
            .report_phase_step_interactive_launch(self.step.kind, &self.step.description);

        let mut execution =
            match instance.run_with_frontend(self.frontend.clone().into_agent_frontend()) {
                Ok(e) => e,
                Err(e) => {
                    // `take_io` may already have put the terminal in raw mode (CLI)
                    // or readied a container window (TUI); hand it back.
                    self.report_exited(-1);
                    return Err(e);
                }
            };
        let transcript = execution.output_tail();

        let exit = tokio::runtime::Handle::current().block_on(execution.wait());
        drop(execution);
        self.report_exited(exit.as_ref().map(|e| e.exit_code).unwrap_or(-1));
        let exit = exit?;

        let stdout = match transcript {
            Some(tail) => {
                tail.wait_for_writers(TRANSCRIPT_FLUSH_TIMEOUT);
                tail.plain_text()
            }
            None => String::new(),
        };
        Ok(ExecOutput {
            stdout,
            stderr: String::new(),
            exit_code: exit.exit_code,
        })
    }

    fn report_exited(&self, exit_code: i32) {
        self.frontend
            .lock()
            .unwrap()
            .report_phase_step_container_exited(self.step.kind, exit_code);
    }
}

impl AgentExec for InteractivePhaseContainer {
    fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.run(command, env)
    }

    /// The output is on the user's screen already, in the PTY, so nothing is
    /// streamed line by line — `on_line` would only print it a second time.
    fn exec_streaming(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
        _on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        self.run(command, env)
    }
}
