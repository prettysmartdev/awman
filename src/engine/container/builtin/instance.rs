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
        mut frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        frontend.report_status(crate::engine::agent_runtime::frontend::AgentStatus::Starting);
        let id = self.plan.spec.name.clone();
        let _image_lease = self.plan.image_lease;
        self.driver.create(self.plan.spec)?;
        let mut request = self.plan.command;
        // Read I/O only once; this adapter gives the bridge the captured channels.
        let io = frontend.take_io();
        request.tty = io.initial_size;
        request.stdin = true;
        let persistent = self.plan.persistent_stdin || request.tty.is_some();
        let session = match self.driver.exec(&id, request) {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .driver
                    .stop_owned(&id, &self.owner, std::time::Duration::from_secs(2));
                return Err(e);
            }
        };
        let handle = match self.driver.get(&id) {
            Ok(summary) => summary.handle,
            Err(error) => {
                let _ = self
                    .driver
                    .stop_owned(&id, &self.owner, std::time::Duration::from_secs(2));
                return Err(error);
            }
        };
        let path = self.paths.attach(&id, &self.owner);
        let cleanup = Some((
            self.driver.clone(),
            self.owner.clone(),
            self.plan.remove_on_exit,
        ));
        match super::exec_bridge::run(
            handle,
            session,
            Box::new(CapturedFrontend {
                frontend,
                io: Some(io),
            }),
            cleanup,
            Some(path),
            persistent,
            super::exec_bridge::RunInput {
                seeded_prompt: self.plan.seeded_prompt,
                leases: self.plan.leases,
            },
        ) {
            Ok(execution) => Ok(execution),
            Err(error) => {
                let _ = self
                    .driver
                    .stop_owned(&id, &self.owner, std::time::Duration::from_secs(2));
                Err(error)
            }
        }
    }
}
struct CapturedFrontend {
    frontend: Box<dyn AgentFrontend>,
    io: Option<crate::engine::agent_runtime::frontend::AgentIo>,
}
impl crate::data::message::UserMessageSink for CapturedFrontend {
    fn write_message(&mut self, message: crate::data::message::UserMessage) {
        self.frontend.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.frontend.replay_queued();
    }
}
#[async_trait::async_trait]
impl AgentFrontend for CapturedFrontend {
    fn report_status(&mut self, status: crate::engine::agent_runtime::frontend::AgentStatus) {
        self.frontend.report_status(status);
    }
    fn report_progress(&mut self, progress: crate::engine::agent_runtime::frontend::AgentProgress) {
        self.frontend.report_progress(progress);
    }
    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        self.io.take().expect("I/O taken once")
    }
    fn grace_timeout(&self) -> std::time::Duration {
        self.frontend.grace_timeout()
    }
    fn stuck_timeout(&self) -> std::time::Duration {
        self.frontend.stuck_timeout()
    }
}
