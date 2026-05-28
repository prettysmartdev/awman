//! `ReadyFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::engine::step_status::StepStatus;

pub trait ReadyFrontend: UserMessageSink + Send {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError>;

    fn report_phase(&mut self, phase: &ReadyPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &ReadySummary);
}
