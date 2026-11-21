//! The TUI [`AcpFrontend`] implementation.
//!
//! Layer 3 presentation glue between an [`AcpSession`](crate::engine::acp::AcpSession)
//! (running on the engine's async task) and the TUI. It owns no business
//! logic: it appends rendered updates to a slot's shared state, opens a
//! permission modal through the existing dialog framework, and blocks for the
//! next prompt typed into the command box.
//!
//! Threading mirrors `InstanceCommandFrontend`: the dialog and prompt channels use
//! `std::sync::mpsc`, so the blocking `recv()` in `request_permission` /
//! `next_prompt` parks the OS thread the command runs on rather than stalling
//! a tokio worker — the `AcpFrontend` trait methods are synchronous, so this
//! is the correct blocking strategy.

use std::sync::Mutex;

use crate::command::instance::feeds::SharedAcpState;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::UserMessageSink;
use crate::engine::acp::{AcpFrontend, PermissionDecision, PermissionRequest, SessionUpdate};

/// Sender end of the command-box → session prompt channel. `Some(text)`
/// delivers the next prompt; `None` asks the session to cancel (matching the
/// `AcpFrontend::next_prompt` contract, where `None` triggers `session/cancel`).
pub type AcpPromptSender = std::sync::mpsc::Sender<Option<String>>;
/// Receiver end held by [`InstanceAcpFrontend`].
pub type AcpPromptReceiver = std::sync::mpsc::Receiver<Option<String>>;

/// TUI-side [`AcpFrontend`]. Constructed by the command-wiring layer, which
/// shares `state` with the ACP [`ContainerSlot`](crate::command::instance::feeds::ContainerSlot)
/// and feeds `prompt_rx` from the command box while an ACP window is focused.
pub struct InstanceAcpFrontend {
    /// Shared render state for the ACP window this session drives. Updates are
    /// appended here; the renderer reads it every frame.
    state: SharedAcpState,
    /// Dialog request channel — same mechanism `InstanceCommandFrontend::ask_dialog`
    /// uses to open modals from a command thread.
    dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
    dialog_rx: Mutex<std::sync::mpsc::Receiver<DialogResponse>>,
    /// Next-prompt source: the command box routes submitted text here instead
    /// of `Dispatch` while an ACP window is focused.
    prompt_rx: AcpPromptReceiver,
}

impl InstanceAcpFrontend {
    pub fn new(
        state: SharedAcpState,
        dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
        dialog_rx: std::sync::mpsc::Receiver<DialogResponse>,
        prompt_rx: AcpPromptReceiver,
    ) -> Self {
        Self {
            state,
            dialog_tx,
            dialog_rx: Mutex::new(dialog_rx),
            prompt_rx,
        }
    }

    /// Send a dialog request and block for the response. Mirrors
    /// `InstanceCommandFrontend::ask_dialog`; a closed channel maps to `Dismissed`.
    fn ask_dialog(&self, request: DialogRequest) -> DialogResponse {
        if self.dialog_tx.send(request).is_err() {
            return DialogResponse::Dismissed;
        }
        match self.dialog_rx.lock() {
            Ok(rx) => rx.recv().unwrap_or(DialogResponse::Dismissed),
            Err(_) => DialogResponse::Dismissed,
        }
    }
}

impl AcpFrontend for InstanceAcpFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        // Append to the focused ACP window's history; the event loop repaints
        // every tick, so no explicit redraw signal is needed.
        if let Ok(mut state) = self.state.lock() {
            state.push_update(update);
        }
    }

    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision {
        // Each option becomes a numbered hotkey in a `Custom` modal. Keep a
        // parallel char → option_id map so the response resolves back to the
        // ACP option id the engine needs.
        let mut keys: Vec<(char, String)> = Vec::new();
        let mut char_to_id: Vec<(char, String)> = Vec::new();
        for (i, opt) in request.options.iter().enumerate().take(9) {
            let ch = char::from_digit((i + 1) as u32, 10).unwrap_or('1');
            keys.push((ch, opt.name.clone()));
            char_to_id.push((ch, opt.option_id.clone()));
        }

        let tool = request
            .tool_call
            .title
            .clone()
            .unwrap_or_else(|| "(tool call)".to_string());
        let kind = request
            .tool_call
            .kind
            .as_deref()
            .map(|k| format!(" ({k})"))
            .unwrap_or_default();
        let body = format!("The agent wants to run:\n\n  {tool}{kind}\n\nAllow this action?");

        // Park the request on the slot so the window can show a pending hint
        // while the modal is open; clear it once resolved.
        if let Ok(mut state) = self.state.lock() {
            state.pending_permission = Some(request.clone());
        }

        let response = self.ask_dialog(DialogRequest::Custom {
            title: "ACP Permission Request".to_string(),
            body,
            keys,
        });

        if let Ok(mut state) = self.state.lock() {
            state.pending_permission = None;
        }

        match response {
            DialogResponse::Char(c) => char_to_id
                .into_iter()
                .find(|(ch, _)| *ch == c)
                .map(|(_, option_id)| PermissionDecision::Selected { option_id })
                .unwrap_or(PermissionDecision::Cancelled),
            _ => PermissionDecision::Cancelled,
        }
    }

    fn next_prompt(&mut self) -> Option<String> {
        // Block until the command box submits a prompt for this session. A
        // closed channel (command box gone) or an explicit `None` both end the
        // session, which the driver turns into `session/cancel`.
        self.prompt_rx.recv().unwrap_or(None)
    }
}

// The command frontend is also the portable ACP boundary for single-command
// launches.  The richer `InstanceAcpFrontend` above is used by slot-aware wiring;
// until a command has installed its slot, this fallback renders updates as
// plain summary lines (never raw JSON) and — crucially — FAILS CLOSED on
// permission requests. `AcpSession` only calls `request_permission` when
// neither `--yolo` nor `--auto` is active, so a request reaching this stub is
// by definition a run the user did NOT ask to auto-approve; approving it here
// would silently grant a tool call with no prompt. Deny instead.
impl AcpFrontend for InstanceCommandFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        self.messages
            .info(crate::engine::acp::protocol::summarize_update(&update));
    }

    fn request_permission(&mut self, _request: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Cancelled
    }

    fn next_prompt(&mut self) -> Option<String> {
        None
    }
}
