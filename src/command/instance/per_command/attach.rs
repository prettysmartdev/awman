//! TUI presentation for `awman attach` (WI 0141): the terminal stream is
//! rendered in a container slot's `vt100` window, exactly as `squad attach`
//! renders today, so no raw agent byte reaches the host terminal.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::command::commands::attach::{
    initial_client_frames, AttachCommandFrontend, AttachSlot, AttachViewState,
};
use crate::command::error::CommandError;
use crate::command::instance::frontend::{HostByteFilter, InstanceCommandFrontend};
use crate::command::terminals::{ReplayEffect, ReplayFeed, ReplayFilter, LAGGED_MESSAGE};
use crate::data::ids::TerminalId;
use crate::data::terminal::HostOutputPolicy;
use crate::data::terminal::{DetachReason, TerminalClientFrame, TerminalFrame};
use crate::engine::remote::terminal_client::TerminalStream;

/// The slot key and container label of an ordinary `awman attach`.
const ATTACH_STEP: &str = "attach";
const ATTACH_CONTAINER: &str = "attached terminal";

/// One container slot bound to one terminal stream (WI 0105 B.6). The
/// squad follower launches one per terminal it shows; ordinary attach uses
/// one with the labels above.
pub(crate) struct TerminalBridge {
    /// Slot key (`ContainerSlotEvent` `step_name`).
    pub step_name: String,
    pub agent_label: String,
    pub container_label: String,
    /// Never takes control and never sends input or resizes.
    pub read_only: bool,
    pub policy: HostOutputPolicy,
    /// Cancelling detaches this bridge only (`Detached { User }`).
    pub cancel: CancellationToken,
    /// A run viewer keeps the last screen during transport loss. A later
    /// run snapshot or reconnect replaces/evicts it.
    pub keep_on_hangup: bool,
}

/// Why a bridge stopped. The bridge itself cancels nothing but its stream;
/// the caller decides what the end means for the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeEnd {
    /// `TerminalBridge::cancel` fired.
    Detached,
    /// The stream or the slot's output channel went away.
    Hangup,
    /// The terminal exited with this code.
    Exited(i32),
}

/// The presenter's host-output filter as the replay feed's filter: one
/// instance for the producer stream (history and live bytes), and a second
/// one, same policy, for snapshot repaints so a repaint never disturbs the
/// stream's mid-sequence state.
struct SlotFilter {
    stream: Box<dyn HostByteFilter>,
    snapshot: Box<dyn HostByteFilter>,
}

impl ReplayFilter for SlotFilter {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.stream.feed(chunk, out);
    }
    fn finish(&mut self, out: &mut Vec<u8>) {
        self.stream.finish(out);
    }
    fn feed_snapshot(&mut self, screen: &[u8], out: &mut Vec<u8>) {
        self.snapshot.feed(screen, out);
        self.snapshot.finish(out);
    }
}

impl InstanceCommandFrontend {
    /// Launch one container slot for one terminal stream. History (when the
    /// stream asked for it), the snapshot and live output are written to the
    /// slot through one filter, in arrival order (`ReplayFeed`). The slot
    /// is announced (`Launched`, `ContainerName`) before this returns and
    /// `AttachedExited` is pushed when the bridge ends.
    pub(crate) fn spawn_terminal_bridge(
        &mut self,
        bridge: TerminalBridge,
        mut stream: TerminalStream,
    ) -> tokio::task::JoinHandle<BridgeEnd> {
        let (stdout_tx, stdout_rx) = crate::command::instance::feeds::slot_output_channel();
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let (cols, rows) = self.pty_size.get();
        let io = crate::command::instance::feeds::ContainerSlotIo {
            stdout_rx,
            stdin_tx,
            resize_tx: resize_tx.clone(),
        };
        let TerminalBridge {
            step_name,
            agent_label,
            container_label,
            read_only,
            policy,
            cancel,
            keep_on_hangup,
        } = bridge;
        if let Ok(mut events) = self.container.events.lock() {
            events.push_back(
                crate::command::instance::feeds::ContainerSlotEvent::Launched {
                    step_name: step_name.clone(),
                    agent: agent_label,
                    model: None,
                    io: Some(io),
                },
            );
            events.push_back(
                crate::command::instance::feeds::ContainerSlotEvent::ContainerName {
                    step_name: step_name.clone(),
                    container_name: container_label.clone(),
                },
            );
        }
        let events = self.container.events.clone();
        let messages = self.messages.clone();
        let mut filter = SlotFilter {
            stream: self.presentation.host_output_filter(policy),
            snapshot: self.presentation.host_output_filter(policy),
        };
        tokio::spawn(async move {
            use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
            let mut writable = !read_only;
            let mut feed = ReplayFeed::new();
            // The slot renders at the presenter's size: a snapshot captured at
            // another size is fitted to it instead of scrolling it.
            feed.set_viewer_size(cols, rows);
            let mut end = BridgeEnd::Detached;
            let mut reason = DetachReason::User;
            for frame in initial_client_frames(read_only, Some((cols, rows))) {
                let _ = stream.send(frame).await;
            }
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Some(bytes) = stdin_rx.recv() => {
                        if writable { let _ = stream.send(TerminalClientFrame::Input { bytes }).await; }
                    }
                    Some((cols, rows)) = resize_rx.recv() => {
                        feed.set_viewer_size(cols, rows);
                        if writable { let _ = stream.send(TerminalClientFrame::Resize { cols, rows }).await; }
                    }
                    // At the queued-bytes budget the bridge stops reading: the hub
                    // then lags this viewer and resynchronises it with a snapshot,
                    // instead of the bridge buffering a stalled slot without bound.
                    _ = stdout_tx.drained(), if stdout_tx.over_budget() => {}
                    frame = stream.next(), if !stdout_tx.over_budget() => match frame {
                        Ok(Some(frame @ (TerminalFrame::History { .. } | TerminalFrame::Snapshot { .. } | TerminalFrame::Output { .. } | TerminalFrame::Lagged))) => {
                            let mut safe = Vec::new();
                            let notice = match feed.apply(&frame, &mut filter, &mut safe) {
                                ReplayEffect::FirstLag => Some(LAGGED_MESSAGE),
                                ReplayEffect::Truncated => Some(crate::command::terminals::TRUNCATED_MESSAGE),
                                ReplayEffect::None => None,
                            };
                            if let Some(text) = notice {
                                let mut messages = messages.clone();
                                messages.write_message(UserMessage { level: MessageLevel::Info, text: text.into() });
                            }
                            if !safe.is_empty() && stdout_tx.send(safe).is_err() { end = BridgeEnd::Hangup; reason = DetachReason::Hangup; break; }
                        }
                        Ok(Some(TerminalFrame::Exit { code })) => { end = BridgeEnd::Exited(code); break; }
                        Ok(None) | Err(_) => { end = BridgeEnd::Hangup; reason = DetachReason::Hangup; break; }
                        Ok(Some(TerminalFrame::Error { code, .. })) => {
                            if writable && matches!(code.as_str(), "control_held" | "input_not_allowed") {
                                writable = false;
                                let mut messages = messages.clone();
                                messages.write_message(UserMessage { level: MessageLevel::Warning, text: "terminal control unavailable; watching read-only".into() });
                            }
                        }
                        Ok(Some(TerminalFrame::AutomationState { snapshot })) => {
                            if snapshot.status_support == crate::data::automation::StatusSupport::BestEffort {
                                let mut messages = messages.clone();
                                messages.write_message(UserMessage { level: MessageLevel::Warning, text: "Best-effort automation status".into() });
                            }
                        }
                        Ok(Some(TerminalFrame::Resize { .. } | TerminalFrame::TurnAccepted { .. } | TerminalFrame::TurnResult { .. } | TerminalFrame::ExecutionClosed { .. } | TerminalFrame::ExecutionReplaced { .. } | TerminalFrame::CredentialUpdateState { .. } | TerminalFrame::OperationError { .. })) => {}
                    }
                }
            }
            let mut safe = Vec::new();
            filter.finish(&mut safe);
            if !safe.is_empty() {
                let _ = stdout_tx.send(safe);
            }
            let _ = stream.send(TerminalClientFrame::Detached { reason }).await;
            let _ = stream.close().await;
            if let Ok(mut events) = events.lock() {
                if end == BridgeEnd::Hangup && keep_on_hangup {
                    return end;
                }
                events.push_back(
                    crate::command::instance::feeds::ContainerSlotEvent::AttachedExited {
                        step_name,
                        container_name: container_label,
                    },
                );
            }
            end
        })
    }
}

#[async_trait]
impl AttachCommandFrontend for InstanceCommandFrontend {
    fn follows_run(&self) -> bool {
        true
    }

    fn attach_view_state(&mut self, state: AttachViewState) {
        if let Some(reachable) = &self.attach_daemon_reachable {
            reachable.store(
                state != AttachViewState::DaemonUnreachable,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        self.attach_status(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Info,
            text: state.message(),
        });
    }

    fn publish_workflow(&mut self, state: &crate::data::workflow_state::WorkflowState) {
        let projected = crate::command::instance::feeds::workflow_state_to_view_state(state);
        self.workflow.publish_remote(state.invocation_id, projected);
    }

    fn bind_terminal_slot(
        &mut self,
        slot: AttachSlot,
        stream: TerminalStream,
        read_only: bool,
        policy: HostOutputPolicy,
        cancel: CancellationToken,
    ) -> Result<tokio::task::JoinHandle<()>, CommandError> {
        self.container.push(
            crate::command::instance::feeds::ContainerSlotEvent::Exited {
                step_name: slot.step_name.clone(),
            },
        );
        let bridge = self.spawn_terminal_bridge(
            TerminalBridge {
                step_name: slot.step_name,
                agent_label: slot.agent,
                container_label: slot.container,
                read_only,
                policy,
                cancel,
                keep_on_hangup: true,
            },
            stream,
        );
        // A leader/step exit ends only its slot. The command follows the run.
        Ok(tokio::spawn(async move {
            let _ = bridge.await;
        }))
    }

    fn remove_terminal_slot(&mut self, step_name: &str) {
        self.container.push(
            crate::command::instance::feeds::ContainerSlotEvent::Exited {
                step_name: step_name.to_owned(),
            },
        );
    }

    fn begin_attach(&mut self, _terminal_id: &TerminalId) -> CancellationToken {
        self.attach_hangup = CancellationToken::new();
        if let Ok(mut events) = self.container.events.lock() {
            events.clear();
            events.push_back(
                crate::command::instance::feeds::ContainerSlotEvent::Exited {
                    step_name: String::new(),
                },
            );
        }
        self.command_cancel.clone()
    }

    fn attach_status(&mut self, message: crate::data::message::UserMessage) {
        if let Ok(mut status) = self.attach_status.lock() {
            *status = Some(message.clone());
        }
        crate::data::message::UserMessageSink::write_message(self, message);
    }

    fn terminal_ready(&mut self) {
        if let Ok(mut status) = self.attach_status.lock() {
            *status = None;
        }
    }

    fn hangup(&self) -> CancellationToken {
        self.attach_hangup.clone()
    }

    fn bind_terminal(
        &mut self,
        stream: TerminalStream,
        read_only: bool,
        policy: HostOutputPolicy,
    ) -> Result<tokio::task::JoinHandle<()>, CommandError> {
        let cancel = self.command_cancel.clone();
        let hangup = self.attach_hangup.clone();
        let exit_code = self.attach_exit_code.clone();
        exit_code.store(0, std::sync::atomic::Ordering::Relaxed);
        let bridge = self.spawn_terminal_bridge(
            TerminalBridge {
                step_name: ATTACH_STEP.to_string(),
                agent_label: "attached agent".to_string(),
                container_label: ATTACH_CONTAINER.to_string(),
                read_only,
                policy,
                cancel: cancel.clone(),
                keep_on_hangup: false,
            },
            stream,
        );
        Ok(tokio::spawn(async move {
            match bridge.await {
                Ok(BridgeEnd::Exited(code)) => {
                    exit_code.store(code, std::sync::atomic::Ordering::Relaxed);
                    cancel.cancel();
                }
                Ok(BridgeEnd::Detached) => cancel.cancel(),
                Ok(BridgeEnd::Hangup) | Err(_) => hangup.cancel(),
            }
        }))
    }

    fn end_attach(&mut self) {
        self.attach_lease.take();
        if let Ok(mut events) = self.container.events.lock() {
            events.push_back(
                crate::command::instance::feeds::ContainerSlotEvent::Exited {
                    step_name: ATTACH_STEP.to_string(),
                },
            );
        }
    }
    fn terminal_exit_code(&self) -> i32 {
        self.attach_exit_code
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}
