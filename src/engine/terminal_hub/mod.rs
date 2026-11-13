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

pub use mirror::{RingBuffer, ScreenMirror};
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
    Opened(TerminalRecord),
    Closed { id: TerminalId, exit_code: i32 },
    Forgotten(TerminalId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRegistration {
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
        entry.mirror.feed(bytes);
        if let Some(output) = &entry.output {
            if output.receiver_count() > 0 {
                for chunk in bytes.chunks(MAX_OUTPUT_FRAME_BYTES) {
                    let _ = output.send(HubMessage::Frame(TerminalFrame::Output {
                        bytes: chunk.to_vec(),
                    }));
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
                entry.resize(cols, rows, false);
            }
        }
    }

    pub fn exit(&self, code: i32) {
        if let Some(hub) = self.hub.upgrade() {
            let mut state = hub
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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

pub struct TerminalHub {
    lifecycle_sink: Mutex<Option<LifecycleSink>>,
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
        }
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
            },
        );
        self.publish_event(TerminalHubEvent::Opened(record));
        TerminalHandle {
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
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = state.entry(id)?;
        Ok(TerminalSubscription::new(
            id.clone(),
            entry,
            Arc::downgrade(&self.state),
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
        if state.open_entry_mut(id)?.holder.is_some() {
            return Err(TerminalHubError::ControlHeld);
        }
        state.next_grant = state
            .next_grant
            .checked_add(1)
            .expect("input grant counter exhausted");
        let token = state.next_grant;
        state.open_entry_mut(id)?.holder = Some(token);
        Ok(InputGrant {
            id: id.clone(),
            token,
            hub: Arc::downgrade(self),
        })
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
