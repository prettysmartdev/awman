//! The automation launch wrapper around a single-agent PTY frontend.
//!
//! It is the one place an automation execution meets its runtime I/O:
//!
//! * stdout is decoded by the existing OSC 7501 scanner in receive order and
//!   folded into the OSC lifecycle reducer (rendered bytes are forwarded
//!   unchanged and never interpreted otherwise);
//! * direct (CLI/TUI) raw input passes the controller's manual-input gate;
//!   a lone Ctrl-C while input is gated cancels through the owned command;
//! * the controller's guarded writer is bound to the engine stdin channel;
//! * the terminal registration carries the initial snapshot atomically;
//! * the first-byte grace kill is replaced by the lifecycle deadline.

use std::sync::{Arc, Mutex};

use crate::data::automation::StartupTiming;
use crate::engine::agent::automation::osc7501::OscLifecycle;
use crate::engine::agent_runtime::execution::TerminalAutomationBinding;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::agent_runtime::program_status::Osc7501Scanner;

use super::controller::OwnedExecution;

pub struct AutomationFrontend {
    inner: Box<dyn AgentFrontend>,
    owned: OwnedExecution,
    osc_enabled: bool,
    running_reported: Arc<Mutex<bool>>,
}

impl AutomationFrontend {
    pub fn wrap(
        inner: Box<dyn AgentFrontend>,
        owned: OwnedExecution,
        osc_enabled: bool,
    ) -> Box<dyn AgentFrontend> {
        Box::new(Self {
            inner,
            owned,
            osc_enabled,
            running_reported: Arc::new(Mutex::new(false)),
        })
    }
}

impl crate::data::message::UserMessageSink for AutomationFrontend {
    fn write_message(&mut self, message: crate::data::message::UserMessage) {
        self.inner.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}

#[async_trait::async_trait]
impl AgentFrontend for AutomationFrontend {
    fn translated_output(&mut self, item: crate::engine::agent_output::Translated) -> bool {
        self.inner.translated_output(item)
    }
    fn output_redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        self.inner.output_redaction()
    }
    fn terminal_launch(&self) -> Option<crate::engine::agent_runtime::execution::TerminalLaunch> {
        self.inner.terminal_launch()
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        self.inner.terminal_source()
    }
    fn program_status_config(
        &self,
    ) -> crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
        self.inner.program_status_config()
    }
    fn report_status(&mut self, status: AgentStatus) {
        if matches!(status, AgentStatus::Running { .. }) {
            let mut reported = self
                .running_reported
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !*reported {
                *reported = true;
                self.owned.running();
            }
        }
        self.inner.report_status(status);
    }
    fn report_progress(&mut self, progress: AgentProgress) {
        self.inner.report_progress(progress);
    }
    fn take_io(&mut self) -> AgentIo {
        let inner = self.inner.take_io();
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        self.owned.bind_writer(stdin_tx.clone());

        // stdout: decode OSC 7501 in receive order, forward bytes unchanged.
        let forward = inner.stdout.clone();
        let owned = self.owned.clone();
        let mut lifecycle = OscLifecycle::new(owned.execution_id().clone(), self.osc_enabled);
        let register = self.owned.clone();
        let consumer = tokio::spawn(async move {
            let mut scanner = Osc7501Scanner::new();
            while let Some(bytes) = stdout_rx.recv().await {
                let observations: Vec<_> = scanner
                    .feed(&bytes)
                    .events
                    .iter()
                    .flat_map(|event| lifecycle.on_scan_event(event))
                    .collect();
                if !observations.is_empty() {
                    owned.observe(observations);
                }
                let _ = forward.send(bytes);
            }
            owned.observe(lifecycle.on_stream_end());
        });
        // The owner joins this consumer before freezing the result, so the
        // final working/done/error reports are never lost to a race.
        register.register_drain(consumer);

        // Direct raw input: the manual-input gate.
        let mut keystrokes = inner.stdin_rx;
        let gate_tx = stdin_tx.clone();
        let owned = self.owned.clone();
        tokio::spawn(async move {
            while let Some(bytes) = keystrokes.recv().await {
                match owned.manual_input() {
                    Ok(()) => {
                        if gate_tx.send(bytes).is_err() {
                            break;
                        }
                    }
                    // Cancel stays available while raw input is gated; it
                    // goes through the owned command cancellation, never as
                    // a raw interrupt byte.
                    Err(_) if bytes == [0x03] => owned.cancel(),
                    Err(_) => {}
                }
            }
        });

        // The sender the engine retains (stdin injection, a terminal-less
        // attach client, the bridge's own replies) passes the same manual
        // input gate; only the fixed OSC 7501 query reply is exempt. The
        // guarded writer pump alone writes to the raw channel.
        let (engine_tx, mut engine_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let raw = stdin_tx.clone();
        let owned = self.owned.clone();
        tokio::spawn(async move {
            while let Some(bytes) = engine_rx.recv().await {
                let exempt =
                    bytes == crate::engine::agent_runtime::program_status::OSC7501_QUERY_REPLY;
                if (exempt || owned.manual_input().is_ok()) && raw.send(bytes).is_err() {
                    break;
                }
            }
        });

        AgentIo {
            stdout: stdout_tx,
            stderr: inner.stderr,
            stdin_tx: engine_tx,
            stdin_rx,
            resize: inner.resize,
            initial_size: inner.initial_size,
        }
    }
    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        self.inner.set_agent_output_format(format);
    }
    fn grace_timeout(&self) -> std::time::Duration {
        self.inner.grace_timeout()
    }
    fn startup_timing(&self) -> StartupTiming {
        StartupTiming::LifecycleDeadline
    }
    fn terminal_automation(&self) -> Option<TerminalAutomationBinding> {
        let owned = self.owned.clone();
        Some(TerminalAutomationBinding {
            snapshot: self.owned.snapshot(),
            on_registered: Arc::new(move |id, hub| owned.bind_terminal(id, hub)),
        })
    }
    fn stuck_timeout(&self) -> std::time::Duration {
        self.inner.stuck_timeout()
    }
}

/// Headless automation (`exec prompt --headless --automation`): the shared
/// stuck detector's first-byte grace never kills the launch. The saved
/// automation startup deadline (native identity by then) is the only
/// startup clock; the command enforces it.
pub fn with_lifecycle_deadline(frontend: Box<dyn AgentFrontend>) -> Box<dyn AgentFrontend> {
    Box::new(LifecycleDeadlineFrontend { frontend })
}

struct LifecycleDeadlineFrontend {
    frontend: Box<dyn AgentFrontend>,
}

impl crate::data::message::UserMessageSink for LifecycleDeadlineFrontend {
    fn write_message(&mut self, message: crate::data::message::UserMessage) {
        self.frontend.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.frontend.replay_queued();
    }
    fn report_image_stale(&mut self, warning: &crate::data::execution_event::ImageStaleWarning) {
        self.frontend.report_image_stale(warning);
    }
}

#[async_trait::async_trait]
impl AgentFrontend for LifecycleDeadlineFrontend {
    fn translated_output(&mut self, item: crate::engine::agent_output::Translated) -> bool {
        self.frontend.translated_output(item)
    }
    fn output_redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        self.frontend.output_redaction()
    }
    fn terminal_launch(&self) -> Option<crate::engine::agent_runtime::execution::TerminalLaunch> {
        self.frontend.terminal_launch()
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        self.frontend.terminal_source()
    }
    fn program_status_config(
        &self,
    ) -> crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
        self.frontend.program_status_config()
    }
    fn report_status(&mut self, status: AgentStatus) {
        self.frontend.report_status(status);
    }
    fn report_progress(&mut self, progress: AgentProgress) {
        self.frontend.report_progress(progress);
    }
    fn take_io(&mut self) -> AgentIo {
        self.frontend.take_io()
    }
    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        self.frontend.set_agent_output_format(format);
    }
    fn grace_timeout(&self) -> std::time::Duration {
        self.frontend.grace_timeout()
    }
    fn startup_timing(&self) -> StartupTiming {
        StartupTiming::LifecycleDeadline
    }
    fn terminal_automation(&self) -> Option<TerminalAutomationBinding> {
        self.frontend.terminal_automation()
    }
    fn stuck_timeout(&self) -> std::time::Duration {
        self.frontend.stuck_timeout()
    }
}
