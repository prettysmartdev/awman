//! Finite ACP queue budgets and the bounded queues that enforce them.
//!
//! Every queue between an ACP agent's pipes and the session driver is bounded
//! by items and by bytes. An item costs the length of the wire line it came
//! from. What happens when a queue is full depends on what it carries:
//!
//! | Queue | When full |
//! |---|---|
//! | transport stdout | the bridge stops reading (backpressure); an unbounded runtime stream fails with `AcpOverflow` |
//! | transport stdin | the sender waits, cancellably |
//! | transport stderr | oldest dropped, with a counted notice |
//! | bulk updates | replaceable state is coalesced, then the reader waits |
//! | control | `AcpOverflow`: the session is cancelled |
//! | pending requests | the new request fails with `AcpOverflow` |
//!
//! Message and output chunks, permission requests and results are never
//! dropped.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::Notify;

use crate::engine::acp::protocol::{SessionUpdate, ToolCallUpdate, MAX_ACP_LINE_BYTES};
use crate::engine::agent_runtime::frontend::GateUsage;

/// The queue budgets of one ACP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpBudgets {
    pub stdout_items: usize,
    pub stdout_bytes: usize,
    pub stdin_items: usize,
    pub stdin_bytes: usize,
    pub stderr_items: usize,
    pub stderr_bytes: usize,
    pub bulk_items: usize,
    pub bulk_bytes: usize,
    pub control_items: usize,
    pub control_bytes: usize,
    /// Session updates decoded ahead of a full bulk queue (so later control
    /// frames are still read), together with the framer's partial line.
    pub decode_ahead_items: usize,
    pub decode_ahead_bytes: usize,
    pub pending_requests: usize,
}

impl Default for AcpBudgets {
    /// The production budgets. Each byte budget that carries protocol lines
    /// equals [`MAX_ACP_LINE_BYTES`], so a maximum-size valid line always
    /// fits. Queued memory per session stays under 48 MiB: stdout 8, framer
    /// plus decode-ahead 8 (plus one transport chunk), bulk 8, control 8
    /// (permission requests and unread RPC responses), stdin 8, stderr 2
    /// (transport 1, ring 1).
    fn default() -> Self {
        Self {
            stdout_items: 1024,
            stdout_bytes: MAX_ACP_LINE_BYTES,
            stdin_items: 64,
            stdin_bytes: MAX_ACP_LINE_BYTES,
            stderr_items: 1024,
            stderr_bytes: 1024 * 1024,
            bulk_items: 1024,
            bulk_bytes: MAX_ACP_LINE_BYTES,
            control_items: 64,
            control_bytes: MAX_ACP_LINE_BYTES,
            decode_ahead_items: 1024,
            decode_ahead_bytes: MAX_ACP_LINE_BYTES,
            pending_requests: 64,
        }
    }
}

/// Occupancy and high-water marks of one bounded queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueUsage {
    pub items: usize,
    pub bytes: usize,
    pub high_items: usize,
    pub high_bytes: usize,
    pub max_items: usize,
    pub max_bytes: usize,
}

impl From<GateUsage> for QueueUsage {
    fn from(u: GateUsage) -> Self {
        Self {
            items: u.items,
            bytes: u.bytes,
            high_items: u.high_items,
            high_bytes: u.high_bytes,
            max_items: u.max_items,
            max_bytes: u.max_bytes,
        }
    }
}

/// Every ACP queue of a session, for diagnostics and stress evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AcpQueueStats {
    pub stdout: QueueUsage,
    pub stdin: QueueUsage,
    pub stderr: QueueUsage,
    /// Session updates decoded ahead of a full bulk queue, plus the
    /// framer's partial line (bytes).
    pub decode_ahead: QueueUsage,
    /// Stderr chunks between the runtime bridge and the stderr ring.
    pub stderr_transport: QueueUsage,
    pub bulk: QueueUsage,
    /// Permission requests plus RPC responses not yet taken by their caller.
    pub control: QueueUsage,
    pub pending_requests: usize,
    pub high_pending_requests: usize,
    /// Bytes of agent stderr dropped because its queue was full.
    pub stderr_dropped_bytes: u64,
    /// Bulk updates merged into a newer update of the same state.
    pub coalesced_updates: u64,
    /// True once the reader tasks have stopped and released their queues.
    pub reader_stopped: bool,
}

/// Why a decoded queue refused an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueError {
    Closed,
    Full,
}

/// State updates where only the latest value matters.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoalesceKey {
    Plan,
    Commands,
    Mode,
    ToolCall(String),
}

fn coalesce_key(update: &SessionUpdate) -> Option<CoalesceKey> {
    match update {
        SessionUpdate::Plan { .. } => Some(CoalesceKey::Plan),
        SessionUpdate::AvailableCommandsUpdate { .. } => Some(CoalesceKey::Commands),
        SessionUpdate::CurrentModeUpdate { .. } => Some(CoalesceKey::Mode),
        SessionUpdate::ToolCallUpdate { tool_call } => {
            Some(CoalesceKey::ToolCall(tool_call.tool_call_id.clone()))
        }
        // Message/thought chunks and new tool calls are content, never state.
        _ => None,
    }
}

/// Merge `newer` into `older` (same key). A tool-call update carries only the
/// fields that changed, so later fields win and earlier ones survive.
fn merge(older: SessionUpdate, newer: SessionUpdate) -> SessionUpdate {
    match (older, newer) {
        (
            SessionUpdate::ToolCallUpdate { tool_call: old },
            SessionUpdate::ToolCallUpdate { tool_call: new },
        ) => SessionUpdate::ToolCallUpdate {
            tool_call: ToolCallUpdate {
                tool_call_id: new.tool_call_id,
                kind: new.kind.or(old.kind),
                status: new.status.or(old.status),
                title: new.title.or(old.title),
                content: new.content.or(old.content),
                locations: new.locations.or(old.locations),
                raw_input: new.raw_input.or(old.raw_input),
                raw_output: new.raw_output.or(old.raw_output),
            },
        },
        (_, newer) => newer,
    }
}

fn encoded_len(update: &SessionUpdate) -> usize {
    serde_json::to_vec(update).map(|v| v.len()).unwrap_or(0)
}

struct Entry<T> {
    item: T,
    cost: usize,
}

struct QueueState<T> {
    entries: VecDeque<Entry<T>>,
    bytes: usize,
    /// Items charged to the queue without an entry (see
    /// [`DecodedQueue::try_reserve`]): RPC responses awaiting their caller.
    reserved_items: usize,
    reserved_bytes: usize,
    high_items: usize,
    high_bytes: usize,
    closed: bool,
    coalesced: u64,
}

impl<T> QueueState<T> {
    fn items(&self) -> usize {
        self.entries.len() + self.reserved_items
    }
    fn total_bytes(&self) -> usize {
        self.bytes + self.reserved_bytes
    }
}

/// Why [`DecodedQueue::try_push_update`] did not queue an update.
pub(crate) enum PushUpdate {
    /// The queue is closed; the update is discarded.
    Closed,
    /// The update can never fit (larger than the byte budget).
    TooLarge,
    /// No space now; retry with the returned update once the queue drains.
    Full(Box<SessionUpdate>, usize),
}

/// A FIFO bounded by items and bytes. Every item must fit the byte budget
/// on its own (a maximum valid ACP line does, see [`AcpBudgets`]); an item
/// larger than the budget is refused, never admitted as an exception.
pub(crate) struct DecodedQueue<T> {
    max_items: usize,
    max_bytes: usize,
    state: Mutex<QueueState<T>>,
    readable: Notify,
    writable: Notify,
}

impl<T> DecodedQueue<T> {
    pub(crate) fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(1),
            state: Mutex::new(QueueState {
                entries: VecDeque::new(),
                bytes: 0,
                reserved_items: 0,
                reserved_bytes: 0,
                high_items: 0,
                high_bytes: 0,
                closed: false,
                coalesced: 0,
            }),
            readable: Notify::new(),
            writable: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fits(&self, state: &QueueState<T>, cost: usize) -> bool {
        cost <= self.max_bytes
            && state.items() < self.max_items
            && state.total_bytes() + cost <= self.max_bytes
    }

    fn note_high(&self, state: &mut QueueState<T>) {
        state.high_items = state.high_items.max(state.items());
        state.high_bytes = state.high_bytes.max(state.total_bytes());
    }

    fn append(&self, state: &mut QueueState<T>, item: T, cost: usize) {
        state.entries.push_back(Entry { item, cost });
        state.bytes += cost;
        self.note_high(state);
        self.readable.notify_one();
    }

    /// Queue `item` now, or report why not. Never waits.
    pub(crate) fn try_push(&self, item: T, cost: usize) -> Result<(), QueueError> {
        let mut state = self.lock();
        if state.closed {
            return Err(QueueError::Closed);
        }
        if !self.fits(&state, cost) {
            return Err(QueueError::Full);
        }
        self.append(&mut state, item, cost);
        Ok(())
    }

    /// Charge one item of `cost` bytes held outside the queue (until
    /// [`release_reserved`](Self::release_reserved)). Never waits.
    pub(crate) fn try_reserve(&self, cost: usize) -> Result<(), QueueError> {
        let mut state = self.lock();
        if state.closed {
            return Err(QueueError::Closed);
        }
        if !self.fits(&state, cost) {
            return Err(QueueError::Full);
        }
        state.reserved_items += 1;
        state.reserved_bytes += cost;
        self.note_high(&mut state);
        Ok(())
    }

    pub(crate) fn release_reserved(&self, cost: usize) {
        {
            let mut state = self.lock();
            state.reserved_items = state.reserved_items.saturating_sub(1);
            state.reserved_bytes = state.reserved_bytes.saturating_sub(cost);
        }
        self.writable.notify_waiters();
    }

    pub(crate) fn try_pop(&self) -> Option<T> {
        let mut state = self.lock();
        let entry = state.entries.pop_front()?;
        state.bytes -= entry.cost;
        drop(state);
        self.writable.notify_waiters();
        Some(entry.item)
    }

    /// The next item; `None` once the queue is closed and drained.
    pub(crate) async fn pop(&self) -> Option<T> {
        loop {
            let notified = self.readable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(item) = self.try_pop() {
                return Some(item);
            }
            if self.lock().closed {
                return None;
            }
            notified.await;
        }
    }

    /// No more items will arrive; queued ones remain to be drained.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.readable.notify_waiters();
        self.writable.notify_waiters();
    }

    /// Close and free everything queued (cancellation).
    pub(crate) fn abort(&self) {
        {
            let mut state = self.lock();
            state.closed = true;
            state.entries = VecDeque::new();
            state.bytes = 0;
        }
        self.readable.notify_waiters();
        self.writable.notify_waiters();
    }

    pub(crate) fn usage(&self) -> QueueUsage {
        let state = self.lock();
        QueueUsage {
            items: state.items(),
            bytes: state.total_bytes(),
            high_items: state.high_items,
            high_bytes: state.high_bytes,
            max_items: self.max_items,
            max_bytes: self.max_bytes,
        }
    }

    pub(crate) fn coalesced(&self) -> u64 {
        self.lock().coalesced
    }
}

impl DecodedQueue<SessionUpdate> {
    /// Queue a session update now if it fits. When the queue is full and the
    /// update replaces state already queued, the older entry is merged into
    /// it (latest wins) — but only when the merged update itself fits in the
    /// space the older entry frees; otherwise both stay separate and the
    /// caller waits for the queue to drain. Content is never merged or
    /// dropped.
    pub(crate) fn try_push_update(
        &self,
        update: SessionUpdate,
        cost: usize,
    ) -> Result<(), PushUpdate> {
        if cost > self.max_bytes {
            return Err(PushUpdate::TooLarge);
        }
        let mut state = self.lock();
        if state.closed {
            return Err(PushUpdate::Closed);
        }
        if self.fits(&state, cost) {
            self.append(&mut state, update, cost);
            return Ok(());
        }
        if let Some(key) = coalesce_key(&update) {
            let same = state
                .entries
                .iter()
                .position(|e| coalesce_key(&e.item).as_ref() == Some(&key));
            if let Some(index) = same {
                let older = &state.entries[index];
                let merged = merge(older.item.clone(), update.clone());
                let merged_cost = encoded_len(&merged).min(older.cost + cost);
                let older_cost = older.cost;
                // Fits in place of the older entry (same item count).
                let fits = merged_cost <= self.max_bytes
                    && state.total_bytes() - older_cost + merged_cost <= self.max_bytes;
                if fits {
                    state.entries.remove(index);
                    state.bytes -= older_cost;
                    state.coalesced += 1;
                    state.entries.push_back(Entry {
                        item: merged,
                        cost: merged_cost,
                    });
                    state.bytes += merged_cost;
                    self.note_high(&mut state);
                    self.readable.notify_one();
                    return Ok(());
                }
            }
        }
        Err(PushUpdate::Full(Box::new(update), cost))
    }

    /// Resolves after the queue may have gained space. Create (and enable)
    /// it before [`try_push_update`](Self::try_push_update) to not miss a
    /// wakeup.
    pub(crate) fn writable(&self) -> tokio::sync::futures::Notified<'_> {
        self.writable.notified()
    }

    /// Queue a session update, waiting for space (see
    /// [`try_push_update`](Self::try_push_update)). Returns `Closed` when the
    /// queue closes or `cancel` fires, `Full` for an update that can never
    /// fit.
    #[cfg(test)]
    pub(crate) async fn push_update(
        &self,
        mut update: SessionUpdate,
        mut cost: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), QueueError> {
        loop {
            let notified = self.writable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_push_update(update, cost) {
                Ok(()) => return Ok(()),
                Err(PushUpdate::Closed) => return Err(QueueError::Closed),
                Err(PushUpdate::TooLarge) => return Err(QueueError::Full),
                Err(PushUpdate::Full(u, c)) => {
                    update = *u;
                    cost = c;
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(QueueError::Closed),
                _ = &mut notified => {}
            }
        }
    }
}

struct StderrState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    high_items: usize,
    high_bytes: usize,
    dropped: u64,
    unreported: u64,
    closed: bool,
}

/// Agent diagnostics: newest kept, oldest dropped and counted.
pub(crate) struct StderrRing {
    max_items: usize,
    max_bytes: usize,
    state: Mutex<StderrState>,
    readable: Notify,
}

impl StderrRing {
    pub(crate) fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(1),
            state: Mutex::new(StderrState {
                chunks: VecDeque::new(),
                bytes: 0,
                high_items: 0,
                high_bytes: 0,
                dropped: 0,
                unreported: 0,
                closed: false,
            }),
            readable: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StderrState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn push(&self, mut chunk: Vec<u8>) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        if chunk.len() > self.max_bytes {
            let excess = chunk.len() - self.max_bytes;
            chunk.drain(..excess);
            state.dropped += excess as u64;
            state.unreported += excess as u64;
        }
        state.bytes += chunk.len();
        state.chunks.push_back(chunk);
        while state.chunks.len() > self.max_items || state.bytes > self.max_bytes {
            let Some(old) = state.chunks.pop_front() else {
                break;
            };
            state.bytes -= old.len();
            state.dropped += old.len() as u64;
            state.unreported += old.len() as u64;
        }
        state.high_items = state.high_items.max(state.chunks.len());
        state.high_bytes = state.high_bytes.max(state.bytes);
        drop(state);
        self.readable.notify_one();
    }

    /// The next chunk and the bytes dropped since the previous call.
    pub(crate) async fn pop(&self) -> Option<(Option<Vec<u8>>, u64)> {
        loop {
            let notified = self.readable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                let dropped = std::mem::take(&mut state.unreported);
                if let Some(chunk) = state.chunks.pop_front() {
                    state.bytes -= chunk.len();
                    return Some((Some(chunk), dropped));
                }
                if dropped > 0 {
                    return Some((None, dropped));
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Count bytes dropped before they reached the ring (at ingress).
    pub(crate) fn note_dropped(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        {
            let mut state = self.lock();
            state.dropped += bytes;
            state.unreported += bytes;
        }
        self.readable.notify_one();
    }

    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.readable.notify_waiters();
    }

    pub(crate) fn abort(&self) {
        {
            let mut state = self.lock();
            state.closed = true;
            state.chunks = VecDeque::new();
            state.bytes = 0;
        }
        self.readable.notify_waiters();
    }

    pub(crate) fn usage(&self) -> (QueueUsage, u64) {
        let state = self.lock();
        (
            QueueUsage {
                items: state.chunks.len(),
                bytes: state.bytes,
                high_items: state.high_items,
                high_bytes: state.high_bytes,
                max_items: self.max_items,
                max_bytes: self.max_bytes,
            },
            state.dropped,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::acp::protocol::ContentChunk;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk {
            chunk: serde_json::from_value::<ContentChunk>(
                json!({"content": {"type": "text", "text": text}}),
            )
            .unwrap(),
        }
    }

    fn status(id: &str, status: &str) -> SessionUpdate {
        serde_json::from_value(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status}),
        )
        .unwrap()
    }

    #[test]
    fn an_item_larger_than_the_budget_is_refused_even_by_an_empty_queue() {
        let q = DecodedQueue::new(8, 10);
        assert_eq!(q.try_push(1, 50), Err(QueueError::Full));
        assert_eq!(q.try_reserve(11), Err(QueueError::Full));
        assert_eq!(q.usage().bytes, 0);
        q.try_push(1, 10).unwrap();
        assert_eq!(q.try_push(2, 1), Err(QueueError::Full));
        assert_eq!(q.try_pop(), Some(1));
        q.try_push(3, 6).unwrap();
        assert_eq!(q.try_push(4, 5), Err(QueueError::Full));
        q.try_push(5, 4).unwrap();
        assert_eq!(q.usage().bytes, 10);
        assert!(q.usage().high_bytes <= 10);
    }

    #[test]
    fn reservations_share_the_item_and_byte_budget() {
        let q: DecodedQueue<u8> = DecodedQueue::new(2, 10);
        q.try_reserve(6).unwrap();
        assert_eq!(q.try_push(1, 5), Err(QueueError::Full));
        q.try_push(1, 4).unwrap();
        assert_eq!(q.try_reserve(0), Err(QueueError::Full), "item budget");
        assert_eq!(q.usage().bytes, 10);
        assert_eq!(q.usage().items, 2);
        q.release_reserved(6);
        q.try_reserve(6).unwrap();
        // Abort frees queued entries; outstanding reservations release later.
        q.abort();
        q.release_reserved(6);
        assert_eq!(q.usage().bytes, 0);
    }

    #[tokio::test]
    async fn coalescing_never_admits_a_merged_update_beyond_the_byte_budget() {
        let big = "x".repeat(600);
        let input: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t", "rawInput": big
        }))
        .unwrap();
        let output: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t", "rawOutput": big
        }))
        .unwrap();
        let cost = encoded_len(&input);
        let max = cost + 100;
        let q = DecodedQueue::new(8, max);
        let cancel = CancellationToken::new();
        q.push_update(input.clone(), cost, &cancel).await.unwrap();
        // Full by bytes; merging would keep both fields (~2x) and exceed the
        // budget, so the update waits instead of replacing the entry.
        let blocked =
            futures_util::FutureExt::now_or_never(q.push_update(output.clone(), cost, &cancel));
        assert!(blocked.is_none(), "an oversize merge must wait");
        assert_eq!(q.coalesced(), 0);
        assert!(q.usage().high_bytes <= max);
        // Once drained, the separate updates are delivered in order.
        assert_eq!(q.try_pop(), Some(input));
        q.push_update(output.clone(), cost, &cancel).await.unwrap();
        assert_eq!(q.try_pop(), Some(output));
        assert!(q.usage().high_bytes <= max);
        // An update that can never fit is refused, not admitted as an exception.
        assert_eq!(
            q.push_update(status("t", "done"), max + 1, &cancel).await,
            Err(QueueError::Full)
        );
    }

    #[tokio::test]
    async fn a_full_queue_coalesces_replaceable_state_but_never_content() {
        let cancel = CancellationToken::new();
        let q = DecodedQueue::new(2, 1 << 20);
        q.push_update(status("t1", "pending"), 10, &cancel)
            .await
            .unwrap();
        q.push_update(chunk("a"), 10, &cancel).await.unwrap();
        // Full: the newer status of t1 replaces the queued one.
        q.push_update(status("t1", "completed"), 10, &cancel)
            .await
            .unwrap();
        assert_eq!(q.coalesced(), 1);
        assert_eq!(q.try_pop(), Some(chunk("a")));
        assert_eq!(q.try_pop(), Some(status("t1", "completed")));
        // Content never coalesces: a full queue makes it wait.
        q.push_update(chunk("b"), 10, &cancel).await.unwrap();
        q.push_update(chunk("c"), 10, &cancel).await.unwrap();
        let blocked = futures_util::FutureExt::now_or_never(q.push_update(chunk("d"), 10, &cancel));
        assert!(blocked.is_none(), "content must wait for space");
        assert_eq!(q.usage().items, 2);
        cancel.cancel();
        assert_eq!(
            q.push_update(chunk("d"), 10, &cancel).await,
            Err(QueueError::Closed)
        );
    }

    #[test]
    fn a_merged_tool_update_keeps_earlier_fields() {
        let older: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t", "title": "Edit", "status": "pending"
        }))
        .unwrap();
        let merged = merge(older, status("t", "completed"));
        let SessionUpdate::ToolCallUpdate { tool_call } = merged else {
            panic!("not a tool update");
        };
        assert_eq!(tool_call.title.as_deref(), Some("Edit"));
        assert_eq!(tool_call.status.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn stderr_drops_oldest_and_counts_the_bytes() {
        let ring = StderrRing::new(2, 1 << 20);
        ring.push(b"one".to_vec());
        ring.push(b"two".to_vec());
        ring.push(b"three".to_vec());
        assert_eq!(ring.pop().await, Some((Some(b"two".to_vec()), 3)));
        assert_eq!(ring.pop().await, Some((Some(b"three".to_vec()), 0)));
        assert_eq!(ring.usage().1, 3);
        ring.close();
        assert_eq!(ring.pop().await, None);
    }
}
