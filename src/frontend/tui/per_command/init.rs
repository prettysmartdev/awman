//! `InitFrontend` impl for the TUI.

use crate::data::config::repo::WorkItemsConfig;
use crate::data::message::UserMessageSink;
use crate::data::prompt::Prompt;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::{DockerfileSetupChoice, DockerfileSetupDecision, InitFrontend};
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
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

    /// The wording, the options and the answer a dismissal means are
    /// `prompt`'s (F-19); `display_path` is a fact the engine resolved, shown
    /// beside the question.
    fn ask_dockerfile_setup(
        &mut self,
        prompt: &Prompt<DockerfileSetupChoice>,
        _git_root: &std::path::Path,
        display_path: &str,
    ) -> Result<DockerfileSetupDecision, EngineError> {
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
#[cfg(test)]
mod tests {
    use crate::command::prompts;
    use crate::engine::init::frontend::DockerfileSetupDecision;
    use crate::engine::init::InitFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn make_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        crate::frontend::tui::tests::test_command_frontend(&["init"], Default::default())
    }

    /// Drive `ask_dockerfile_setup` with a scripted sequence of dialog
    /// answers and return the decision it reports.
    fn answer_with(responses: Vec<DialogResponse>) -> DockerfileSetupDecision {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            for response in responses {
                let _req = req_rx.recv().unwrap();
                resp_tx.send(response).unwrap();
            }
        });
        let result = frontend
            .ask_dockerfile_setup(
                &prompts::dockerfile_setup(),
                git_root.path(),
                "Dockerfile.dev",
            )
            .unwrap();
        handle.join().unwrap();
        result
    }

    /// What the prompt itself says a non-answer means. Read, never written:
    /// these are frontend tests, and a frontend test that asserts a default is
    /// a finding in the next audit (WI 0114 F-19, Test Considerations).
    fn dismissal_answer() -> DockerfileSetupDecision {
        let prompt = prompts::dockerfile_setup();
        prompt
            .default_on_dismiss
            .expect("the dockerfile prompt declares a dismissal answer")
            .decide(&prompt, None)
    }

    // ─── Key/index → value mapping. Legal in a frontend test. ───────────────

    #[test]
    fn index_0_selects_the_prompts_first_choice() {
        let expected = prompts::dockerfile_setup()
            .answer_at(0)
            .unwrap()
            .decide(&prompts::dockerfile_setup(), None);
        assert_eq!(answer_with(vec![DialogResponse::Index(0)]), expected);
    }

    #[test]
    fn index_2_selects_the_prompts_third_choice() {
        let expected = prompts::dockerfile_setup()
            .answer_at(2)
            .unwrap()
            .decide(&prompts::dockerfile_setup(), None);
        assert_eq!(answer_with(vec![DialogResponse::Index(2)]), expected);
    }

    #[test]
    fn index_1_then_a_path_uses_that_path() {
        assert_eq!(
            answer_with(vec![
                DialogResponse::Index(1),
                DialogResponse::Text("docker/Dockerfile".to_string()),
            ]),
            DockerfileSetupDecision::UseExisting("docker/Dockerfile".to_string())
        );
    }

    // ─── Non-answers defer to the prompt, not to this frontend. ─────────────

    #[test]
    fn dismissing_the_choice_takes_the_prompts_answer() {
        assert_eq!(
            answer_with(vec![DialogResponse::Dismissed]),
            dismissal_answer()
        );
    }

    #[test]
    fn dismissing_the_path_box_takes_the_prompts_answer() {
        assert_eq!(
            answer_with(vec![DialogResponse::Index(1), DialogResponse::Dismissed]),
            dismissal_answer()
        );
    }

    #[test]
    fn an_empty_path_takes_the_prompts_answer() {
        assert_eq!(
            answer_with(vec![
                DialogResponse::Index(1),
                DialogResponse::Text(String::new()),
            ]),
            dismissal_answer()
        );
    }

    /// The choice the user picked still has to reach the decision: a frontend
    /// that answered the prompt's dismissal value for *everything* would pass
    /// the tests above.
    #[test]
    fn the_skip_choice_is_not_the_dismissal_answer() {
        assert_eq!(
            answer_with(vec![DialogResponse::Index(2)]),
            DockerfileSetupDecision::Skip
        );
        assert_ne!(dismissal_answer(), DockerfileSetupDecision::Skip);
    }
}
