//! `ReadyFrontend` impl for the TUI.

use crate::data::message::UserMessageSink;
use crate::data::ready_phase::ReadyPhase;
use crate::data::ready_summary::ReadySummary;
use crate::engine::error::EngineError;
use crate::engine::ready::frontend::ReadyFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl ReadyFrontend for TuiCommandFrontend {
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Create Dockerfile?".into(),
                body: format!(
                    "No Dockerfile found at {}. Create one from the default template?",
                    dockerfile_path.display()
                ),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Run audit?".into(),
                body: "Dockerfile.dev matches the default template. Run the audit to install project dependencies?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.messages.info(format!("ready: {phase:?}"));
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        // The rows are the summary's own (F-23); this renders them.
        for line in crate::frontend::render_helpers::render_ready_summary(summary).lines() {
            self.messages.info(line.to_string());
        }

        let has_missing = summary
            .non_default_agent_images
            .iter()
            .any(|(_, s)| matches!(s, crate::data::step_status::StepStatus::Warn(_)));
        if has_missing {
            self.messages.info(
                "Tip: run \"ready --build\" to build all available agent images.".to_string(),
            );
        }

        self.messages.success("awman is ready.".to_string());
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::ready::frontend::ReadyFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn make_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        crate::frontend::tui::tests::test_command_frontend(&["ready"], Default::default())
    }

    #[test]
    fn ask_create_dockerfile_yes_returns_true() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Yes).unwrap();
        });
        let result = frontend
            .ask_create_dockerfile(std::path::Path::new("/tmp/Dockerfile.dev"))
            .unwrap();
        handle.join().unwrap();
        assert!(result);
    }

    #[test]
    fn ask_create_dockerfile_no_returns_false() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::No).unwrap();
        });
        let result = frontend
            .ask_create_dockerfile(std::path::Path::new("/tmp/Dockerfile.dev"))
            .unwrap();
        handle.join().unwrap();
        assert!(!result);
    }

    #[test]
    fn ask_create_dockerfile_dismissed_returns_false() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Dismissed).unwrap();
        });
        let result = frontend
            .ask_create_dockerfile(std::path::Path::new("/tmp/Dockerfile.dev"))
            .unwrap();
        handle.join().unwrap();
        assert!(!result);
    }

    #[test]
    fn ask_run_audit_on_template_yes_returns_true() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Yes).unwrap();
        });
        let result = frontend.ask_run_audit_on_template().unwrap();
        handle.join().unwrap();
        assert!(result);
    }

    #[test]
    fn ask_run_audit_on_template_no_returns_false() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::No).unwrap();
        });
        let result = frontend.ask_run_audit_on_template().unwrap();
        handle.join().unwrap();
        assert!(!result);
    }
}
