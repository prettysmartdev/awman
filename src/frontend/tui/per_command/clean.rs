//! `CleanCommandFrontend` impl for the TUI.
//!
//! Presents the itemized cleanup summary in a yes/no confirmation dialog and
//! proceeds only when the user confirms.

use crate::command::commands::clean::{CleanCommandFrontend, CleanSummary};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl CleanCommandFrontend for TuiCommandFrontend {
    fn confirm_deletion(&mut self, summary: &CleanSummary) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Confirm clean".to_string(),
            body: summary.render(),
        })?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::command::commands::clean::{CleanContainer, CleanSummary};
    use crate::command::dispatch::parsed_input::FlagValue;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn make_tui_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        make_tui_frontend_with_flags(BTreeMap::new())
    }

    fn make_tui_frontend_with_flags(
        flags: BTreeMap<String, FlagValue>,
    ) -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        crate::frontend::tui::tests::test_command_frontend(&["clean"], flags)
    }

    fn sample_summary() -> CleanSummary {
        CleanSummary {
            containers: vec![CleanContainer {
                id: "abc1234567890f".to_string(),
                name: "awman-test".to_string(),
            }],
            ..Default::default()
        }
    }

    // Test that confirm_deletion sends DialogRequest::YesNo with the correct
    // title ("Confirm clean") and body equal to summary.render().
    #[test]
    fn tui_confirm_deletion_sends_yesno_with_correct_title_and_body() {
        let (mut fe, req_rx, resp_tx) = make_tui_frontend();

        let summary = sample_summary();
        let expected_body = summary.render();

        // Pre-populate the response so ask_dialog doesn't block
        resp_tx.send(DialogResponse::Yes).unwrap();

        let result = fe.confirm_deletion(&summary).unwrap();
        assert!(result, "DialogResponse::Yes must return Ok(true)");

        // Inspect the request that was sent
        let req = req_rx
            .try_recv()
            .expect("DialogRequest must have been sent");
        match req {
            DialogRequest::YesNo { title, body } => {
                assert_eq!(
                    title, "Confirm clean",
                    "dialog title must be 'Confirm clean'"
                );
                assert_eq!(
                    body, expected_body,
                    "dialog body must match summary.render()"
                );
            }
            other => panic!("expected YesNo dialog, got: {other:?}"),
        }
    }

    // Test that DialogResponse::No causes confirm_deletion to return Ok(false),
    // which will cause the command to abort deletion.
    #[test]
    fn tui_confirm_deletion_no_response_aborts_deletion() {
        let (mut fe, req_rx, resp_tx) = make_tui_frontend();

        let summary = sample_summary();
        resp_tx.send(DialogResponse::No).unwrap();

        let result = fe.confirm_deletion(&summary).unwrap();
        assert!(!result, "DialogResponse::No must return Ok(false)");

        // Confirm the request was still sent (dialog was opened)
        assert!(
            req_rx.try_recv().is_ok(),
            "dialog request must have been sent even when No"
        );
    }

    // Test that DialogResponse::Char('y') is treated as confirmation.
    #[test]
    fn tui_confirm_deletion_char_y_response_confirms() {
        let (mut fe, req_rx, resp_tx) = make_tui_frontend();

        let summary = sample_summary();
        resp_tx.send(DialogResponse::Char('y')).unwrap();

        let result = fe.confirm_deletion(&summary).unwrap();
        assert!(result, "DialogResponse::Char('y') must return Ok(true)");
        let _ = req_rx.try_recv();
    }

    /// The TUI's summary display is its dialog; there is nothing else to show.
    #[test]
    fn tui_show_summary_prints_nothing_of_its_own() {
        let (mut fe, req_rx, _resp_tx) = make_tui_frontend();
        fe.show_summary(&sample_summary());
        assert!(
            req_rx.try_recv().is_err(),
            "show_summary must not open a dialog"
        );
    }
}
