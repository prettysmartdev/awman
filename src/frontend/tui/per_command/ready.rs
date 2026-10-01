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
        // The shared `ReadyPhase` Display text, as the API reports it.
        self.messages.info(format!("ready: {phase}"));
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        // The rows, outcome line and hint are the summary's own (F-23;
        // readiness now accounts for per-image build/import results too):
        // this renders them without recomputing any of that itself.
        for line in crate::frontend::render_helpers::render_ready_summary(summary).lines() {
            self.messages.info(line.to_string());
        }

        if let Some(hint) = summary.hint() {
            self.messages.info(format!("Tip: {hint}"));
        }

        if summary.is_ready() {
            self.messages.success(summary.outcome_line().to_string());
        } else {
            self.messages.error_msg(summary.outcome_line().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::data::message::MessageLevel;
    use crate::data::ready_phase::ReadyPhase;
    use crate::data::ready_summary::ReadySummary;
    use crate::data::step_status::StepStatus;
    use crate::engine::ready::frontend::ReadyFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn log_texts(frontend: &TuiCommandFrontend) -> Vec<(MessageLevel, String)> {
        frontend
            .status_log
            .lock()
            .unwrap()
            .iter()
            .map(|e| (e.level, e.text.clone()))
            .collect()
    }

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

    /// `report_phase` renders the shared `ReadyPhase` `Display` text — the
    /// same wording the API reports over its event stream (F2 parity).
    #[test]
    fn report_phase_uses_the_shared_ready_phase_display() {
        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.report_phase(&ReadyPhase::BuildingBaseImage);
        let log = log_texts(&frontend);
        assert!(
            log.iter()
                .any(|(_, t)| t == "ready: Building base image..."),
            "{log:?}"
        );
    }

    /// A ready summary renders the rows, then `success` (not a plain `info`)
    /// with the summary's own `outcome_line()`, plus a `Tip:` line when
    /// `ReadySummary::hint()` has one — e.g. missing non-default images,
    /// whose hint verb follows `ReadyImageAction`.
    #[test]
    fn report_summary_shows_success_and_the_missing_images_hint_when_ready() {
        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        let mut summary = ReadySummary::new("builtin");
        summary.dockerfile = StepStatus::Done;
        summary.base_image = StepStatus::Skipped;
        summary.agent_image = StepStatus::Done;
        summary.local_agent = StepStatus::Done;
        summary.audit = StepStatus::Skipped;
        summary.image_rebuild = StepStatus::Skipped;
        summary.image_action = crate::data::ready_summary::ReadyImageAction::Import;
        summary
            .non_default_agent_images
            .push(("Missing images".into(), StepStatus::Warn("codex".into())));

        frontend.report_summary(&summary);

        assert!(summary.is_ready(), "sanity: this summary must be ready");
        let log = log_texts(&frontend);
        assert!(
            log.iter()
                .any(|(level, t)| *level == MessageLevel::Success && t == "awman is ready."),
            "{log:?}"
        );
        assert!(
            log.iter()
                .any(|(_, t)| t
                    == "Tip: run \"ready --build\" to import all available agent images."),
            "{log:?}"
        );
    }

    /// A not-ready summary (a failed required image stage) renders `error_msg`
    /// with the "not ready" outcome line and the fix-and-retry hint — the TUI
    /// must never show `success` for a run that recorded a failure (the
    /// behaviour change WI 0125 M-7 fixed: this frontend used to call
    /// `messages.success("awman is ready.")` unconditionally).
    #[test]
    fn report_summary_shows_error_and_the_recovery_hint_when_not_ready() {
        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        let mut summary = ReadySummary::new("builtin");
        summary.base_image = StepStatus::Failed("daemon unreachable".into());

        frontend.report_summary(&summary);

        assert!(
            !summary.is_ready(),
            "sanity: this summary must not be ready"
        );
        let log = log_texts(&frontend);
        assert!(
            log.iter()
                .any(|(level, t)| *level == MessageLevel::Error && t == "awman is not ready."),
            "{log:?}"
        );
        assert!(
            log.iter().any(|(_, t)| t
                == "Tip: complete the required image steps, then run \"awman ready\" again."),
            "{log:?}"
        );
    }
}
