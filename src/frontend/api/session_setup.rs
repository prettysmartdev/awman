//! Session setup bus and frontend — async session creation pipeline.
//!
//! `SessionSetupBus` follows the same broadcast pattern as the command
//! `EventBus` but is scoped to session setup lifecycle events.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use crate::data::session_setup_event::{
    ReadyStepEntry, SessionSetupError, SessionSetupEvent, SessionSetupState,
    SessionSetupStatus, SetupEventPayload,
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
use crate::data::session::AgentName;
use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::frontend::api::event_bus::EventBusSender;

/// Bridges the `ReadyFrontend` trait (from the ready engine) to the
/// `SessionSetupBus` during async session setup.
pub struct SetupReadyFrontend {
    bus: SessionSetupBusSender,
    event_bus: EventBusSender,
}

impl SetupReadyFrontend {
    pub fn new(bus: SessionSetupBusSender, event_bus: EventBusSender) -> Self {
        Self { bus, event_bus }
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

    fn ask_migrate_legacy_layout(&mut self, _agent_name: &AgentName) -> Result<bool, EngineError> {
        Ok(true)
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        let message = ready_phase_display(phase);
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
            ready_summary: summary.clone(),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(SetupContainerSink {
            event_bus: self.event_bus.clone(),
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
        ReadyPhase::AwaitingLegacyMigrationDecision => "Checking for legacy layout...".into(),
        ReadyPhase::MigratingLegacyLayout => "Migrating legacy layout...".into(),
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
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }

    fn report_progress(&mut self, progress: ContainerProgress) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_container_io(&mut self) -> crate::engine::container::frontend::ContainerIo {
        let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::engine::container::frontend::ContainerIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}
