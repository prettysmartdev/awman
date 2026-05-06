//! `TuiCommandFrontend` — the single Layer 3 struct implementing every
//! per-command frontend trait for the TUI execution mode.
//!
//! Constructed from a `ParsedCommandBoxInput` (produced by
//! `Dispatch::parse_command_box_input`). Flag/argument extraction reads from
//! the parsed input's typed maps. Interactive Q&A methods open modal dialogs
//! via the dialog channel and block until the user responds.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::command::dispatch::catalogue::{CommandCatalogue, FlagKind};
use crate::command::dispatch::parsed_input::{ArgValue, FlagValue, ParsedCommandBoxInput};
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::engine::container::frontend::ContainerIo;
use crate::engine::message::{UserMessage, UserMessageSink};
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::{SharedPtyResetFlag, SharedWorkflowViewState, SharedYoloState};
use crate::frontend::tui::user_message::{SharedStatusLog, TuiUserMessageSink};

/// TUI frontend struct. Implements every per-command frontend trait.
///
/// Dialog channels use `std::sync::mpsc` so that the blocking `recv()` in
/// `ask_dialog` parks the OS thread rather than stalling a tokio worker —
/// the engine trait methods are synchronous, so this is the correct
/// blocking strategy.
///
/// Container I/O channels (stdout/stdin/resize) are bundled into a
/// `ContainerIo` and detached lazily by the engine via `take_container_io`.
/// The TUI populates these channels from `App::spawn_command`; the engine's
/// container backend drains them against a real PTY master.
pub struct TuiCommandFrontend {
    parsed: ParsedCommandBoxInput,
    pub(crate) messages: TuiUserMessageSink,
    pub(crate) pty_active: bool,
    pub(crate) dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
    pub(crate) dialog_rx: Mutex<std::sync::mpsc::Receiver<DialogResponse>>,
    pub(crate) container_io: Option<ContainerIo>,
    pub(crate) status_log: SharedStatusLog,
    /// Workflow strip state — workflow_frontend.rs writes to it on
    /// `report_workflow_progress` / `report_step_status`. The renderer reads
    /// it under the same lock.
    pub(crate) workflow_view: SharedWorkflowViewState,
    /// Yolo countdown overlay state — `yolo_countdown_tick` updates it
    /// every 100ms. The renderer reads it for the non-modal countdown
    /// indicator (avoids the dialog-spam that a per-tick `ask_dialog` would
    /// cause).
    pub(crate) yolo_state: SharedYoloState,
    /// Shared flag: set to `true` to signal the TUI event loop to reset the
    /// vt100 parser between workflow steps.
    pub(crate) pty_reset_flag: SharedPtyResetFlag,
}

impl TuiCommandFrontend {
    pub fn new(
        parsed: ParsedCommandBoxInput,
        status_log: SharedStatusLog,
        dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
        dialog_rx: std::sync::mpsc::Receiver<DialogResponse>,
        container_io: ContainerIo,
        workflow_view: SharedWorkflowViewState,
        yolo_state: SharedYoloState,
        pty_reset_flag: SharedPtyResetFlag,
    ) -> Self {
        Self {
            parsed,
            messages: TuiUserMessageSink::new(status_log.clone()),
            pty_active: false,
            dialog_tx,
            dialog_rx: Mutex::new(dialog_rx),
            container_io: Some(container_io),
            status_log,
            workflow_view,
            yolo_state,
            pty_reset_flag,
        }
    }

    /// Send a dialog request and block waiting for the response.
    ///
    /// This uses `std::sync::mpsc::Receiver::recv()` which blocks the OS
    /// thread. Since engine trait methods are synchronous this is correct —
    /// no tokio executor is blocked.
    pub(crate) fn ask_dialog(
        &self,
        request: DialogRequest,
    ) -> Result<DialogResponse, CommandError> {
        let _ = self.dialog_tx.send(request);
        self.dialog_rx
            .lock()
            .map_err(|_| CommandError::Aborted)?
            .recv()
            .map_err(|_| CommandError::Aborted)
    }

    /// Check if a flag-path flag is a known Bool flag in the catalogue.
    fn is_known_bool_flag(&self, command_path: &[&str], flag: &str) -> bool {
        let cat = CommandCatalogue::get();
        cat.lookup(command_path)
            .and_then(|spec| spec.find_flag(flag))
            .map(|f| matches!(f.kind, FlagKind::Bool))
            .unwrap_or(false)
    }
}

// ─── UserMessageSink ──────────────────────────────────────────────────────

impl UserMessageSink for TuiCommandFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.messages.replay_queued();
    }
}

// ─── CommandFrontend ──────────────────────────────────────────────────────

impl CommandFrontend for TuiCommandFrontend {
    fn flag_bool(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<bool>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::Bool(v)) => Ok(Some(*v)),
            Some(_) => Ok(Some(true)),
            None => {
                if self.is_known_bool_flag(&self.parsed.path.iter().map(|s| s.as_str()).collect::<Vec<_>>(), flag) {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => Ok(Some(v.clone())),
            _ => Ok(None),
        }
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::Strings(v)) => Ok(v.clone()),
            Some(FlagValue::String(v)) => Ok(vec![v.clone()]),
            _ => Ok(Vec::new()),
        }
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => Ok(Some(PathBuf::from(v))),
            _ => Ok(None),
        }
    }

    fn flag_enum(
        &self,
        command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        self.flag_string(command_path, flag)
    }

    fn flag_u16(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<u16>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => v
                .parse::<u16>()
                .map(Some)
                .map_err(|_| CommandError::InvalidFlagValue {
                    command: self.parsed.path.clone(),
                    flag: flag.to_string(),
                    reason: format!("'{v}' is not a valid u16"),
                }),
            _ => Ok(None),
        }
    }

    fn argument(
        &self,
        _command_path: &[&str],
        name: &str,
    ) -> Result<Option<String>, CommandError> {
        match self.parsed.arguments.get(name) {
            Some(ArgValue::Single(v)) => Ok(Some(v.clone())),
            Some(ArgValue::Multi(v)) => Ok(Some(v.join(" "))),
            None => Ok(None),
        }
    }

    fn arguments(
        &self,
        _command_path: &[&str],
        name: &str,
    ) -> Result<Vec<String>, CommandError> {
        match self.parsed.arguments.get(name) {
            Some(ArgValue::Multi(v)) => Ok(v.clone()),
            Some(ArgValue::Single(v)) => Ok(vec![v.clone()]),
            None => Ok(Vec::new()),
        }
    }
}
