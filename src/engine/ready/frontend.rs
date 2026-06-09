//! `ReadyFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::data::message::UserMessageSink;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::error::EngineError;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::engine::step_status::StepStatus;

pub trait ReadyFrontend: UserMessageSink + Send {
    /// Called when the project Dockerfile (default `Dockerfile.dev` or the
    /// path configured via `RepoConfig.dockerfile`) is missing.
    ///
    /// `dockerfile_path` is the resolved absolute path that the engine
    /// expects the user to confirm creating.
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError>;
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError>;

    fn report_phase(&mut self, phase: &ReadyPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend>;
    fn report_summary(&mut self, summary: &ReadySummary);
}
