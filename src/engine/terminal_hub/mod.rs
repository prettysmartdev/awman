//! Transport-neutral daemon terminals: screen mirrors, bounded fan-out and
//! exclusive input grants. Layer 2 owns admission, identity and event logging.
//!
//! All synchronous operations serialize on one mutex; no lock crosses an
//! await. The PTY resize callback runs synchronously under that mutex to keep
//! PTY sizes and frame ordering consistent. It must be short and must not call
//! back into the hub. Bridges report output/exit through `TerminalHandle`.

// WI 0142: registers engine agent channels as hub terminals (filled by engine-feature-layer1).
pub mod bridge;
mod mirror;
mod registry;
mod subscription;

use std::sync::{Arc, Mutex, Weak};

use chrono::Utc;
use tokio::sync::broadcast;

use crate::data::ids::TerminalId;
use crate::data::terminal::{
    TerminalFrame, TerminalOwner, TerminalOwnerInfo, TerminalRecord, TerminalSnapshot,
    TerminalState,
};

pub use mirror::{fit_snapshot, RingBuffer, RingReplay, ScreenMirror, REPLAY_TRIM_WINDOW};
pub use subscription::TerminalSubscription;

use registry::{HubMessage, HubState, TerminalEntry};

/// Output chunks are split before entering the frame ring, bounding retained
/// bytes independently of the size of a bridge's read buffer.
pub const MAX_OUTPUT_FRAME_BYTES: usize = 16 * 1024;

/// Largest screen a mirror (and the PTY behind it) is sized to. Resize
/// requests come from untrusted viewers: anything larger is clamped before
/// vt100 allocates the grid or the PTY callback runs, so one frame cannot
/// ask for billions of cells (at most 1000 x 500 = 500 000 cells).
pub const MAX_TERMINAL_COLS: u16 = 1000;
pub const MAX_TERMINAL_ROWS: u16 = 500;

/// Input is queued to the PTY writer in chunks of at most this many bytes.
pub const MAX_INPUT_CHUNK_BYTES: usize = 4 * 1024;

/// Chunks a terminal's stdin queue holds ([`input_channel`]): at most
/// 256 KiB of input waits for an agent that stopped reading. Further input is
/// refused with [`TerminalHubError::InputBackpressure`] until it drains.
pub const INPUT_QUEUE_CHUNKS: usize = 64;

/// The bounded stdin queue a registration hands the hub. The receiver must
/// be drained by the PTY writer itself (not forwarded into an unbounded
/// queue), so a stalled agent pushes back on viewers.
pub fn input_channel() -> (
    tokio::sync::mpsc::Sender<Vec<u8>>,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    tokio::sync::mpsc::channel(INPUT_QUEUE_CHUNKS)
}

/// Queue `bytes` whole or not at all: a frame is never half-sent.
fn queue_input(
    stdin: &tokio::sync::mpsc::Sender<Vec<u8>>,
    bytes: Vec<u8>,
) -> Result<(), TerminalHubError> {
    use tokio::sync::mpsc::error::TrySendError;
    if stdin.is_closed() {
        return Err(TerminalHubError::Closed);
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let chunks = bytes.len().div_ceil(MAX_INPUT_CHUNK_BYTES);
    // The hub's sender is only used under the hub mutex, so capacity cannot
    // shrink between this check and the sends below.
    if stdin.capacity() < chunks {
        return Err(TerminalHubError::InputBackpressure);
    }
    for chunk in bytes.chunks(MAX_INPUT_CHUNK_BYTES) {
        stdin
            .try_send(chunk.to_vec())
            .map_err(|error| match error {
                TrySendError::Full(_) => TerminalHubError::InputBackpressure,
                TrySendError::Closed(_) => TerminalHubError::Closed,
            })?;
    }
    Ok(())
}

/// Sizing knobs. Defaults suit the daemon; tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalHubConfig {
    /// Raw output bytes retained per terminal. Default 256 KiB.
    pub ring_bytes: usize,
    /// Broadcast frames retained per terminal (Tokio rounds up to a power of
    /// two; zero is treated as one). Default 256.
    pub subscriber_capacity: usize,
    /// Finished records and recent forgotten-id tombstones retained,
    /// independently. Zero keeps no finished records and one tombstone.
    pub max_finished: usize,
    /// Mirror history beyond the visible screen. Default zero.
    pub scrollback_lines: usize,
}

impl Default for TerminalHubConfig {
    fn default() -> Self {
        Self {
            ring_bytes: 256 * 1024,
            subscriber_capacity: 256,
            max_finished: 256,
            scrollback_lines: 0,
        }
    }
}

/// What a viewer asks for when it subscribes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeOptions {
    /// Replay the retained raw output (at most `ring_bytes`) as `History`
    /// frames before the first snapshot (WI 0105). Lag recovery and
    /// execution rebinds never replay it.
    pub history: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminalHubError {
    #[error("terminal not found")]
    NotFound,
    #[error("terminal gone")]
    Gone,
    #[error("terminal closed")]
    Closed,
    #[error("another viewer holds terminal control")]
    ControlHeld,
    /// The agent is not reading its input; the bounded queue is full.
    #[error("terminal input queue is full")]
    InputBackpressure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalHubEvent {
    Opened(Box<TerminalRecord>),
    Closed {
        id: TerminalId,
        exit_code: i32,
        source: Option<crate::data::execution_event::EventSource>,
    },
    Forgotten(TerminalId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRegistration {
    pub automation: Option<crate::data::automation_records::AutomationSnapshot>,
    pub owner: TerminalOwnerInfo,
    pub agent: String,
    pub model: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// The bridge's stdin injector and PTY resize operation. `stdin` is bounded
/// (build it with [`input_channel`]) so viewer input cannot grow without
/// limit while the agent is not reading.
pub struct TerminalInput {
    pub stdin: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Called synchronously under the hub lock; must not reenter the hub.
    pub resize: Option<Arc<dyn Fn(u16, u16) + Send + Sync>>,
}

impl std::fmt::Debug for TerminalInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalInput")
            .field("resize", &self.resize.is_some())
            .finish()
    }
}

/// Bridge-side lifecycle handle. Clones do not close the terminal on drop:
/// the wait path must report the actual exit code explicitly, exactly once.
#[derive(Debug, Clone)]
pub struct TerminalHandle {
    id: TerminalId,
    hub: Weak<TerminalHub>,
    generation: u64,
}

impl TerminalHandle {
    pub fn id(&self) -> &TerminalId {
        &self.id
    }

    pub fn push_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let mut state = hub
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Ok(entry) = state.open_entry_mut(&self.id) else {
            return;
        };
        if entry.generation != self.generation {
            return;
        }
        entry.mirror.feed(bytes);
        if let Some(output) = &entry.output {
            if output.receiver_count() > 0 {
                for chunk in bytes.chunks(MAX_OUTPUT_FRAME_BYTES) {
                    let _ = output.send(HubMessage::Frame(Arc::new(TerminalFrame::Output {
                        bytes: chunk.to_vec(),
                    })));
                }
            }
        }
    }

    /// Report an already-applied PTY resize. Only calls the PTY callback to
    /// correct dimensions below 2x2. Does not change the fallback size.
    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(hub) = self.hub.upgrade() {
            let mut state = hub
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Ok(entry) = state.open_entry_mut(&self.id) {
                if entry.generation == self.generation {
                    entry.resize(cols, rows, false);
                }
            }
        }
    }

    pub fn exit(&self, code: i32) {
        if let Some(hub) = self.hub.upgrade() {
            let mut state = hub
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Ok(entry) = state.open_entry_mut(&self.id) else {
                return;
            };
            if entry.generation != self.generation {
                return;
            }
            if entry.deferred {
                entry.input = None;
                entry.holder = None;
                return;
            }
            hub.close_locked(&mut state, &self.id, code);
            hub.trim_finished(&mut state);
        }
    }
}

/// An exclusive, unforgeable input capability. Layer 2 admits the viewer
/// before taking it and retains it for that viewer's lifetime. Separate local
/// viewers (or viewers sharing an actor name) still get distinct capabilities.
/// Release explicitly or drop on disconnect; both restore the fallback size.
/// Exit revokes the capability. Acquiring it makes no policy/logging decision.
#[derive(Debug)]
pub struct InputGrant {
    id: TerminalId,
    token: u64,
    hub: Weak<TerminalHub>,
}

impl InputGrant {
    pub fn terminal_id(&self) -> &TerminalId {
        &self.id
    }

    pub fn send_input(&self, bytes: Vec<u8>) -> Result<(), TerminalHubError> {
        let hub = self.hub.upgrade().ok_or(TerminalHubError::Closed)?;
        let mut state = hub
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = state.open_entry_mut(&self.id)?;
        if entry.holder != Some(self.token) {
            return Err(TerminalHubError::Closed);
        }
        queue_input(
            &entry.input.as_ref().ok_or(TerminalHubError::Closed)?.stdin,
            bytes,
        )
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), TerminalHubError> {
        let hub = self.hub.upgrade().ok_or(TerminalHubError::Closed)?;
        let mut state = hub
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = state.open_entry_mut(&self.id)?;
        if entry.holder != Some(self.token) {
            return Err(TerminalHubError::Closed);
        }
        entry.resize(cols, rows, true);
        Ok(())
    }

    /// Read-only fencing identity; never creates a capability.
    pub fn epoch(&self) -> u64 {
        self.token
    }
    pub fn is_valid(&self) -> bool {
        self.hub.upgrade().is_some_and(|hub| {
            let state = hub.state.lock().unwrap_or_else(|p| p.into_inner());
            state
                .entry(&self.id)
                .is_ok_and(|e| e.input.is_some() && e.holder == Some(self.token))
        })
    }
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for InputGrant {
    fn drop(&mut self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let mut state = hub
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Ok(entry) = state.open_entry_mut(&self.id) {
            if entry.holder == Some(self.token) {
                entry.holder = None;
                let (cols, rows) = entry.default_size;
                entry.resize(cols, rows, true);
            }
        }
    }
}

/// One process's terminals. Synchronous producers may be OS reader threads;
/// subscriptions expose an async `next()` without spawning any hub tasks.
type LifecycleSink = Arc<dyn Fn(TerminalHubEvent) + Send + Sync>;

/// Layer 2's decision for raw input that reaches a terminal outside the
/// typed terminal operations (the internal binary attach transport). It is
/// installed by the automation owner; Layer 1 only asks it. Terminals it
/// does not own are always admitted.
pub trait RawInputGate: Send + Sync {
    /// The input holder of `terminal` changed (taken or released).
    fn control_changed(&self, terminal: &TerminalId);
    /// Whether raw bytes from the input holder may be written now.
    fn admit_raw_input(&self, terminal: &TerminalId) -> bool;
}

pub struct TerminalHub {
    lifecycle_sink: Mutex<Option<LifecycleSink>>,
    raw_input_gate: Mutex<Option<Arc<dyn RawInputGate>>>,
    config: TerminalHubConfig,
    state: Arc<Mutex<HubState>>,
    events: broadcast::Sender<TerminalHubEvent>,
}

impl std::fmt::Debug for TerminalHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalHub")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TerminalHub {
    pub fn new(config: TerminalHubConfig) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            config,
            state: Arc::new(Mutex::new(HubState::default())),
            events,
            lifecycle_sink: Mutex::new(None),
            raw_input_gate: Mutex::new(None),
        }
    }

    /// Install the automation owner's raw-input gate (once, at bootstrap).
    pub fn set_raw_input_gate(&self, gate: Arc<dyn RawInputGate>) {
        *self
            .raw_input_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(gate);
    }

    fn raw_input_gate(&self) -> Option<Arc<dyn RawInputGate>> {
        self.raw_input_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Take control on behalf of a transport without typed operations and
    /// report the change to the installed gate (readiness is invalidated).
    pub fn take_control_gated(
        self: &Arc<Self>,
        id: &TerminalId,
    ) -> Result<InputGrant, TerminalHubError> {
        let grant = self.take_control(id)?;
        if let Some(gate) = self.raw_input_gate() {
            gate.control_changed(id);
        }
        Ok(grant)
    }

    /// A transport without typed operations released control (the grant
    /// is already dropped; never call this under a hub or cell lock).
    pub fn report_control_released(&self, id: &TerminalId) {
        if let Some(gate) = self.raw_input_gate() {
            gate.control_changed(id);
        }
    }

    /// Whether the installed gate admits raw input to `id` now (always,
    /// without a gate).
    pub fn admit_raw_input(&self, id: &TerminalId) -> bool {
        self.raw_input_gate()
            .is_none_or(|gate| gate.admit_raw_input(id))
    }

    pub fn config(&self) -> &TerminalHubConfig {
        &self.config
    }

    pub fn register(
        self: &Arc<Self>,
        registration: TerminalRegistration,
        input: TerminalInput,
    ) -> TerminalHandle {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = TerminalId::new();
        let (cols, rows) = mirror::normalize_size(registration.cols, registration.rows);
        if (cols, rows) != (registration.cols, registration.rows) {
            if let Some(resize) = &input.resize {
                resize(cols, rows);
            }
        }
        let record = TerminalRecord {
            credential_updates: None,
            automation: registration.automation,
            id: id.clone(),
            owner_id: registration.owner.owner,
            feature: registration.owner.feature,
            source: registration.owner.source,
            agent: registration.agent,
            model: registration.model,
            attempt: registration.owner.attempt,
            cols,
            rows,
            state: TerminalState::Open,
            exit_code: None,
            created_at: Utc::now(),
            allow_remote_input: registration.owner.allow_remote_input,
        };
        let (output, _) = broadcast::channel(self.config.subscriber_capacity.max(1));
        state
            .by_owner
            .entry(record.owner_id.clone())
            .or_default()
            .push(id.clone());
        state.terminals.insert(
            id.clone(),
            TerminalEntry {
                mirror: ScreenMirror::new(
                    cols,
                    rows,
                    self.config.scrollback_lines,
                    self.config.ring_bytes,
                ),
                record: record.clone(),
                output: Some(output),
                input: Some(input),
                default_size: (cols, rows),
                holder: None,
                generation: 0,
                deferred: record.automation.is_some(),
                close_result: None,
            },
        );
        self.publish_event(TerminalHubEvent::Opened(Box::new(record)));
        TerminalHandle {
            generation: 0,
            id,
            hub: Arc::downgrade(self),
        }
    }

    pub fn get(&self, id: &TerminalId) -> Result<TerminalRecord, TerminalHubError> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)?
            .record
            .clone())
    }

    /// Open and finished records, newest first, optionally one owner's.
    pub fn list(&self, owner: Option<&TerminalOwner>) -> Vec<TerminalRecord> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut records: Vec<_> = state
            .terminals
            .values()
            .filter(|entry| owner.is_none_or(|owner| *owner == entry.record.owner_id))
            .map(|entry| entry.record.clone())
            .collect();
        records.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        records
    }

    pub fn snapshot(&self, id: &TerminalId) -> Result<TerminalSnapshot, TerminalHubError> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)?
            .mirror
            .snapshot())
    }

    /// Snapshot and receiver are captured under the same lock, preventing a
    /// gap or duplicated output between the initial screen and live frames.
    pub fn subscribe(&self, id: &TerminalId) -> Result<TerminalSubscription, TerminalHubError> {
        self.subscribe_with(id, SubscribeOptions::default())
    }

    /// [`Self::subscribe`] with options. With `history`, the retained output
    /// is queued before the snapshot under the same lock, so history,
    /// snapshot and live output are contiguous and never overlap.
    pub fn subscribe_with(
        &self,
        id: &TerminalId,
        options: SubscribeOptions,
    ) -> Result<TerminalSubscription, TerminalHubError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = state.entry(id)?;
        Ok(TerminalSubscription::new(
            id.clone(),
            entry,
            Arc::downgrade(&self.state),
            options,
        ))
    }

    /// Privileged bridge/policy operation. Viewer sessions should retain an
    /// `InputGrant` and use its methods, after Layer 2 admission.
    pub fn send_input(&self, id: &TerminalId, bytes: Vec<u8>) -> Result<(), TerminalHubError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = state.open_entry_mut(id)?;
        queue_input(
            &entry.input.as_ref().ok_or(TerminalHubError::Closed)?.stdin,
            bytes,
        )
    }

    /// Privileged resize operation. Read-only viewers must never call it.
    pub fn resize(&self, id: &TerminalId, cols: u16, rows: u16) -> Result<(), TerminalHubError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open_entry_mut(id)?.resize(cols, rows, true);
        Ok(())
    }

    pub fn take_control(self: &Arc<Self>, id: &TerminalId) -> Result<InputGrant, TerminalHubError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.open_entry_mut(id)?.input.is_none() {
            return Err(TerminalHubError::Closed);
        }
        if state.open_entry_mut(id)?.holder.is_some() {
            return Err(TerminalHubError::ControlHeld);
        }
        state.next_grant = state
            .next_grant
            .checked_add(1)
            .filter(|n| *n <= crate::data::automation_records::MAX_SAFE_COUNTER)
            .ok_or(TerminalHubError::Closed)?;
        let token = state.next_grant;
        state.open_entry_mut(id)?.holder = Some(token);
        Ok(InputGrant {
            id: id.clone(),
            token,
            hub: Arc::downgrade(self),
        })
    }

    /// The fencing epoch of the grant that holds input now, if any (a
    /// read-only check; it creates no capability).
    pub fn input_holder_epoch(&self, id: &TerminalId) -> Option<u64> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .entry(id)
            .ok()
            .filter(|e| e.input.is_some())
            .and_then(|e| e.holder)
    }

    pub fn has_input_holder(&self, id: &TerminalId) -> Result<bool, TerminalHubError> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)?
            .holder
            .is_some())
    }

    /// Explicit primary-frontend fallback update (0142). Settings reloads
    /// should not call this: unattended runs keep their registration size.
    /// While a grant is held, only the fallback changes, not the PTY size.
    pub fn set_default_size(
        &self,
        id: &TerminalId,
        cols: u16,
        rows: u16,
    ) -> Result<(), TerminalHubError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = state.open_entry_mut(id)?;
        entry.default_size = mirror::normalize_size(cols, rows);
        if entry.holder.is_none() {
            entry.resize(cols, rows, true);
        }
        Ok(())
    }

    /// Opt into controller-owned completion before starting the PTY.
    pub fn defer_finalization(&self, id: &TerminalId) -> Result<(), TerminalHubError> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .open_entry_mut(id)?
            .deferred = true;
        Ok(())
    }
    /// Seal the current binding; output may still drain until finalization.
    pub fn seal_input(&self, id: &TerminalId) -> Result<(), TerminalHubError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = state.open_entry_mut(id)?;
        entry.input = None;
        entry.holder = None;
        Ok(())
    }
    pub fn update_automation(
        &self,
        id: &TerminalId,
        snapshot: crate::data::automation_records::AutomationSnapshot,
    ) -> Result<(), TerminalHubError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = state.open_entry_mut(id)?;
        if snapshot.revision > crate::data::automation_records::MAX_SAFE_COUNTER
            || snapshot
                .status_reason
                .as_ref()
                .is_some_and(|s| s.len() > 2048)
            || serde_json::to_vec(&snapshot.capabilities).map_or(true, |s| s.len() > 65536)
        {
            return Err(TerminalHubError::Closed);
        }
        // Only a controller-owned terminal (registered with automation or
        // deferred) takes snapshots; never an older revision.
        match &entry.record.automation {
            Some(current)
                if current.execution_id != snapshot.execution_id
                    || current.revision > snapshot.revision
                    || (current.revision == snapshot.revision && *current != snapshot) =>
            {
                return Err(TerminalHubError::Closed);
            }
            None if !entry.deferred => return Err(TerminalHubError::Closed),
            _ => {}
        }
        entry.deferred = true;
        entry.record.automation = Some(snapshot.clone());
        if let Some(output) = &entry.output {
            let _ = output.send(HubMessage::Frame(Arc::new(
                TerminalFrame::AutomationState {
                    snapshot: Box::new(snapshot),
                },
            )));
        }
        Ok(())
    }
    /// Caller commits durable records and lifecycle events before publication.
    pub fn publish_automation_frame(
        &self,
        id: &TerminalId,
        frame: TerminalFrame,
    ) -> Result<(), TerminalHubError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = state.open_entry_mut(id)?;
        if serde_json::to_vec(&frame).map_or(true, |bytes| bytes.len() > 16 * 1024 * 1024) {
            return Err(TerminalHubError::Closed);
        }
        if matches!(&frame, TerminalFrame::ExecutionClosed { .. }) {
            if let Some(cached) = &entry.close_result {
                return if **cached == frame {
                    Ok(())
                } else {
                    Err(TerminalHubError::Closed)
                };
            }
        }
        match &frame {
            TerminalFrame::TurnAccepted { execution_id, .. }
            | TerminalFrame::TurnResult { execution_id, .. }
            | TerminalFrame::ExecutionClosed { execution_id, .. }
                if entry
                    .record
                    .automation
                    .as_ref()
                    .is_some_and(|s| &s.execution_id == execution_id) => {}
            TerminalFrame::OperationError { .. } => {}
            _ => return Err(TerminalHubError::Closed),
        }
        if matches!(frame, TerminalFrame::ExecutionClosed { .. }) {
            entry.close_result = Some(Arc::new(frame.clone()));
        }
        if let Some(output) = &entry.output {
            let _ = output.send(HubMessage::Frame(Arc::new(frame)));
        }
        Ok(())
    }
    /// Controller holds its admission lock and drains the sealed old binding first.
    pub fn rebind_execution(
        self: &Arc<Self>,
        id: &TerminalId,
        input: TerminalInput,
        snapshot: crate::data::automation_records::AutomationSnapshot,
        request_id: uuid::Uuid,
        credential_generation: u64,
    ) -> Result<TerminalHandle, TerminalHubError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if snapshot.revision > crate::data::automation_records::MAX_SAFE_COUNTER
            || snapshot
                .status_reason
                .as_ref()
                .is_some_and(|s| s.len() > 2048)
            || serde_json::to_vec(&snapshot.capabilities).map_or(true, |bytes| bytes.len() > 65536)
        {
            return Err(TerminalHubError::Closed);
        }
        let entry = state.open_entry_mut(id)?;
        if entry.input.is_some()
            || credential_generation > crate::data::automation_records::MAX_SAFE_COUNTER
        {
            return Err(TerminalHubError::Closed);
        }
        let previous_execution_id = entry
            .record
            .automation
            .as_ref()
            .ok_or(TerminalHubError::Closed)?
            .execution_id
            .clone();
        if previous_execution_id == snapshot.execution_id {
            return Err(TerminalHubError::Closed);
        }
        entry.generation = entry
            .generation
            .checked_add(1)
            .ok_or(TerminalHubError::Closed)?;
        entry.holder = None;
        entry.input = Some(input);
        entry.close_result = None;
        entry.deferred = true;
        // The departed holder's geometry must not size the new PTY/mirror.
        let (cols, rows) = entry.default_size;
        entry.resize(cols, rows, true);
        entry.mirror = ScreenMirror::new(
            entry.record.cols,
            entry.record.rows,
            self.config.scrollback_lines,
            self.config.ring_bytes,
        );
        if let Some(source) = &mut entry.record.source {
            source.execution_id = Some(snapshot.execution_id.clone());
            source.turn_id = None;
        }
        entry.record.credential_updates = None;
        entry.record.automation = Some(snapshot.clone());
        if let Some(output) = &entry.output {
            let _ = output.send(HubMessage::Frame(Arc::new(
                TerminalFrame::ExecutionReplaced {
                    request_id,
                    previous_execution_id,
                    execution_id: snapshot.execution_id.clone(),
                    credential_generation,
                },
            )));
            let screen = entry.mirror.snapshot();
            let _ = output.send(HubMessage::Frame(Arc::new(TerminalFrame::Snapshot {
                has_output: screen.has_output,
                cols: screen.cols,
                rows: screen.rows,
                screen: screen.screen,
            })));
            let _ = output.send(HubMessage::Frame(Arc::new(
                TerminalFrame::AutomationState {
                    snapshot: Box::new(snapshot),
                },
            )));
        }
        Ok(TerminalHandle {
            id: id.clone(),
            hub: Arc::downgrade(self),
            generation: entry.generation,
        })
    }
    /// Finish after state/result publication and bounded drain. Exactly one exit.
    pub fn finish_automation(&self, id: &TerminalId, code: i32) -> Result<(), TerminalHubError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.open_entry_mut(id)?;
        self.close_locked(&mut state, id, code);
        self.trim_finished(&mut state);
        Ok(())
    }
    /// Forget a finished terminal. Recent forgotten ids answer `Gone`;
    /// older tombstones expire to `NotFound` to bound registry memory.
    pub fn forget(&self, id: &TerminalId) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.forget_locked(&mut state, id)
    }

    /// Close and forget all an owner's terminals, including open ones.
    pub fn forget_owner(&self, owner: &TerminalOwner, exit_code: i32) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ids = state.by_owner.get(owner).cloned().unwrap_or_default();
        for id in &ids {
            self.close_locked(&mut state, id, exit_code);
            self.forget_locked(&mut state, id);
        }
        ids.len()
    }

    /// Bounded lifecycle channel (64 events). A lagging consumer must
    /// reconcile from `list`; subscribe before registering PTYs.
    pub fn events(&self) -> broadcast::Receiver<TerminalHubEvent> {
        self.events.subscribe()
    }

    /// Install the owner's synchronous audit sink before starting PTYs. The
    /// callback must not reenter the hub. Unlike viewers it cannot lose events
    /// to broadcast lag, even if a record is immediately evicted.
    pub fn set_lifecycle_sink(&self, sink: Arc<dyn Fn(TerminalHubEvent) + Send + Sync>) {
        *self
            .lifecycle_sink
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }
    fn publish_event(&self, event: TerminalHubEvent) {
        if let Some(sink) = self
            .lifecycle_sink
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            sink(event.clone());
        }
        let _ = self.events.send(event);
    }

    fn close_locked(&self, state: &mut HubState, id: &TerminalId, code: i32) {
        if state.close(id, code) {
            self.publish_event(TerminalHubEvent::Closed {
                id: id.clone(),
                exit_code: code,
                source: state
                    .entry(id)
                    .ok()
                    .and_then(|entry| entry.record.source.clone()),
            });
        }
    }

    fn forget_locked(&self, state: &mut HubState, id: &TerminalId) -> bool {
        if !state.forget(id, self.config.max_finished) {
            return false;
        }
        self.publish_event(TerminalHubEvent::Forgotten(id.clone()));
        true
    }

    fn trim_finished(&self, state: &mut HubState) {
        while state.finished.len() > self.config.max_finished {
            let id = state
                .finished
                .front()
                .expect("nonempty finished queue")
                .clone();
            self.forget_locked(state, &id);
        }
    }
}

impl TerminalHub {
    /// Caller persists the generation before publishing. All viewers receive
    /// certainty, while owner-private slots stay on the protected record API.
    pub fn set_credential_updates(
        &self,
        id: &TerminalId,
        snapshot: crate::data::credential_update::EffectiveCredentialUpdates,
    ) -> Result<(), TerminalHubError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = state.open_entry_mut(id)?;
        if entry
            .record
            .automation
            .as_ref()
            .is_some_and(|a| a.execution_id != snapshot.execution_id)
        {
            return Err(TerminalHubError::Closed);
        }
        let frame = TerminalFrame::CredentialUpdateState {
            snapshot: Box::new(snapshot.public_state()),
        };
        entry.record.credential_updates = Some(snapshot);
        if let Some(output) = &entry.output {
            let _ = output.send(HubMessage::Frame(Arc::new(frame)));
        }
        Ok(())
    }
}
