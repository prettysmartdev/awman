//! TUI presentation for `awman attach` (WI 0141): the terminal stream is
//! rendered in a container slot's `vt100` window, exactly as `squad attach`
//! renders today, so no raw agent byte reaches the host terminal.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::command::commands::attach::{initial_client_frames, AttachCommandFrontend};
use crate::command::error::CommandError;
use crate::data::ids::TerminalId;
use crate::data::terminal::HostOutputPolicy;
use crate::data::terminal::{DetachReason, TerminalClientFrame, TerminalFrame};
use crate::engine::remote::terminal_client::TerminalStream;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

#[async_trait]
impl AttachCommandFrontend for TuiCommandFrontend {
    fn begin_attach(&mut self, _terminal_id: &TerminalId) -> CancellationToken {
        self.attach_hangup = CancellationToken::new();
        if let Ok(mut events) = self.container.events.lock() {
            events.clear();
            events.push_back(crate::frontend::tui::tabs::ContainerSlotEvent::Exited {
                step_name: String::new(),
            });
        }
        self.command_cancel.clone()
    }

    fn hangup(&self) -> CancellationToken {
        self.attach_hangup.clone()
    }

    fn bind_terminal(
        &mut self,
        mut stream: TerminalStream,
        read_only: bool,
        policy: HostOutputPolicy,
    ) -> Result<tokio::task::JoinHandle<()>, CommandError> {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let (cols, rows) = crossterm::terminal::size()
            .map(|(cols, rows)| {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            })
            .unwrap_or((80, 24));
        let io = crate::frontend::tui::tabs::ContainerSlotIo {
            stdout_rx,
            stdin_tx,
            resize_tx: resize_tx.clone(),
        };
        if let Ok(mut events) = self.container.events.lock() {
            events.push_back(crate::frontend::tui::tabs::ContainerSlotEvent::Launched {
                step_name: "attach".to_string(),
                agent: "attached agent".to_string(),
                model: None,
                io: Some(io),
            });
            events.push_back(
                crate::frontend::tui::tabs::ContainerSlotEvent::ContainerName {
                    step_name: "attach".to_string(),
                    container_name: "attached terminal".to_string(),
                },
            );
        }
        let cancel = self.command_cancel.clone();
        let events = self.container.events.clone();
        let exit_code = self.attach_exit_code.clone();
        let hangup = self.attach_hangup.clone();
        exit_code.store(0, std::sync::atomic::Ordering::Relaxed);
        let messages = self.messages.clone();
        Ok(tokio::spawn(async move {
            let mut writable = !read_only;
            let mut reason = DetachReason::User;
            let mut filter = crate::frontend::terminal_output_filter::HostOutputFilter::new(policy);
            for frame in initial_client_frames(read_only, Some((cols, rows))) {
                let _ = stream.send(frame).await;
            }
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = stream.send(TerminalClientFrame::Detached { reason: DetachReason::User }).await;
                        break;
                    }
                    Some(bytes) = stdin_rx.recv() => {
                        if writable { let _ = stream.send(TerminalClientFrame::Input { bytes }).await; }
                    }
                    Some((cols, rows)) = resize_rx.recv() => {
                        if writable { let _ = stream.send(TerminalClientFrame::Resize { cols, rows }).await; }
                    }
                    frame = stream.next() => match frame {
                        Ok(Some(TerminalFrame::Snapshot { screen, .. } | TerminalFrame::Output { bytes: screen })) => {
                            let mut safe = Vec::new();
                            filter.feed(&screen, &mut safe);
                            if !safe.is_empty() && stdout_tx.send(safe).is_err() { reason = DetachReason::Hangup; break; }
                        }
                        Ok(Some(TerminalFrame::Exit { code })) => { exit_code.store(code, std::sync::atomic::Ordering::Relaxed); break; }
                        Ok(None) | Err(_) => { reason = DetachReason::Hangup; break; }
                        Ok(Some(TerminalFrame::Error { code, .. })) => {
                            if writable && matches!(code.as_str(), "control_held" | "input_not_allowed") {
                                writable = false;
                                use crate::data::message::UserMessageSink;
                                let mut messages = messages.clone();
                                messages.write_message(crate::data::message::UserMessage { level: crate::data::message::MessageLevel::Warning, text: "terminal control unavailable; watching read-only".into() });
                            }
                        }
                        Ok(Some(TerminalFrame::Resize { .. } | TerminalFrame::Lagged)) => {}
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
            if reason == DetachReason::Hangup {
                hangup.cancel();
            } else {
                cancel.cancel();
            }
            if let Ok(mut events) = events.lock() {
                events.push_back(
                    crate::frontend::tui::tabs::ContainerSlotEvent::AttachedExited {
                        step_name: "attach".to_string(),
                        container_name: "attached terminal".to_string(),
                    },
                );
            }
        }))
    }

    fn end_attach(&mut self) {
        if let Ok(mut events) = self.container.events.lock() {
            events.push_back(crate::frontend::tui::tabs::ContainerSlotEvent::Exited {
                step_name: "attach".to_string(),
            });
        }
    }
    fn terminal_exit_code(&self) -> i32 {
        self.attach_exit_code
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}
