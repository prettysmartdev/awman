//! Session setup bus and frontend — async session creation pipeline.
//!
//! `SessionSetupBus` follows the same broadcast pattern as the command
//! `EventBus` but is scoped to session setup lifecycle events.
//!
//! It is a *pure broadcaster* (WI 0114 F-41): it sends events and hands out
//! the shared [`SessionSetupState`] handle, but every transition of that state
//! is a method on the Layer 0 type. Nothing here decides what a transition
//! means.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use crate::data::session_setup_event::{SessionSetupEvent, SessionSetupState, SetupEventPayload};
use crate::data::step_status::StepStatus;

/// Aggregate state used by both sync ReadyFrontend callbacks (running on the
/// async setup task) and async HTTP handlers. `std::sync::RwLock` is used
/// rather than `tokio::sync::RwLock` because the sync ReadyFrontend trait
/// methods cannot `.await`. Locks are held only briefly (a single field
/// mutation) so blocking the thread is fine.
pub struct SessionSetupBus {
    tx: broadcast::Sender<SessionSetupEvent>,
    sequence: Arc<AtomicU64>,
    pub current_state: Arc<RwLock<SessionSetupState>>,
}

impl SessionSetupBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            sequence: Arc::new(AtomicU64::new(0)),
            current_state: Arc::new(RwLock::new(SessionSetupState::new())),
        }
    }

    pub fn sender(&self) -> SessionSetupBusSender {
        SessionSetupBusSender {
            tx: self.tx.clone(),
            sequence: Arc::clone(&self.sequence),
            current_state: Arc::clone(&self.current_state),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionSetupEvent> {
        self.tx.subscribe()
    }

    /// Read a snapshot of the current state. Cheap clone of the inner struct.
    pub fn snapshot(&self) -> SessionSetupState {
        self.current_state
            .read()
            .expect("session setup state lock poisoned")
            .clone()
    }
}

#[derive(Clone)]
pub struct SessionSetupBusSender {
    tx: broadcast::Sender<SessionSetupEvent>,
    sequence: Arc<AtomicU64>,
    current_state: Arc<RwLock<SessionSetupState>>,
}

impl SessionSetupBusSender {
    pub fn emit(&self, payload: SetupEventPayload) {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        let event = SessionSetupEvent {
            timestamp: chrono::Utc::now(),
            sequence: seq,
            payload,
        };
        let _ = self.tx.send(event);
    }

    pub fn snapshot(&self) -> SessionSetupState {
        self.current_state
            .read()
            .expect("session setup state lock poisoned")
            .clone()
    }

    /// Apply one Layer 0 transition to the shared state. The sender holds the
    /// lock and nothing else; the transition itself is a
    /// [`SessionSetupState`] method.
    fn transition<T>(&self, apply: impl FnOnce(&mut SessionSetupState) -> T) -> T {
        let mut state = self
            .current_state
            .write()
            .expect("session setup state lock poisoned");
        apply(&mut state)
    }
}

// ─── SetupReadyFrontend ────────────────────────────────────────────────────

use async_trait::async_trait;

use super::event_bus::EventBusSender;
use crate::data::execution_event::EventPayload;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::ready_phase::ReadyPhase;
use crate::data::ready_summary::ReadySummary;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::ready::frontend::ReadyFrontend;

/// Bridges the `ReadyFrontend` trait (from the ready engine) to the
/// `SessionSetupBus` during async session setup.
///
/// Every event observed here is also mirrored to the tracing log with the
/// session-id prefix so operators can grep the API server log file for a
/// single session's full setup output (including container build lines).
pub struct SetupReadyFrontend {
    bus: SessionSetupBusSender,
    event_bus: EventBusSender,
    /// Session-tagged API log writer (prefix + verbosity).
    log: SetupLog,
}

impl SetupReadyFrontend {
    /// `verbose` is `EnvSnapshot::api_verbose_setup()`, read once by the
    /// caller: Layer 0 owns the variable (F-37).
    pub fn new(
        session_id: &str,
        bus: SessionSetupBusSender,
        event_bus: EventBusSender,
        verbose: bool,
    ) -> Self {
        Self {
            bus,
            event_bus,
            log: SetupLog::new(session_id, verbose),
        }
    }
}

impl UserMessageSink for SetupReadyFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        let phase = match msg.level {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warn",
            MessageLevel::Error => "error",
            MessageLevel::Success => "ok",
        };
        self.log.line(&format!("[{phase}] {}", msg.text));
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }

    fn replay_queued(&mut self) {}
}

impl ReadyFrontend for SetupReadyFrontend {
    fn ask_create_dockerfile(
        &mut self,
        _dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        Ok(crate::command::headless::HeadlessDefaults::api().create_dockerfile())
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(crate::command::headless::HeadlessDefaults::api().run_audit_on_template())
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        let message = self.bus.transition(|s| s.apply_ready_phase(phase));
        self.log.line(&format!("phase: {phase:?} — {message}"));
        self.bus.emit(SetupEventPayload::ReadyPhaseChanged {
            phase: phase.clone(),
            message,
        });
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        self.log.line(&format!("ready summary: {summary:?}"));
        self.bus.transition(|s| s.set_ready(summary.clone()));
        self.bus.emit(SetupEventPayload::SetupComplete {
            ready_summary: Box::new(summary.clone()),
        });
    }
}

impl crate::engine::agent::AgentImageFrontend for SetupReadyFrontend {
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: StepStatus,
    ) {
        // The setup state and the wire payload are both keyed by the step's
        // rendered label, so it is rendered once here and passed on unchanged.
        // F-45 kept `SetupStep`'s `Display` byte-identical to the `&str` keys
        // the engines used to pass, so neither the log line nor the event
        // stream changes.
        let step = step.to_string();
        self.log
            .line(&format!("step: {step} → {}", format_step_status(&status)));
        self.bus
            .transition(|s| s.apply_ready_step(&step, status.clone()));
        self.bus
            .emit(SetupEventPayload::ReadyStepStatus { step, status });
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(SetupContainerSink {
            event_bus: self.event_bus.clone(),
            log: self.log.clone(),
        })
    }
}

// ─── SetupContainerSink ────────────────────────────────────────────────────

/// Standalone container frontend for use within `SetupReadyFrontend`.
/// Emits execution events (stdout/stderr lines, status messages) to the
/// `EventBusSender`. Mirrors the pattern of `ApiContainerSink` in
/// `command_frontend.rs`.
struct SetupContainerSink {
    event_bus: EventBusSender,
    log: SetupLog,
}

impl UserMessageSink for SetupContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        let phase = match msg.level {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warn",
            MessageLevel::Error => "error",
            MessageLevel::Success => "ok",
        };
        self.log.line(&format!("container [{phase}] {}", msg.text));
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }

    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for SetupContainerSink {
    fn report_status(&mut self, status: AgentStatus) {
        let message = match &status {
            AgentStatus::Building => "Building container image...".to_string(),
            AgentStatus::Pulling => "Pulling container image...".to_string(),
            AgentStatus::Starting => "Starting container...".to_string(),
            AgentStatus::Running { container_name } => {
                format!("Container running: {container_name}")
            }
            AgentStatus::Stopping => "Stopping container...".to_string(),
            AgentStatus::Exited(code) => format!("Container exited with code {code}"),
            AgentStatus::Failed(reason) => format!("Container failed: {reason}"),
        };
        self.log.line(&format!("container: {message}"));
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        self.log.line(&format!(
            "container [{}] {}",
            progress.stage, progress.message
        ));
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        // Drain stdout/stderr into the tracing log so the API log file mirrors
        // the byte-stream output the CLI/TUI would see for the ready container.
        // Lines are tagged with the session prefix; partial lines are buffered
        // by `forward_container_stream_to_tracing` and emitted at line breaks
        // (plus a final flush when the channel closes).
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        forward_container_stream_to_tracing(self.log.clone(), "stdout".into(), stdout_rx);
        forward_container_stream_to_tracing(self.log.clone(), "stderr".into(), stderr_rx);
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }

    fn grace_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(15 * 60)
    }
}

/// Spawn a task that buffers bytes by line and writes each line to the
/// tracing log with the session prefix.
fn forward_container_stream_to_tracing(
    log: SetupLog,
    stream_name: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        while let Some(chunk) = rx.recv().await {
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let s = String::from_utf8_lossy(&line[..line.len() - 1]);
                let trimmed = s.trim_end_matches('\r');
                if !trimmed.is_empty() {
                    log.line(&format!("container.{stream_name}: {trimmed}"));
                }
            }
        }
        if !buf.is_empty() {
            let s = String::from_utf8_lossy(&buf);
            let trimmed = s.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                log.line(&format!("container.{stream_name}: {trimmed}"));
            }
        }
    });
}

// ─── Logging helpers ───────────────────────────────────────────────────────

/// The API log tag for one session: the grep-able session prefix, plus
/// whether setup chatter is logged at `info` or demoted to `debug`.
///
/// The verbosity comes in as a bool rather than being read here: the variable
/// (`AWMAN_API_VERBOSE_SETUP`) is declared and parsed in Layer 0 by
/// [`EnvSnapshot::api_verbose_setup`], and nothing above Layer 0 reads the
/// environment directly (F-37).
///
/// [`EnvSnapshot::api_verbose_setup`]: crate::data::config::env::EnvSnapshot::api_verbose_setup
#[derive(Debug, Clone)]
pub struct SetupLog {
    prefix: String,
    verbose: bool,
}

impl SetupLog {
    pub fn new(session_id: &str, verbose: bool) -> Self {
        Self {
            prefix: session_log_prefix(session_id),
            verbose,
        }
    }

    /// Write one line, tagged with the session prefix. `info` when verbose (the
    /// default), `debug` otherwise — so an operator who silences per-session
    /// chatter can still reach it through `RUST_LOG`.
    pub fn line(&self, line: &str) {
        if self.verbose {
            tracing::info!(target: "awman::api::session_setup", "[{}] {line}", self.prefix);
        } else {
            tracing::debug!(target: "awman::api::session_setup", "[{}] {line}", self.prefix);
        }
    }
}

/// First 8 characters of the session id. Short enough to grep, long enough to
/// disambiguate when multiple sessions run concurrently.
pub(crate) fn session_log_prefix(session_id: &str) -> String {
    let end = session_id.len().min(8);
    session_id[..end].to_string()
}

/// `UserMessageSink` that mirrors every message to the API setup tracing log.
/// Used by the session-setup task to capture the full output of git commands
/// (clone, branch checkout, etc.) into the API server's log file with the
/// session-id prefix that downstream tooling greps for.
pub struct TracingSetupSink {
    log: SetupLog,
}

impl TracingSetupSink {
    /// `verbose` is `EnvSnapshot::api_verbose_setup()`, read once by the
    /// caller: Layer 0 owns the variable (F-37).
    pub fn new(session_id: &str, verbose: bool) -> Self {
        Self {
            log: SetupLog::new(session_id, verbose),
        }
    }
}

/// F-45: takes the default `command_started`, so the `$ git …` echo line
/// is byte-identical to the one `run_git_logged` composed before.
impl crate::engine::git::GitFrontend for TracingSetupSink {}

impl UserMessageSink for TracingSetupSink {
    fn write_message(&mut self, msg: UserMessage) {
        let level = match msg.level {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warn",
            MessageLevel::Error => "error",
            MessageLevel::Success => "ok",
        };
        self.log.line(&format!("git [{level}] {}", msg.text));
    }

    fn replay_queued(&mut self) {}
}

/// Public re-export so route handlers can write a setup-context line to the
/// API log file using the same prefix and gating as the rest of session
/// setup (state transitions, ready output, etc.).
pub fn log_session_setup(session_id: &str, verbose: bool, line: &str) {
    SetupLog::new(session_id, verbose).line(line);
}

fn format_step_status(status: &StepStatus) -> String {
    match status {
        StepStatus::Pending => "pending".into(),
        StepStatus::Running => "running".into(),
        StepStatus::Done => "done".into(),
        StepStatus::Skipped => "skipped".into(),
        StepStatus::Warn(s) => format!("warn({s})"),
        StepStatus::Failed(s) => format!("failed({s})"),
    }
}
