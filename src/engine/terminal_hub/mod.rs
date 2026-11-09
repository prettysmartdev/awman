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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::daemon_state::FeatureName;
    use crate::data::execution_event::{EventSource, EventSourcePhase};
    use crate::data::ids::CommandId;
    use crate::data::terminal::{DetachReason, TerminalClientFrame};

    fn registration() -> TerminalRegistration {
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(CommandId::new()),
                feature: FeatureName::Worker,
                source: Some(EventSource::step(EventSourcePhase::Main, "build", 2)),
                attempt: 3,
                allow_remote_input: false,
            },
            agent: "test-agent".into(),
            model: Some("test-model".into()),
            cols: 20,
            rows: 6,
        }
    }

    fn register(hub: &Arc<TerminalHub>) -> (TerminalHandle, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let (stdin, rx) = crate::engine::terminal_hub::input_channel();
        (
            hub.register(
                registration(),
                TerminalInput {
                    stdin,
                    resize: None,
                },
            ),
            rx,
        )
    }

    fn hub() -> Arc<TerminalHub> {
        Arc::new(TerminalHub::new(TerminalHubConfig {
            ring_bytes: 32,
            subscriber_capacity: 2,
            max_finished: 2,
            scrollback_lines: 0,
        }))
    }

    fn as_frame(snapshot: TerminalSnapshot) -> TerminalFrame {
        TerminalFrame::Snapshot {
            has_output: snapshot.has_output,
            cols: snapshot.cols,
            rows: snapshot.rows,
            screen: snapshot.screen,
        }
    }

    async fn next(subscription: &mut TerminalSubscription) -> Option<TerminalFrame> {
        tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
            .await
            .expect("subscription stalled")
    }

    #[test]
    fn records_preserve_registration_and_finished_records_remain_listable() {
        let hub = hub();
        let mut events = hub.events();
        let registration = registration();
        let owner = registration.owner.owner.clone();
        let (stdin, _) = crate::engine::terminal_hub::input_channel();
        let handle = hub.register(
            registration.clone(),
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        let record = hub.get(handle.id()).unwrap();
        assert!(record.id.as_str().starts_with("term_"));
        assert_eq!(record.owner_id, owner);
        assert_eq!(record.source, registration.owner.source);
        assert_eq!(record.feature, FeatureName::Worker);
        assert_eq!(record.attempt, 3);
        assert_eq!(record.agent, "test-agent");
        assert_eq!(record.model.as_deref(), Some("test-model"));
        assert_eq!(record.state, TerminalState::Open);
        assert!(record.created_at <= Utc::now());
        assert_eq!(events.try_recv().unwrap(), TerminalHubEvent::Opened(record));
        let (other, _rx) = register(&hub);
        assert_eq!(hub.list(None)[0].id, *other.id());
        assert_eq!(hub.list(Some(&owner)).len(), 1);
        handle.exit(7);
        handle.exit(0); // Idempotent; cannot overwrite the actual code.
        handle.push_output(b"ignored after exit");
        assert_eq!(hub.get(handle.id()).unwrap().exit_code, Some(7));
        assert_eq!(hub.list(Some(&owner))[0].state, TerminalState::Exited);
        assert!(!hub.forget(other.id()));
        assert!(hub.forget(handle.id()));
        assert!(!hub.forget(handle.id()));
        assert_eq!(hub.get(handle.id()), Err(TerminalHubError::Gone));
        assert_eq!(hub.get(&TerminalId::new()), Err(TerminalHubError::NotFound));
    }

    #[tokio::test]
    async fn late_joiner_gets_current_screen_then_only_subsequent_output() {
        let hub = hub();
        let (handle, _input) = register(&hub);
        handle.push_output(b"before\r\n\x1b[31mred\x1b[0m");
        let expected = hub.snapshot(handle.id()).unwrap();
        let mut viewer = hub.subscribe(handle.id()).unwrap();
        handle.push_output(b"after");
        assert_eq!(next(&mut viewer).await, Some(as_frame(expected)));
        assert_eq!(
            next(&mut viewer).await,
            Some(TerminalFrame::Output {
                bytes: b"after".to_vec()
            })
        );
        handle.exit(17);
        assert_eq!(
            next(&mut viewer).await,
            Some(TerminalFrame::Exit { code: 17 })
        );
        assert_eq!(next(&mut viewer).await, None);
    }

    #[tokio::test]
    async fn slow_consumer_gets_lagged_and_fresh_snapshot_without_duplicate_output() {
        let hub = hub();
        let (handle, _input) = register(&hub);
        let mut slow = hub.subscribe(handle.id()).unwrap();
        let mut fast = hub.subscribe(handle.id()).unwrap();
        next(&mut slow).await;
        next(&mut fast).await;
        for i in 0..20 {
            let bytes = format!("line {i}\r\n").into_bytes();
            handle.push_output(&bytes);
            assert_eq!(next(&mut fast).await, Some(TerminalFrame::Output { bytes }));
        }
        assert_eq!(next(&mut slow).await, Some(TerminalFrame::Lagged));
        let expected = as_frame(hub.snapshot(handle.id()).unwrap());
        // Snapshot is the screen at resubscription, and subsequent bytes
        // appear once after it even when produced before it is consumed.
        handle.push_output(b"new");
        assert_eq!(next(&mut slow).await, Some(expected));
        assert_eq!(
            next(&mut slow).await,
            Some(TerminalFrame::Output {
                bytes: b"new".to_vec()
            })
        );
        let state = hub.state.lock().unwrap();
        let entry = state.entry(handle.id()).unwrap();
        assert!(entry.mirror.ring().len() <= 32);
        assert!(entry.output.as_ref().unwrap().len() <= 2);
    }

    #[tokio::test]
    async fn exit_fans_out_and_late_subscribers_also_close() {
        let hub = hub();
        let (handle, mut input) = register(&hub);
        let mut a = hub.subscribe(handle.id()).unwrap();
        let mut b = hub.subscribe(handle.id()).unwrap();
        next(&mut a).await;
        next(&mut b).await;
        handle.exit(-9);
        for viewer in [&mut a, &mut b] {
            assert_eq!(next(viewer).await, Some(TerminalFrame::Exit { code: -9 }));
            assert_eq!(next(viewer).await, None);
        }
        assert_eq!(input.recv().await, None);
        assert_eq!(
            hub.send_input(handle.id(), vec![1]),
            Err(TerminalHubError::Closed)
        );
        assert_eq!(
            hub.resize(handle.id(), 30, 9),
            Err(TerminalHubError::Closed)
        );
        let mut late = hub.subscribe(handle.id()).unwrap();
        assert!(matches!(
            next(&mut late).await,
            Some(TerminalFrame::Snapshot { .. })
        ));
        assert_eq!(
            next(&mut late).await,
            Some(TerminalFrame::Exit { code: -9 })
        );
        assert_eq!(next(&mut late).await, None);
    }

    #[tokio::test]
    async fn lag_at_exit_recovers_even_after_immediate_eviction() {
        for max_finished in [0, 2] {
            let hub = Arc::new(TerminalHub::new(TerminalHubConfig {
                subscriber_capacity: 1,
                max_finished,
                ..Default::default()
            }));
            let (handle, _input) = register(&hub);
            let mut viewer = hub.subscribe(handle.id()).unwrap();
            next(&mut viewer).await;
            for _ in 0..10 {
                handle.push_output(b"abc\r\n");
            }
            let expected = as_frame(hub.snapshot(handle.id()).unwrap());
            handle.exit(23);
            assert_eq!(next(&mut viewer).await, Some(TerminalFrame::Lagged));
            assert_eq!(next(&mut viewer).await, Some(expected));
            assert_eq!(
                next(&mut viewer).await,
                Some(TerminalFrame::Exit { code: 23 })
            );
            assert_eq!(next(&mut viewer).await, None);
        }
    }

    #[test]
    fn finished_registry_owner_index_and_tombstones_are_bounded() {
        let hub = hub();
        let mut events = hub.events();
        let mut latest = Vec::new();
        for _ in 0..100 {
            let (handle, _input) = register(&hub);
            handle.exit(0);
            latest.push(handle.id().clone());
            let state = hub.state.lock().unwrap();
            assert!(state.terminals.len() <= 2);
            assert!(state.finished.len() <= 2);
            assert!(state.by_owner.len() <= 2);
            assert!(state.gone.len() <= 2);
        }
        assert_eq!(hub.get(&latest[97]), Err(TerminalHubError::Gone));
        assert_eq!(hub.list(None).len(), 2);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let owner = hub.get(&latest[99]).unwrap().owner_id;
        assert_eq!(hub.forget_owner(&owner, -1), 1);
        assert!(hub.list(Some(&owner)).is_empty());
    }

    #[tokio::test]
    async fn forget_owner_exits_open_terminals_without_affecting_other_owners() {
        let hub = hub();
        let (a, _input_a) = register(&hub);
        let (b, _input_b) = register(&hub);
        let mut viewer = hub.subscribe(a.id()).unwrap();
        next(&mut viewer).await;
        assert_eq!(hub.forget_owner(&hub.get(a.id()).unwrap().owner_id, 19), 1);
        assert_eq!(
            next(&mut viewer).await,
            Some(TerminalFrame::Exit { code: 19 })
        );
        assert_eq!(next(&mut viewer).await, None);
        assert_eq!(hub.get(a.id()), Err(TerminalHubError::Gone));
        assert_eq!(hub.get(b.id()).unwrap().state, TerminalState::Open);
    }

    #[test]
    fn grants_are_exclusive_and_release_restores_configured_or_primary_size() {
        let hub = hub();
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let sink = sizes.clone();
        let (stdin, mut input) = crate::engine::terminal_hub::input_channel();
        let handle = hub.register(
            registration(),
            TerminalInput {
                stdin,
                resize: Some(Arc::new(move |cols, rows| {
                    sink.lock().unwrap().push((cols, rows))
                })),
            },
        );
        let grant = hub.take_control(handle.id()).unwrap();
        assert_eq!(
            hub.take_control(handle.id()).unwrap_err(),
            TerminalHubError::ControlHeld
        );
        assert!(hub.has_input_holder(handle.id()).unwrap());
        grant.send_input(vec![0, 255]).unwrap();
        assert_eq!(input.try_recv().unwrap(), vec![0, 255]);
        grant.resize(80, 24).unwrap();
        assert_eq!(
            (
                hub.get(handle.id()).unwrap().cols,
                hub.get(handle.id()).unwrap().rows
            ),
            (80, 24)
        );
        hub.set_default_size(handle.id(), 30, 10).unwrap();
        assert_eq!(hub.get(handle.id()).unwrap().cols, 80);
        grant.release();
        assert!(!hub.has_input_holder(handle.id()).unwrap());
        assert_eq!(hub.snapshot(handle.id()).unwrap().cols, 30);
        assert_eq!(*sizes.lock().unwrap(), vec![(80, 24), (30, 10)]);
        let next_grant = hub.take_control(handle.id()).unwrap();
        next_grant.resize(40, 12).unwrap();
        drop(next_grant);
        assert_eq!(hub.snapshot(handle.id()).unwrap().cols, 30);
        let grant = hub.take_control(handle.id()).unwrap();
        handle.exit(0);
        assert_eq!(grant.resize(10, 5), Err(TerminalHubError::Closed));
        assert!(!hub.has_input_holder(handle.id()).unwrap());
    }

    #[tokio::test]
    async fn read_only_subscription_does_not_take_control_or_resize() {
        let hub = hub();
        let (handle, _input) = register(&hub);
        let mut viewer = hub.subscribe(handle.id()).unwrap();
        next(&mut viewer).await;
        assert!(!hub.has_input_holder(handle.id()).unwrap());
        assert_eq!(hub.snapshot(handle.id()).unwrap().cols, 20);
        drop(viewer);
        assert_eq!(hub.snapshot(handle.id()).unwrap().cols, 20);
        handle.resize(15, 5);
        assert_eq!(hub.snapshot(handle.id()).unwrap().cols, 15);
    }

    #[test]
    fn concurrent_take_control_has_only_one_winner() {
        let hub = hub();
        let (handle, _input) = register(&hub);
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let hub = hub.clone();
                let id = handle.id().clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    hub.take_control(&id)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        drop(results);
        assert!(!hub.has_input_holder(handle.id()).unwrap());
    }

    #[tokio::test]
    async fn holder_resize_is_broadcast_and_release_restores_registration_size() {
        let hub = hub();
        let (handle, _input) = register(&hub);
        let mut viewer = hub.subscribe(handle.id()).unwrap();
        next(&mut viewer).await;
        let grant = hub.take_control(handle.id()).unwrap();
        grant.resize(80, 24).unwrap();
        assert_eq!(
            next(&mut viewer).await,
            Some(TerminalFrame::Resize { cols: 80, rows: 24 })
        );
        grant.release();
        assert_eq!(
            next(&mut viewer).await,
            Some(TerminalFrame::Resize { cols: 20, rows: 6 })
        );
        assert_eq!(hub.snapshot(handle.id()).unwrap().rows, 6);
    }

    /// Review finding: an untrusted `resize{65535,65535}` must be clamped
    /// before the mirror allocates or the PTY callback runs.
    #[tokio::test]
    async fn oversized_resize_requests_are_clamped_before_allocation() {
        let hub = hub();
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let sink = sizes.clone();
        let (stdin, _input) = input_channel();
        let mut big = registration();
        big.cols = u16::MAX;
        big.rows = u16::MAX;
        let handle = hub.register(
            big,
            TerminalInput {
                stdin,
                resize: Some(Arc::new(move |c, r| sink.lock().unwrap().push((c, r)))),
            },
        );
        let max = (MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS);
        let record = hub.get(handle.id()).unwrap();
        assert_eq!((record.cols, record.rows), max);
        let grant = hub.take_control(handle.id()).unwrap();
        grant.resize(80, 24).unwrap();
        grant.resize(u16::MAX, u16::MAX).unwrap();
        let snapshot = hub.snapshot(handle.id()).unwrap();
        assert_eq!((snapshot.cols, snapshot.rows), max);
        assert!(sizes
            .lock()
            .unwrap()
            .iter()
            .all(|&(c, r)| c <= max.0 && r <= max.1));
        // Bridge-reported (already applied) oversize is corrected too.
        drop(grant);
        handle.resize(u16::MAX, 3);
        assert_eq!(hub.get(handle.id()).unwrap().cols, MAX_TERMINAL_COLS);
    }

    /// Review finding: input is bounded; a full queue refuses the whole
    /// frame (never a partial write) until the agent drains it.
    #[test]
    fn input_queue_is_bounded_and_frames_are_all_or_nothing() {
        let hub = hub();
        let (handle, mut rx) = register(&hub);
        let grant = hub.take_control(handle.id()).unwrap();
        let big = vec![b'y'; MAX_INPUT_CHUNK_BYTES * (INPUT_QUEUE_CHUNKS - 1)];
        grant.send_input(big).unwrap();
        assert_eq!(
            grant.send_input(vec![b'z'; MAX_INPUT_CHUNK_BYTES + 1]),
            Err(TerminalHubError::InputBackpressure)
        );
        assert_eq!(
            hub.send_input(handle.id(), vec![b'z'; 2 * MAX_INPUT_CHUNK_BYTES]),
            Err(TerminalHubError::InputBackpressure)
        );
        // One chunk still fits; the refused frames left nothing behind.
        grant.send_input(b"q".to_vec()).unwrap();
        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            assert!(chunk.len() <= MAX_INPUT_CHUNK_BYTES);
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), INPUT_QUEUE_CHUNKS);
        assert_eq!(chunks.last().unwrap(), b"q");
        assert!(!chunks.concat().contains(&b'z'));
        grant
            .send_input(vec![b'z'; 2 * MAX_INPUT_CHUNK_BYTES])
            .unwrap();
    }

    #[tokio::test]
    async fn zero_buffer_settings_and_tiny_sizes_are_safe() {
        let hub = Arc::new(TerminalHub::new(TerminalHubConfig {
            ring_bytes: 0,
            subscriber_capacity: 0,
            max_finished: 0,
            scrollback_lines: 0,
        }));
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let sink = sizes.clone();
        let (stdin, _input) = crate::engine::terminal_hub::input_channel();
        let mut registration = registration();
        registration.cols = 1;
        registration.rows = 0;
        let handle = hub.register(
            registration,
            TerminalInput {
                stdin,
                resize: Some(Arc::new(move |cols, rows| {
                    sink.lock().unwrap().push((cols, rows))
                })),
            },
        );
        assert_eq!(*sizes.lock().unwrap(), vec![(2, 2)]);
        let mut viewer = hub.subscribe(handle.id()).unwrap();
        assert!(matches!(
            next(&mut viewer).await,
            Some(TerminalFrame::Snapshot {
                cols: 2,
                rows: 2,
                ..
            })
        ));
        handle.resize(1, 1);
        assert_eq!(*sizes.lock().unwrap(), vec![(2, 2), (2, 2)]);
        handle.push_output("界界界界".as_bytes());
        assert!(matches!(
            next(&mut viewer).await,
            Some(TerminalFrame::Output { .. })
        ));
        handle.exit(0);
        assert_eq!(hub.get(handle.id()), Err(TerminalHubError::Gone));
        assert_eq!(
            next(&mut viewer).await,
            Some(TerminalFrame::Exit { code: 0 })
        );
        assert_eq!(next(&mut viewer).await, None);
    }

    #[tokio::test]
    async fn giant_output_is_split_and_fanout_memory_is_bounded() {
        let hub = hub();
        let (handle, _input) = register(&hub);
        let mut viewer = hub.subscribe(handle.id()).unwrap();
        next(&mut viewer).await;
        let bytes = vec![b'x'; 2 * MAX_OUTPUT_FRAME_BYTES];
        handle.push_output(&bytes);
        for _ in 0..2 {
            assert!(
                matches!(next(&mut viewer).await, Some(TerminalFrame::Output { bytes }) if bytes.len() == MAX_OUTPUT_FRAME_BYTES)
            );
        }
        handle.push_output(&vec![b'y'; 10 * MAX_OUTPUT_FRAME_BYTES]);
        {
            let state = hub.state.lock().unwrap();
            let entry = state.entry(handle.id()).unwrap();
            assert_eq!(entry.mirror.ring().len(), 32);
            assert!(entry.output.as_ref().unwrap().len() <= 2);
        }
        assert_eq!(next(&mut viewer).await, Some(TerminalFrame::Lagged));
        assert!(matches!(
            next(&mut viewer).await,
            Some(TerminalFrame::Snapshot { .. })
        ));
    }

    #[tokio::test]
    async fn subscriptions_do_not_keep_hub_or_stdin_alive() {
        let hub = hub();
        let weak = Arc::downgrade(&hub);
        let (handle, mut input) = register(&hub);
        let mut viewer = hub.subscribe(handle.id()).unwrap();
        next(&mut viewer).await;
        drop(hub);
        assert!(weak.upgrade().is_none());
        assert_eq!(next(&mut viewer).await, None);
        assert_eq!(input.recv().await, None);
        handle.push_output(b"harmless");
        handle.exit(0);
    }

    // The Layer 0 file is scaffold-owned (annotations belong to attach-cli).
    // Test its shared JSON codec here without changing another step's file.
    #[test]
    fn wire_codec_round_trips_every_server_and_client_frame() {
        for frame in [
            TerminalFrame::Snapshot {
                cols: 80,
                rows: 24,
                has_output: true,
                screen: vec![0, 27, 255],
            },
            TerminalFrame::Output {
                bytes: (0..=255).collect(),
            },
            TerminalFrame::Output { bytes: vec![] },
            TerminalFrame::Resize {
                cols: 120,
                rows: 40,
            },
            TerminalFrame::Exit { code: -9 },
            TerminalFrame::Lagged,
        ] {
            let json = serde_json::to_string(&frame).unwrap();
            assert_eq!(serde_json::from_str::<TerminalFrame>(&json).unwrap(), frame);
        }
        for frame in [
            TerminalClientFrame::Input {
                bytes: vec![0, 255, 27],
            },
            TerminalClientFrame::Resize { cols: 80, rows: 24 },
            TerminalClientFrame::TakeControl,
            TerminalClientFrame::Detached {
                reason: DetachReason::User,
            },
            TerminalClientFrame::Detached {
                reason: DetachReason::Hangup,
            },
        ] {
            let json = serde_json::to_string(&frame).unwrap();
            assert_eq!(
                serde_json::from_str::<TerminalClientFrame>(&json).unwrap(),
                frame
            );
        }
        assert_eq!(
            serde_json::to_string(&TerminalFrame::Output {
                bytes: vec![0, 255]
            })
            .unwrap(),
            r#"{"type":"output","bytes":"AP8="}"#
        );
        for bad in [
            r#"{"type":"output","bytes":[1,2]}"#,
            r#"{"type":"output","bytes":"!"}"#,
            r#"{"type":"wat"}"#,
        ] {
            assert!(serde_json::from_str::<TerminalFrame>(bad).is_err());
        }
    }
}

#[cfg(test)]
mod poison_regression {
    use super::*;
    #[test]
    fn a_poisoned_mutex_does_not_panic_every_subsequent_reader() {
        let hub = Arc::new(TerminalHub::new(Default::default()));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hub.state.lock().unwrap();
            panic!("simulate an unrelated callback panic");
        }));
        let (stdin, _input) = input_channel();
        let terminal = hub.register(
            TerminalRegistration {
                owner: TerminalOwnerInfo {
                    owner: TerminalOwner::Command(crate::data::ids::CommandId::new()),
                    feature: crate::data::daemon_state::FeatureName::Worker,
                    source: None,
                    attempt: 1,
                    allow_remote_input: false,
                },
                agent: "test".into(),
                model: None,
                cols: 80,
                rows: 24,
            },
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        terminal.push_output(b"still draining");
        assert!(
            String::from_utf8_lossy(&hub.snapshot(terminal.id()).unwrap().screen)
                .contains("still draining")
        );
        terminal.exit(0);
        assert_eq!(hub.get(terminal.id()).unwrap().exit_code, Some(0));
    }
}
