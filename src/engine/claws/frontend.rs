//! `ClawsFrontend` trait — defined by Layer 1, implemented by Layer 3.

use std::path::Path;

use crate::engine::claws::phase::ClawsPhase;
use crate::engine::claws::summary::ClawsSummary;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::step_status::StepStatus;

pub trait ClawsFrontend: UserMessageSink + Send {
    fn ask_replace_existing_clone(&mut self, path: &Path) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn report_phase(&mut self, phase: &ClawsPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &ClawsSummary);
}
