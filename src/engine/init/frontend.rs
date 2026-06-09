//! `InitFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::data::config::repo::WorkItemsConfig;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::message::UserMessageSink;
use crate::engine::step_status::StepStatus;

/// User's choice when no project-base Dockerfile is found during init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerfileSetupDecision {
    /// Create `Dockerfile.dev` from the bundled template.
    CreateNew,
    /// Use an existing Dockerfile at this path (relative to git_root or absolute).
    UseExisting(String),
    /// Skip Dockerfile setup entirely.
    Skip,
}

pub trait InitFrontend: UserMessageSink + Send {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError>;
    /// Called when no project-base Dockerfile is found during init.
    /// Returns the user's choice of how to proceed.
    fn ask_dockerfile_setup(
        &mut self,
        git_root: &std::path::Path,
    ) -> Result<DockerfileSetupDecision, EngineError>;
    fn report_phase(&mut self, phase: &InitPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend>;
    fn report_summary(&mut self, summary: &InitSummary);
}
