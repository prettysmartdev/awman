//! The instance owns a launch plan. Agent-specific option translation belongs
//! to the subsequent compatibility step; the runtime accepts a neutral plan.
use super::{driver::*, paths::BuiltinPaths};
use crate::engine::{
    agent_runtime::{
        execution::{AgentExecution, AgentHandlePreview, AgentInstance},
        frontend::AgentFrontend,
    },
    error::EngineError,
};
use std::sync::Arc;
pub struct LaunchPlan {
    pub spec: SandboxSpec,
    pub command: ExecRequest,
    pub name: String,
    pub remove_on_exit: bool,
    pub persistent_stdin: bool,
    pub seeded_prompt: Option<String>,
    pub leases: Vec<crate::engine::credential_refresh::CredentialLease>,
    pub image_lease: std::fs::File,
}
pub struct Instance {
    pub driver: Arc<dyn SandboxDriver>,
    pub paths: BuiltinPaths,
    pub owner: String,
    pub plan: LaunchPlan,
}
impl AgentInstance for Instance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.plan.spec.name.clone(),
            name: self.plan.name.clone(),
            image: self.plan.spec.image.clone(),
        }
    }
    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        super::blocking::BlockingOperation::run(move || self.run(frontend))
    }
}
impl Instance {
    fn run(
        self: Box<Self>,
        mut frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        frontend.report_status(crate::engine::agent_runtime::frontend::AgentStatus::Starting);
        let id = self.plan.spec.name.clone();
        let _image_lease = self.plan.image_lease;
        // This owner-scoped guard covers partial creates, boot errors and unwinds.
        // A dropped spawn_blocking waiter cannot abort boot: its eventual
        // AgentExecution is dropped and its cancel handle owns normal cleanup.
        let mut launch_cleanup = LaunchCleanup {
            driver: self.driver.clone(),
            id: id.clone(),
            owner: self.owner.clone(),
            armed: true,
        };
        self.driver.create(self.plan.spec)?;
        let mut request = self.plan.command;
        // Capture channels once and pass them directly to the bridge.
        let io = frontend.take_io();
        request.tty = io.initial_size;
        request.stdin = true;
        let persistent = self.plan.persistent_stdin || request.tty.is_some();
        let session = self.driver.exec(&id, request)?;
        let handle = self.driver.get(&id)?.handle;
        let path = self.paths.attach(&id, &self.owner);
        let cleanup = Some(super::exec_bridge::CleanupTarget {
            driver: self.driver.clone(),
            owner: self.owner.clone(),
            remove_on_exit: self.plan.remove_on_exit,
        });
        let execution =
            super::exec_bridge::ExecBridge::new(super::exec_bridge::ExecBridgeOptions {
                handle,
                session,
                cleanup,
                attach_path: Some(path),
                persistent_stdin: persistent,
                seeded_prompt: self.plan.seeded_prompt,
                leases: self.plan.leases,
                io,
            })
            .run(frontend)?;
        launch_cleanup.armed = false;
        Ok(execution)
    }
}

struct LaunchCleanup {
    driver: Arc<dyn SandboxDriver>,
    id: String,
    owner: String,
    armed: bool,
}
impl Drop for LaunchCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ =
                self.driver
                    .stop_owned(&self.id, &self.owner, std::time::Duration::from_secs(2));
        }
    }
}
