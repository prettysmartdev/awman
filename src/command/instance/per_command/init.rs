//! `InitFrontend` impl for the TUI.

use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::config::work_items::WorkItemsConfig;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::UserMessageSink;
use crate::data::prompt::Prompt;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::{DockerfileSetupChoice, DockerfileSetupDecision, InitFrontend};
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;

impl InitFrontend for InstanceCommandFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.replace_aspec());
        }
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
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.run_audit());
        }
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

    /// The wording, the options and the answer a dismissal means are
    /// `prompt`'s (F-19); `display_path` is a fact the engine resolved, shown
    /// beside the question.
    fn ask_dockerfile_setup(
        &mut self,
        prompt: &Prompt<DockerfileSetupChoice>,
        _git_root: &std::path::Path,
        display_path: &str,
    ) -> Result<DockerfileSetupDecision, EngineError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.dockerfile_setup());
        }
        self.messages
            .info(format!("init: looked for a Dockerfile at {display_path}"));
        let choice = self
            .pick_from_prompt(prompt)
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let path = if choice == DockerfileSetupChoice::UseExisting {
            let text_response = self
                .ask_dialog(DialogRequest::TextInput {
                    title: "Dockerfile path".into(),
                    prompt: "Path relative to repo root:".into(),
                    default_text: None,
                })
                .map_err(|e| EngineError::Other(e.to_string()))?;
            match text_response {
                DialogResponse::Text(text) => Some(text),
                _ => None,
            }
        } else {
            None
        };
        Ok(choice.decide(prompt, path))
    }

    fn report_phase(&mut self, phase: &InitPhase) {
        self.messages.info(format!("init: {phase:?}"));
    }

    fn report_summary(&mut self, _summary: &InitSummary) {
        self.messages.success("init completed");
    }
}
