//! `CleanCommandFrontend` impl for the TUI.
//!
//! Presents the itemized cleanup summary in a yes/no confirmation dialog and
//! proceeds only when the user confirms.

use crate::command::commands::clean::{CleanCommandFrontend, CleanSummary};
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl CleanCommandFrontend for TuiCommandFrontend {
    fn confirm_deletion(&mut self, summary: &CleanSummary) -> Result<bool, CommandError> {
        if self.flag_bool(&["clean"], "yes")?.unwrap_or(false) {
            return Ok(true);
        }

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
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use crate::command::commands::clean::{CleanContainer, CleanSummary};
    use crate::command::dispatch::parsed_input::{FlagValue, ParsedCommandBoxInput};
    use crate::engine::agent_runtime::frontend::AgentIo;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
    use crate::frontend::tui::user_message::SharedStatusLog;

    fn make_tui_frontend(
        dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
        dialog_rx: std::sync::mpsc::Receiver<DialogResponse>,
    ) -> TuiCommandFrontend {
        make_tui_frontend_with_flags(BTreeMap::new(), dialog_tx, dialog_rx)
    }

    fn make_tui_frontend_with_flags(
        flags: BTreeMap<String, FlagValue>,
        dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
        dialog_rx: std::sync::mpsc::Receiver<DialogResponse>,
    ) -> TuiCommandFrontend {
        let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let container_io = AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx.clone(),
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };
        let status_log: SharedStatusLog = Arc::new(Mutex::new(vec![]));
        TuiCommandFrontend::new(
            ParsedCommandBoxInput {
                path: vec!["clean".to_string()],
                flags,
                arguments: BTreeMap::new(),
            },
            status_log,
            dialog_tx,
            dialog_rx,
            container_io,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext { tabs: vec![] },
            )),
        )
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
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let mut fe = make_tui_frontend(req_tx, resp_rx);

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
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let mut fe = make_tui_frontend(req_tx, resp_rx);

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
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let mut fe = make_tui_frontend(req_tx, resp_rx);

        let summary = sample_summary();
        resp_tx.send(DialogResponse::Char('y')).unwrap();

        let result = fe.confirm_deletion(&summary).unwrap();
        assert!(result, "DialogResponse::Char('y') must return Ok(true)");
        let _ = req_rx.try_recv();
    }

    #[test]
    fn tui_confirm_deletion_yes_flag_skips_dialog() {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (_resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let mut flags = BTreeMap::new();
        flags.insert("yes".to_string(), FlagValue::Bool(true));
        let mut fe = make_tui_frontend_with_flags(flags, req_tx, resp_rx);

        let result = fe.confirm_deletion(&sample_summary()).unwrap();

        assert!(result, "--yes must confirm without opening a dialog");
        assert!(
            req_rx.try_recv().is_err(),
            "--yes must skip the TUI confirmation dialog"
        );
    }
}
