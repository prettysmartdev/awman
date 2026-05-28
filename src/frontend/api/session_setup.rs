//! Session setup bus and frontend — async session creation pipeline.
//!
//! `SessionSetupBus` follows the same broadcast pattern as the command
//! `EventBus` but is scoped to session setup lifecycle events.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use crate::data::session_setup_event::{
    ReadyStepEntry, SessionSetupError, SessionSetupEvent, SessionSetupState, SessionSetupStatus,
    SetupEventPayload,
};
use crate::engine::step_status::StepStatus;

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

    pub fn update_status(&self, status: SessionSetupStatus) {
        let mut state = self
            .current_state
            .write()
            .expect("session setup state lock poisoned");
        state.status = status;
    }

    pub fn update_stage(&self, stage: &str) {
        let mut state = self
            .current_state
            .write()
            .expect("session setup state lock poisoned");
        state.current_stage = Some(stage.to_string());
    }

    pub fn mark_failed(&self, stage: &str, message: &str) {
        let mut state = self
            .current_state
            .write()
            .expect("session setup state lock poisoned");
        state.status = SessionSetupStatus::Failed;
        state.current_stage = Some(format!("Failed: {message}"));
        state.error = Some(SessionSetupError {
            stage: stage.to_string(),
            message: message.to_string(),
        });
    }

    pub fn update_ready_step(&self, step: &str, status: StepStatus) {
        let mut state = self
            .current_state
            .write()
            .expect("session setup state lock poisoned");
        if let Some(entry) = state
            .ready_step_statuses
            .iter_mut()
            .find(|e| e.step == step)
        {
            entry.status = status;
        } else {
            state.ready_step_statuses.push(ReadyStepEntry {
                step: step.to_string(),
                status,
            });
        }
    }

    pub fn snapshot(&self) -> SessionSetupState {
        self.current_state
            .read()
            .expect("session setup state lock poisoned")
            .clone()
    }

    pub fn set_ready(&self, summary: crate::engine::ready::summary::ReadySummary) {
        let mut state = self
            .current_state
            .write()
            .expect("session setup state lock poisoned");
        state.status = SessionSetupStatus::Ready;
        state.ready_summary = Some(summary);
        state.current_stage = Some("Setup complete".to_string());
    }
}

// ─── SetupReadyFrontend ────────────────────────────────────────────────────

use async_trait::async_trait;

use crate::data::execution_event::EventPayload;
use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::frontend::api::event_bus::EventBusSender;

/// Bridges the `ReadyFrontend` trait (from the ready engine) to the
/// `SessionSetupBus` during async session setup.
///
/// Every event observed here is also mirrored to the tracing log with the
/// session-id prefix so operators can grep the API server log file for a
/// single session's full setup output (including container build lines).
pub struct SetupReadyFrontend {
    bus: SessionSetupBusSender,
    event_bus: EventBusSender,
    /// Short session-id prefix used as the line tag in the API log file.
    session_prefix: String,
}

impl SetupReadyFrontend {
    pub fn new(session_id: &str, bus: SessionSetupBusSender, event_bus: EventBusSender) -> Self {
        Self {
            bus,
            event_bus,
            session_prefix: session_log_prefix(session_id),
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
        log_setup_line(&self.session_prefix, &format!("[{phase}] {}", msg.text));
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }

    fn replay_queued(&mut self) {}
}

impl ReadyFrontend for SetupReadyFrontend {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> {
        Ok(true)
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        let message = ready_phase_display(phase);
        log_setup_line(
            &self.session_prefix,
            &format!("phase: {phase:?} — {message}"),
        );
        {
            let mut state = self
                .bus
                .current_state
                .write()
                .expect("session setup state lock poisoned");
            state.status = SessionSetupStatus::RunningReady;
            state.current_ready_phase = Some(phase.clone());
            state.current_stage = Some(message.clone());
        }
        self.bus.emit(SetupEventPayload::ReadyPhaseChanged {
            phase: phase.clone(),
            message,
        });
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        log_setup_line(
            &self.session_prefix,
            &format!("step: {step} → {}", format_step_status(&status)),
        );
        {
            let mut state = self
                .bus
                .current_state
                .write()
                .expect("session setup state lock poisoned");
            if let Some(entry) = state
                .ready_step_statuses
                .iter_mut()
                .find(|e| e.step == step)
            {
                entry.status = status.clone();
            } else {
                state.ready_step_statuses.push(ReadyStepEntry {
                    step: step.to_string(),
                    status: status.clone(),
                });
            }
        }
        self.bus.emit(SetupEventPayload::ReadyStepStatus {
            step: step.to_string(),
            status,
        });
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        log_setup_line(&self.session_prefix, &format!("ready summary: {summary:?}"));
        {
            let mut state = self
                .bus
                .current_state
                .write()
                .expect("session setup state lock poisoned");
            state.status = SessionSetupStatus::Ready;
            state.ready_summary = Some(summary.clone());
            state.current_stage = Some("Setup complete".to_string());
        }
        self.bus.emit(SetupEventPayload::SetupComplete {
            ready_summary: Box::new(summary.clone()),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(SetupContainerSink {
            event_bus: self.event_bus.clone(),
            session_prefix: self.session_prefix.clone(),
            line_buffer_stdout: String::new(),
            line_buffer_stderr: String::new(),
        })
    }
}

fn ready_phase_display(phase: &ReadyPhase) -> String {
    match phase {
        ReadyPhase::Preflight => "Running preflight checks...".into(),
        ReadyPhase::AwaitingDockerfileDecision => "Checking Dockerfile...".into(),
        ReadyPhase::CreatingDockerfile => "Creating Dockerfile.dev...".into(),
        ReadyPhase::BuildingBaseImage => "Building base image...".into(),
        ReadyPhase::BuildingAgentImage => "Building agent image...".into(),
        ReadyPhase::CheckingNonDefaultAgents => "Checking non-default agent images...".into(),
        ReadyPhase::CheckingLocalAgent => "Checking local agent...".into(),
        ReadyPhase::RunningAudit => "Running audit...".into(),
        ReadyPhase::RebuildingAfterAudit => "Rebuilding after audit...".into(),
        ReadyPhase::Complete => "Ready checks complete".into(),
        ReadyPhase::Failed(f) => format!("Failed: {}", f.message),
    }
}

// ─── SetupContainerSink ────────────────────────────────────────────────────

/// Standalone container frontend for use within `SetupReadyFrontend`.
/// Emits execution events (stdout/stderr lines, status messages) to the
/// `EventBusSender`. Mirrors the pattern of `ApiContainerSink` in
/// `command_frontend.rs`.
struct SetupContainerSink {
    event_bus: EventBusSender,
    session_prefix: String,
    line_buffer_stdout: String,
    line_buffer_stderr: String,
}

impl UserMessageSink for SetupContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        let phase = match msg.level {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warn",
            MessageLevel::Error => "error",
            MessageLevel::Success => "ok",
        };
        log_setup_line(
            &self.session_prefix,
            &format!("container [{phase}] {}", msg.text),
        );
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }

    fn replay_queued(&mut self) {}
}

#[async_trait]
impl ContainerFrontend for SetupContainerSink {
    fn report_status(&mut self, status: ContainerStatus) {
        let message = match &status {
            ContainerStatus::Building => "Building container image...".to_string(),
            ContainerStatus::Pulling => "Pulling container image...".to_string(),
            ContainerStatus::Starting => "Starting container...".to_string(),
            ContainerStatus::Running { container_name } => {
                format!("Container running: {container_name}")
            }
            ContainerStatus::Stopping => "Stopping container...".to_string(),
            ContainerStatus::Exited(code) => format!("Container exited with code {code}"),
            ContainerStatus::Failed(reason) => format!("Container failed: {reason}"),
        };
        log_setup_line(&self.session_prefix, &format!("container: {message}"));
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }

    fn report_progress(&mut self, progress: ContainerProgress) {
        log_setup_line(
            &self.session_prefix,
            &format!("container [{}] {}", progress.stage, progress.message),
        );
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_container_io(&mut self) -> crate::engine::container::frontend::ContainerIo {
        // Drain stdout/stderr into the tracing log so the API log file mirrors
        // the byte-stream output the CLI/TUI would see for the ready container.
        // Lines are tagged with the session prefix; partial lines are buffered
        // by `forward_container_stream_to_tracing` and emitted at line breaks
        // (plus a final flush when the channel closes).
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        forward_container_stream_to_tracing(
            self.session_prefix.clone(),
            "stdout".into(),
            stdout_rx,
        );
        forward_container_stream_to_tracing(
            self.session_prefix.clone(),
            "stderr".into(),
            stderr_rx,
        );
        crate::engine::container::frontend::ContainerIo {
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
    session_prefix: String,
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
                    log_setup_line(
                        &session_prefix,
                        &format!("container.{stream_name}: {trimmed}"),
                    );
                }
            }
        }
        if !buf.is_empty() {
            let s = String::from_utf8_lossy(&buf);
            let trimmed = s.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                log_setup_line(
                    &session_prefix,
                    &format!("container.{stream_name}: {trimmed}"),
                );
            }
        }
    });
}

// ─── Logging helpers ───────────────────────────────────────────────────────

/// First 8 characters of the session id. Short enough to grep, long enough to
/// disambiguate when multiple sessions run concurrently.
pub(crate) fn session_log_prefix(session_id: &str) -> String {
    let end = session_id.len().min(8);
    session_id[..end].to_string()
}

/// Write a line to the tracing log tagged with the session prefix. Level is
/// `info` by default (so it ends up in the API server log file) and `debug`
/// when `AWMAN_API_VERBOSE_SETUP` is set to a falsy value, letting operators
/// silence per-session chatter while keeping it available via `RUST_LOG`.
pub(crate) fn log_setup_line(session_prefix: &str, line: &str) {
    if verbose_setup_enabled() {
        tracing::info!(target: "awman::api::session_setup", "[{session_prefix}] {line}");
    } else {
        tracing::debug!(target: "awman::api::session_setup", "[{session_prefix}] {line}");
    }
}

/// Returns `true` when verbose setup logging is enabled (the default).
/// Disabled when `AWMAN_API_VERBOSE_SETUP` is one of `0`, `false`, `no`, `off`.
pub(crate) fn verbose_setup_enabled() -> bool {
    match std::env::var("AWMAN_API_VERBOSE_SETUP") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// `UserMessageSink` that mirrors every message to the API setup tracing log.
/// Used by the session-setup task to capture the full output of git commands
/// (clone, branch checkout, etc.) into the API server's log file with the
/// session-id prefix that downstream tooling greps for.
pub struct TracingSetupSink {
    session_prefix: String,
}

impl TracingSetupSink {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_prefix: session_log_prefix(session_id),
        }
    }
}

impl UserMessageSink for TracingSetupSink {
    fn write_message(&mut self, msg: UserMessage) {
        let level = match msg.level {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warn",
            MessageLevel::Error => "error",
            MessageLevel::Success => "ok",
        };
        log_setup_line(&self.session_prefix, &format!("git [{level}] {}", msg.text));
    }

    fn replay_queued(&mut self) {}
}

/// Public re-export so route handlers can write a setup-context line to the
/// API log file using the same prefix and gating as the rest of session
/// setup (state transitions, ready output, etc.).
pub fn log_session_setup(session_id: &str, line: &str) {
    log_setup_line(&session_log_prefix(session_id), line);
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
