//! `InitFrontend` impl for the TUI.

use crate::data::config::repo::WorkItemsConfig;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::InitFrontend;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::message::UserMessageSink;
use crate::engine::step_status::StepStatus;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl InitFrontend for TuiCommandFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Replace aspec?".into(),
                body: "An aspec/ folder already exists. Replace it with fresh templates?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Run audit?".into(),
                body: "Run the audit to check the project setup?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(None) // Work items config is an advanced feature
    }

    fn report_phase(&mut self, phase: &InitPhase) {
        self.messages.info(format!("init: {phase:?}"));
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.messages.info(format!("  {step}: {status:?}"));
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }

    fn report_summary(&mut self, _summary: &InitSummary) {
        self.messages.success("init completed");
    }
}
