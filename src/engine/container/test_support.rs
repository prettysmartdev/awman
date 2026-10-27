use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;

use super::backend::ContainerBackend;
use super::options::ResolvedContainerOptions;
use super::runtime::{ContainerImageInfo, CONTAINER_CAPABILITIES};
use super::ContainerRuntime;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::background::ExecOutput;
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, AgentStats, CancelHandle,
    ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentStatus};
use crate::engine::agent_runtime::{Capabilities, ImageImportRequest, ImportedImage};
use crate::engine::container::background::BackgroundLaunchRequest;
use crate::engine::error::EngineError;

const DEFAULT_GENERATED_WORKFLOW: &str =
    "[[steps]]\nname = \"generated\"\nagent = \"claude\"\nprompt = \"work\"\n";

pub(crate) type HeldFrontends = Arc<Mutex<Vec<Box<dyn AgentFrontend>>>>;

pub(crate) fn holding_runtime(frontends: HeldFrontends) -> Arc<ContainerRuntime> {
    Arc::new(ContainerRuntime::with_backend(Arc::new(HoldingBackend {
        frontends,
        notices: Default::default(),
        workflow_toml: None,
        on_exec: None,
        on_build: None,
        on_start: None,
    })))
}

/// Called with each command the holding backend is asked to run in a
/// background container (the phase steps of a `run_shell` setup or teardown).
pub(crate) type ExecHook = Arc<dyn Fn(&str) + Send + Sync>;

/// As [`holding_runtime`], but a dynamic-workflow leader "writes" `workflow_toml`
/// instead of the one-step default, and every background `exec` is passed to
/// `on_exec`.
pub(crate) fn holding_runtime_with_workflow(
    frontends: HeldFrontends,
    workflow_toml: &str,
    on_exec: ExecHook,
) -> Arc<ContainerRuntime> {
    Arc::new(ContainerRuntime::with_backend(Arc::new(HoldingBackend {
        frontends,
        notices: Default::default(),
        workflow_toml: Some(workflow_toml.to_string()),
        on_exec: Some(on_exec),
        on_build: None,
        on_start: None,
    })))
}

pub(crate) type BuildHook =
    Arc<dyn Fn(&ResolvedContainerOptions) -> Result<(), EngineError> + Send + Sync>;

pub(crate) type StartHook = Arc<dyn Fn() -> Result<(), EngineError> + Send + Sync>;

pub(crate) fn holding_runtime_with_hooks(
    frontends: HeldFrontends,
    workflow: &str,
    on_exec: ExecHook,
    on_build: BuildHook,
    on_start: Option<StartHook>,
) -> Arc<ContainerRuntime> {
    Arc::new(ContainerRuntime::with_backend(Arc::new(HoldingBackend {
        frontends,
        workflow_toml: Some(workflow.into()),
        on_exec: Some(on_exec),
        on_build: Some(on_build),
        on_start,
        notices: Default::default(),
    })))
}

#[derive(Default)]
pub(super) struct HoldingBackend {
    frontends: HeldFrontends,
    /// Returned (once) by `take_notices`.
    pub(super) notices: Mutex<Vec<String>>,
    /// The workflow a dynamic leader writes; `None` writes `DEFAULT_GENERATED_WORKFLOW`.
    workflow_toml: Option<String>,
    on_exec: Option<ExecHook>,
    on_build: Option<BuildHook>,
    on_start: Option<StartHook>,
}

struct HoldingInstance {
    frontends: HeldFrontends,
    on_start: Option<StartHook>,
}

struct ImmediateExecution;

impl ExecutionBackend for ImmediateExecution {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        let now = Utc::now();
        Ok(AgentExitInfo {
            exit_code: 0,
            signal: None,
            started_at: now,
            ended_at: now,
        })
    }

    fn cancel(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::new(|| Ok(()))
    }
}

impl AgentInstance for HoldingInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: "held-id".into(),
            name: "held-container".into(),
            image: "held-image".into(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        if let Some(hook) = &self.on_start {
            hook()?;
        }
        frontend.report_status(AgentStatus::Running {
            container_name: "held-container".into(),
        });
        let _io = frontend.take_io();
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "held runtime message".into(),
        });
        self.frontends.lock().unwrap().push(frontend);
        let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
        Ok(AgentExecution::new(
            AgentHandle {
                id: "held-id".into(),
                name: "held-container".into(),
                image_tag: "held-image".into(),
                started_at: Utc::now(),
            },
            Box::new(ImmediateExecution),
            Arc::new(stuck_tx),
            None,
        ))
    }
}

impl ContainerBackend for HoldingBackend {
    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }
    fn name(&self) -> &'static str {
        "holding-container"
    }
    fn display_name(&self) -> &'static str {
        "Holding container"
    }
    fn capabilities(&self) -> &'static Capabilities {
        &CONTAINER_CAPABILITIES
    }
    fn reattach_after_owner_exit(&self) -> bool {
        false
    }
    fn is_available(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        if let Some(hook) = &self.on_build {
            hook(&options)?;
        }
        let squad_workspace = options
            .overlays
            .iter()
            .find(|overlay| overlay.container_path == Path::new("/awman/context/workflow"));
        let squad_run = options.overlays.iter().find(|overlay| {
            overlay.container_path
                == Path::new(crate::data::fs::squad_verdict::RUN_DIR_CONTAINER_PATH)
        });
        if let (Some(workspace), Some(run)) = (squad_workspace, squad_run) {
            std::fs::write(
                workspace.host_path.join("workflow.toml"),
                "[[steps]]\nname = \"generated\"\nagent = \"claude\"\nprompt = \"work\"\n",
            )
            .map_err(|error| EngineError::Other(error.to_string()))?;
            std::fs::write(
                run.host_path
                    .join(crate::data::fs::squad_verdict::VERDICT_FILE_NAME),
                "{\"triggered\":true}",
            )
            .map_err(|error| EngineError::Other(error.to_string()))?;
        }
        for overlay in &options.overlays {
            if overlay.host_path.join("workflow-usage.md").exists() {
                std::fs::write(
                    overlay.host_path.join("workflow.toml"),
                    self.workflow_toml
                        .as_deref()
                        .unwrap_or(DEFAULT_GENERATED_WORKFLOW),
                )
                .map_err(|error| EngineError::Other(error.to_string()))?;
            }
        }
        Ok(Box::new(HoldingInstance {
            frontends: self.frontends.clone(),
            on_start: self.on_start.clone(),
        }))
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        Ok(Box::new(HoldingInstance {
            frontends: self.frontends.clone(),
            on_start: self.on_start.clone(),
        }))
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        Ok(AgentStats {
            name: "held-container".into(),
            cpu_percent: 0.0,
            memory_mb: 0.0,
        })
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn remove_agent(&self, _: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(Some("/root".into()))
    }
    fn build_image(
        &self,
        _request: &crate::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn import_image(
        &self,
        _: &ImageImportRequest,
        _: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        Err(EngineError::Other(
            "import unsupported in holding backend".into(),
        ))
    }
    fn list_dangling_images(&self) -> Result<Vec<ContainerImageInfo>, EngineError> {
        Ok(vec![])
    }
    fn remove_image(&self, _: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn start_background(&self, _: &BackgroundLaunchRequest<'_>) -> Result<String, EngineError> {
        Ok("held-background".into())
    }
    fn exec_in_background(
        &self,
        _: &str,
        command: &str,
        _: &str,
        _: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        if let Some(on_exec) = &self.on_exec {
            on_exec(command);
        }
        Ok(ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }
    fn exec_in_background_streaming(
        &self,
        request: &crate::engine::container::background::BackgroundExecRequest<'_>,
        _: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        let id = request.id;
        let command = request.command;
        let working_dir = request.working_dir;
        let env = request.env;

        self.exec_in_background(id, command, working_dir, env)
    }
    fn stop_and_remove(&self, _: &str) -> Result<(), EngineError> {
        Ok(())
    }
}
