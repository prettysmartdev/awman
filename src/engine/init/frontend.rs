//! `InitFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::data::config::repo::WorkItemsConfig;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::message::UserMessageSink;
use crate::engine::step_status::StepStatus;

pub trait InitFrontend: UserMessageSink + Send {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError>;
    fn report_phase(&mut self, phase: &InitPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &InitSummary);
}
