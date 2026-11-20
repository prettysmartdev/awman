//! JSON-RPC transport, correlation, and inbound ACP dispatch.
//!
//! The transport is bounded end to end (see [`crate::engine::acp::budget`]):
//!
//! - a **pump** task moves container stdout into the client, charging the
//!   stdout budget itself when the runtime bridge does not apply it;
//! - a **dispatcher** task frames and decodes lines in order, completes
//!   pending requests, answers agent-initiated requests, and queues permission
//!   requests on the *control* queue and session updates on the *bulk* queue;
//! - a **stderr** pump keeps the newest diagnostics and counts what it drops.
//!
//! Every task selects on the client's cancellation token. Cancelling (or
//! dropping the [`AcpClient`]) stops them, frees what is queued, closes the
//! stream gates so a blocked bridge thread resumes and discards, and fails
//! every pending request.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::acp::budget::{
    AcpBudgets, AcpQueueStats, DecodedQueue, PushUpdate, QueueError, QueueUsage, StderrRing,
};
use crate::engine::acp::protocol::{
    encode_line, JsonRpcError, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    LineFramer, PermissionDecision, PermissionRequest, ReadTextFileRequest, SessionUpdate,
    SessionUpdateNotification, WriteTextFileRequest, MAX_ACP_LINE_BYTES,
};
use crate::engine::agent_runtime::frontend::{
    AcpIoGates, AgentFrontend, AgentIo, AgentProgress, AgentStatus, GateError, StreamGate,
};
use crate::engine::error::EngineError;

type SharedSink = Arc<Mutex<Box<dyn UserMessageSink>>>;
type PendingSender = oneshot::Sender<Answer>;

/// A response delivered to a waiting request. Until the caller takes it (or
/// stops waiting and drops it) its bytes are charged to the control budget,
/// so unread responses cannot accumulate beyond it.
struct Answer {
    result: Result<Value, EngineError>,
    _credit: Option<ControlCredit>,
}

impl Answer {
    fn failed(error: EngineError) -> Self {
        Self {
            result: Err(error),
            _credit: None,
        }
    }
}

/// Returns a response's control-budget charge when dropped.
struct ControlCredit {
    shared: Weak<Shared>,
    cost: usize,
}

impl Drop for ControlCredit {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.control.release_reserved(self.cost);
        }
    }
}

/// The largest line the client writes: a maximum-size frame plus newline.
const MAX_OUTBOUND_LINE_BYTES: usize = MAX_ACP_LINE_BYTES + 1;

/// I/O detached from an ACP runtime frontend. It is consumed once by
/// [`AcpClient::new`] after the runtime has taken the matching `AgentIo`.
pub struct AcpTransport {
    stdout_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stderr_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
    gates: AcpIoGates,
    budgets: AcpBudgets,
}

/// Runtime adapter used to select the persistent piped ACP spawn path.
///
/// `initial_size` and `resize` are deliberately `None`; a PTY would corrupt
/// line framing. It carries no presentation logic. It offers the transport's
/// byte budgets to the runtime bridge through
/// [`AgentFrontend::acp_io_gates`].
pub struct AcpTransportFrontend {
    io: Option<AgentIo>,
    gates: AcpIoGates,
}

impl AcpTransport {
    /// A transport with the production budgets.
    pub fn channel() -> (AcpTransportFrontend, Self) {
        Self::with_budgets(AcpBudgets::default())
    }

    /// A transport with explicit budgets (tests and diagnostics).
    pub fn with_budgets(budgets: AcpBudgets) -> (AcpTransportFrontend, Self) {
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        let gates = AcpIoGates {
            stdout: StreamGate::new(budgets.stdout_items, budgets.stdout_bytes),
            stdin: StreamGate::new(budgets.stdin_items, budgets.stdin_bytes),
            stderr: StreamGate::new(budgets.stderr_items, budgets.stderr_bytes),
        };
        (
            AcpTransportFrontend {
                io: Some(AgentIo {
                    stdout: stdout_tx,
                    stderr: stderr_tx,
                    stdin_tx: stdin_tx.clone(),
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }),
                gates: gates.clone(),
            },
            Self {
                stdout_rx,
                stderr_rx,
                stdin_tx,
                gates,
                budgets,
            },
        )
    }
}

impl UserMessageSink for AcpTransportFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for AcpTransportFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn acp_io_gates(&self) -> Option<AcpIoGates> {
        Some(self.gates.clone())
    }
    fn take_io(&mut self) -> AgentIo {
        self.io
            .take()
            .expect("AcpTransportFrontend::take_io called more than once")
    }
}

/// Container-only filesystem callback. An implementation may use a container
/// backend API, but it must never translate an ACP path into a host path.
pub trait ContainerFileSystem: Send + Sync {
    fn read_text_file(&self, request: &ReadTextFileRequest) -> Result<String, EngineError>;
    fn write_text_file(&self, request: &WriteTextFileRequest) -> Result<(), EngineError>;
}

/// Default filesystem policy. It intentionally refuses requests rather than
/// accidentally treating an absolute container path as a host filesystem path.
pub struct DenyHostFilesystem;

impl ContainerFileSystem for DenyHostFilesystem {
    fn read_text_file(&self, _request: &ReadTextFileRequest) -> Result<String, EngineError> {
        Err(EngineError::Acp(
            "container filesystem service is unavailable".into(),
        ))
    }
    fn write_text_file(&self, _request: &WriteTextFileRequest) -> Result<(), EngineError> {
        Err(EngineError::Acp(
            "container filesystem service is unavailable".into(),
        ))
    }
}

/// Requests initiated by an ACP agent which require the portable session UI.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingRequest {
    Permission(PermissionRequest),
}

/// State shared by the client handle and its tasks.
struct Shared {
    stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
    gates: AcpIoGates,
    max_pending: usize,
    pending: Mutex<HashMap<JsonRpcId, PendingSender>>,
    high_pending: AtomicUsize,
    control: DecodedQueue<IncomingRequest>,
    bulk: DecodedQueue<SessionUpdate>,
    stderr: StderrRing,
    cancel: CancellationToken,
    failure: Mutex<Option<&'static str>>,
    reader_stopped: AtomicBool,
    /// Decoded-ahead session updates and the framer's partial line.
    decode_ahead: Mutex<QueueUsage>,
}

impl Shared {
    fn closed_error(&self) -> EngineError {
        match self.failure() {
            Some(queue) => EngineError::AcpOverflow { queue },
            None => EngineError::Acp("ACP connection closed".into()),
        }
    }

    fn failure(&self) -> Option<&'static str> {
        *self.failure.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A budget that must not wait was exceeded: record it (first wins) and
    /// cancel the session.
    fn overflow(&self, queue: &'static str) {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(queue);
        self.shutdown();
    }

    /// Stop every task, free queued data, unblock the bridge and fail
    /// pending requests. Idempotent.
    fn shutdown(&self) {
        self.cancel.cancel();
        self.control.abort();
        self.bulk.abort();
        self.stderr.abort();
        self.gates.stdout.close();
        self.gates.stdin.close();
        self.gates.stderr.close();
        self.fail_pending();
    }

    /// Refuse a line no conforming peer could accept, before it is charged
    /// to a gate (a gate admits one item into an empty budget).
    fn check_outbound(len: usize) -> Result<(), EngineError> {
        if len > MAX_OUTBOUND_LINE_BYTES {
            return Err(EngineError::Acp(format!(
                "ACP message of {len} bytes exceeds the {MAX_ACP_LINE_BYTES}-byte line limit"
            )));
        }
        Ok(())
    }

    fn fail_pending(&self) {
        let drained: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for (_, sender) in drained {
            let _ = sender.send(Answer::failed(self.closed_error()));
        }
    }

    /// Write one encoded line toward the container, waiting (cancellably) for
    /// stdin budget.
    async fn send_bytes(&self, bytes: Vec<u8>) -> Result<(), EngineError> {
        let len = bytes.len();
        Self::check_outbound(len)?;
        let gate = &self.gates.stdin;
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(self.closed_error()),
            acquired = gate.acquire(len) => {
                if acquired.is_err() {
                    return Err(self.closed_error());
                }
            }
        }
        if self.stdin_tx.send(bytes).is_err() {
            gate.release(len);
            return Err(EngineError::Acp("ACP stdin connection closed".into()));
        }
        // A bridge that does not release after writing gives no consumption
        // signal; do not hold budget it will never return.
        if !gate.is_attached() {
            gate.release(len);
        }
        Ok(())
    }

    /// Like [`send_bytes`](Self::send_bytes) but never waits: teardown must
    /// not depend on an agent that stopped reading its stdin.
    fn send_bytes_now(&self, bytes: Vec<u8>) -> Result<(), EngineError> {
        let len = bytes.len();
        Self::check_outbound(len)?;
        let gate = &self.gates.stdin;
        match gate.try_acquire(len) {
            Ok(()) => {}
            Err(GateError::Full) => return Err(EngineError::AcpOverflow { queue: "stdin" }),
            Err(GateError::Closed) => return Err(self.closed_error()),
        }
        if self.stdin_tx.send(bytes).is_err() {
            gate.release(len);
            return Err(EngineError::Acp("ACP stdin connection closed".into()));
        }
        if !gate.is_attached() {
            gate.release(len);
        }
        Ok(())
    }

    async fn send<T: Serialize>(&self, message: &T) -> Result<(), EngineError> {
        self.send_bytes(encode_line(message).map_err(EngineError::Acp)?)
            .await
    }

    fn stats(&self) -> AcpQueueStats {
        let (stderr, stderr_dropped_bytes) = self.stderr.usage();
        AcpQueueStats {
            stdout: self.gates.stdout.usage().into(),
            stdin: self.gates.stdin.usage().into(),
            stderr,
            stderr_transport: self.gates.stderr.usage().into(),
            decode_ahead: *self.decode_ahead.lock().unwrap_or_else(|e| e.into_inner()),
            bulk: self.bulk.usage(),
            control: self.control.usage(),
            pending_requests: self.pending.lock().unwrap_or_else(|e| e.into_inner()).len(),
            high_pending_requests: self.high_pending.load(Ordering::SeqCst),
            stderr_dropped_bytes,
            coalesced_updates: self.bulk.coalesced(),
            reader_stopped: self.reader_stopped.load(Ordering::SeqCst),
        }
    }
}

/// Removes a request's pending entry when its caller stops waiting (timeout,
/// cancellation, drop), so a never-answered request does not hold a slot.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: JsonRpcId,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Cancels an ACP client from outside the task driving it.
#[derive(Clone)]
pub struct AcpCancelHandle {
    shared: Arc<Shared>,
}

impl AcpCancelHandle {
    /// Stop the reader tasks, free queued data and fail pending requests.
    pub fn cancel(&self) {
        self.shared.shutdown();
    }

    pub fn is_cancelled(&self) -> bool {
        self.shared.cancel.is_cancelled()
    }

    /// Current occupancy and high-water marks of every queue.
    pub fn stats(&self) -> AcpQueueStats {
        self.shared.stats()
    }
}

/// Permission requests from the agent, in arrival order.
pub struct AcpIncoming {
    shared: Arc<Shared>,
}

impl AcpIncoming {
    /// The next request; `None` once the connection ended or was cancelled.
    pub async fn recv(&mut self) -> Option<IncomingRequest> {
        self.shared.control.pop().await
    }
    pub fn try_recv(&mut self) -> Option<IncomingRequest> {
        self.shared.control.try_pop()
    }
}

/// Session updates from the agent, in arrival order (replaceable state may
/// have been coalesced under pressure).
pub struct AcpUpdates {
    shared: Arc<Shared>,
}

impl AcpUpdates {
    /// The next update; `None` once the connection ended (after the queued
    /// tail) or was cancelled.
    pub async fn recv(&mut self) -> Option<SessionUpdate> {
        self.shared.bulk.pop().await
    }
    pub fn try_recv(&mut self) -> Option<SessionUpdate> {
        self.shared.bulk.try_pop()
    }
}

/// An ACP JSON-RPC client bound to one agent process's stdin/stdout.
pub struct AcpClient {
    shared: Arc<Shared>,
    next_id: AtomicI64,
    incoming_taken: AtomicBool,
    updates_taken: AtomicBool,
}

impl Drop for AcpClient {
    /// The consumer is gone: release everything the transport owns.
    fn drop(&mut self) {
        self.shared.shutdown();
    }
}

impl AcpClient {
    pub fn new(transport: AcpTransport, sink: Box<dyn UserMessageSink>) -> Self {
        Self::with_filesystem(transport, sink, Arc::new(DenyHostFilesystem))
    }

    /// Construct a client with a custom filesystem service. Restricted to the
    /// crate: an `fs/read_text_file`/`fs/write_text_file` handler MUST operate
    /// strictly within the running container's own filesystem and MUST NEVER
    /// interpret an agent-supplied path against awman's host filesystem (see
    /// `aspec/architecture/security.md`). Production uses [`DenyHostFilesystem`]
    /// via [`AcpClient::new`]; no container-scoped implementation exists yet, so
    /// keeping this seam crate-private prevents an out-of-tree caller from
    /// wiring a host `std::fs` implementation into the ACP fs handlers.
    pub(crate) fn with_filesystem(
        transport: AcpTransport,
        sink: Box<dyn UserMessageSink>,
        filesystem: Arc<dyn ContainerFileSystem>,
    ) -> Self {
        let AcpTransport {
            stdout_rx,
            stderr_rx,
            stdin_tx,
            gates,
            budgets,
        } = transport;
        let shared = Arc::new(Shared {
            stdin_tx,
            gates,
            max_pending: budgets.pending_requests.max(1),
            pending: Mutex::new(HashMap::new()),
            high_pending: AtomicUsize::new(0),
            control: DecodedQueue::new(budgets.control_items, budgets.control_bytes),
            bulk: DecodedQueue::new(budgets.bulk_items, budgets.bulk_bytes),
            stderr: StderrRing::new(budgets.stderr_items, budgets.stderr_bytes),
            cancel: CancellationToken::new(),
            failure: Mutex::new(None),
            reader_stopped: AtomicBool::new(false),
            decode_ahead: Mutex::new(QueueUsage {
                max_items: budgets.decode_ahead_items.max(1),
                max_bytes: budgets.decode_ahead_bytes.max(1),
                ..QueueUsage::default()
            }),
        });
        let sink = Arc::new(Mutex::new(sink));
        spawn_tasks(shared.clone(), stdout_rx, stderr_rx, filesystem, sink);
        Self {
            shared,
            next_id: AtomicI64::new(1),
            incoming_taken: AtomicBool::new(false),
            updates_taken: AtomicBool::new(false),
        }
    }

    pub fn take_incoming(&mut self) -> AcpIncoming {
        assert!(
            !self.incoming_taken.swap(true, Ordering::SeqCst),
            "AcpClient incoming receiver already taken"
        );
        AcpIncoming {
            shared: self.shared.clone(),
        }
    }

    pub fn take_updates(&mut self) -> AcpUpdates {
        assert!(
            !self.updates_taken.swap(true, Ordering::SeqCst),
            "AcpClient update receiver already taken"
        );
        AcpUpdates {
            shared: self.shared.clone(),
        }
    }

    pub fn cancel_handle(&self) -> AcpCancelHandle {
        AcpCancelHandle {
            shared: self.shared.clone(),
        }
    }

    /// The overflow that cancelled this client, if any.
    pub fn failure(&self) -> Option<EngineError> {
        self.shared
            .failure()
            .map(|queue| EngineError::AcpOverflow { queue })
    }

    pub fn stats(&self) -> AcpQueueStats {
        self.shared.stats()
    }

    /// Send a request and wait for its response. At most
    /// `pending_requests` may be outstanding; a further one fails with
    /// [`EngineError::AcpOverflow`]. Dropping the future frees its slot.
    pub async fn request<T: Serialize>(
        &self,
        method: &str,
        params: T,
    ) -> Result<Value, EngineError> {
        let shared = &*self.shared;
        if shared.cancel.is_cancelled() {
            return Err(shared.closed_error());
        }
        let id = JsonRpcId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() >= shared.max_pending {
                return Err(EngineError::AcpOverflow {
                    queue: "pending_requests",
                });
            }
            pending.insert(id.clone(), tx);
            shared
                .high_pending
                .fetch_max(pending.len(), Ordering::SeqCst);
        }
        let _guard = PendingGuard {
            shared,
            id: id.clone(),
        };
        shared
            .send(&JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id,
                method: method.into(),
                params,
            })
            .await?;
        tokio::select! {
            response = rx => match response {
                // The credit is returned as the answer is dropped here.
                Ok(answer) => answer.result,
                Err(_) => Err(EngineError::Acp(format!(
                    "ACP connection closed while waiting for {method}"
                ))),
            },
            _ = shared.cancel.cancelled() => Err(shared.closed_error()),
        }
    }

    pub async fn notify<T: Serialize>(&self, method: &str, params: T) -> Result<(), EngineError> {
        self.shared
            .send(&JsonRpcNotification {
                jsonrpc: "2.0".into(),
                method: method.into(),
                params,
            })
            .await
    }

    pub async fn respond_permission(
        &self,
        request_id: JsonRpcId,
        decision: PermissionDecision,
    ) -> Result<(), EngineError> {
        self.respond_result(request_id, decision.as_result()).await
    }

    pub async fn respond_result(&self, id: JsonRpcId, result: Value) -> Result<(), EngineError> {
        self.shared
            .send(&JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(result),
                error: None,
            })
            .await
    }

    /// Send a notification only if the stdin budget has room now.
    pub fn notify_now<T: Serialize>(&self, method: &str, params: T) -> Result<(), EngineError> {
        self.shared.send_bytes_now(
            encode_line(&JsonRpcNotification {
                jsonrpc: "2.0".into(),
                method: method.into(),
                params,
            })
            .map_err(EngineError::Acp)?,
        )
    }

    /// Send a response only if the stdin budget has room now.
    pub fn respond_now(&self, id: JsonRpcId, result: Value) -> Result<(), EngineError> {
        self.shared.send_bytes_now(
            encode_line(&JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(result),
                error: None,
            })
            .map_err(EngineError::Acp)?,
        )
    }
}

fn spawn_tasks(
    shared: Arc<Shared>,
    mut stdout_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut stderr_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    filesystem: Arc<dyn ContainerFileSystem>,
    sink: SharedSink,
) {
    // stderr: keep draining into the ring so the runtime channel never
    // grows; a separate writer surfaces it (without interpreting it as
    // protocol frames). A bridge that honours the stderr gate drops (and
    // counts) at ingress while this pump is behind, so the channel itself
    // stays within the stderr budget.
    let pump = shared.clone();
    tokio::spawn(async move {
        let gate = &pump.gates.stderr;
        loop {
            tokio::select! {
                biased;
                _ = pump.cancel.cancelled() => break,
                chunk = stderr_rx.recv() => match chunk {
                    Some(chunk) => {
                        let len = chunk.len();
                        pump.stderr.note_dropped(gate.take_dropped());
                        pump.stderr.push(chunk);
                        if gate.is_attached() {
                            gate.release(len);
                        }
                    }
                    None => break,
                },
            }
        }
        pump.stderr.note_dropped(gate.take_dropped());
        pump.stderr.close();
    });
    let writer = shared.clone();
    let stderr_sink = sink.clone();
    tokio::spawn(async move {
        while let Some((chunk, dropped)) = writer.stderr.pop().await {
            if dropped > 0 {
                warning(
                    &stderr_sink,
                    format!("{dropped} bytes of agent stderr dropped"),
                );
            }
            if let Some(chunk) = chunk {
                let text = String::from_utf8_lossy(&chunk).trim().to_owned();
                if !text.is_empty() {
                    warning(&stderr_sink, format!("ACP agent stderr: {text}"));
                }
            }
        }
    });

    // stdout pump: move runtime chunks to the dispatcher. When the bridge
    // honours the stdout gate it has already acquired each chunk (and fails
    // the gate if it cannot wait and finds it full); otherwise the pump
    // charges it and refuses to let an unbounded stream outgrow the budget.
    let (staged_tx, mut staged_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let pump = shared.clone();
    tokio::spawn(async move {
        let gate = &pump.gates.stdout;
        loop {
            tokio::select! {
                biased;
                _ = pump.cancel.cancelled() => break,
                _ = gate.closed() => {
                    if gate.is_overflowed() {
                        pump.overflow("transport_stdout");
                    }
                    break;
                }
                chunk = stdout_rx.recv() => {
                    let Some(chunk) = chunk else { break };
                    if !gate.is_attached() && gate.try_acquire(chunk.len()).is_err() {
                        if !gate.is_closed() {
                            pump.overflow("transport_stdout");
                        }
                        break;
                    }
                    if staged_tx.send(chunk).is_err() {
                        break;
                    }
                }
            }
        }
        // Dropping `stdout_rx` here makes further bridge sends fail, so the
        // bridge drains and discards rather than queueing for nobody.
    });

    // Dispatcher. Control frames (permission requests, RPC responses,
    // agent-initiated requests) are handled as soon as they are framed.
    // Session updates that do not fit the bulk queue wait in `ahead` while
    // framing continues, so a saturated bulk queue never hides a later
    // control frame. `ahead` and the framer's partial line together are
    // bounded by the decode-ahead budget: once it is reached no further
    // chunk is taken, the staged chunks keep their stdout budget, and the
    // bridge stops reading.
    tokio::spawn(async move {
        let mut framer = LineFramer::default();
        let mut ahead = DecodeAhead::default();
        let mut input_open = true;
        'run: loop {
            let writable = shared.bulk.writable();
            tokio::pin!(writable);
            writable.as_mut().enable();
            while let Some((update, cost)) = ahead.pop() {
                match shared.bulk.try_push_update(update, cost) {
                    Ok(()) => {}
                    Err(PushUpdate::Full(update, cost)) => {
                        ahead.unpop(*update, cost);
                        break;
                    }
                    Err(PushUpdate::TooLarge) => {
                        shared.overflow("bulk");
                        break 'run;
                    }
                    Err(PushUpdate::Closed) => break 'run,
                }
            }
            shared.note_decode_ahead(&ahead, framer.buffered());
            if !input_open && ahead.is_empty() {
                break;
            }
            let room = ahead.is_empty() || shared.decode_ahead_has_room(&ahead, framer.buffered());
            let chunk = tokio::select! {
                biased;
                _ = shared.cancel.cancelled() => break,
                _ = &mut writable, if !ahead.is_empty() => continue,
                chunk = staged_rx.recv(), if input_open && room => match chunk {
                    Some(chunk) => chunk,
                    None => {
                        input_open = false;
                        continue;
                    }
                },
            };
            let len = chunk.len();
            for frame in framer.push_sized(&chunk) {
                match frame.value {
                    Ok(value) => {
                        match dispatch(&shared, value, frame.bytes, filesystem.as_ref(), &sink)
                            .await
                        {
                            Ok(Some((update, cost))) => ahead.push(update, cost),
                            Ok(None) => {}
                            Err(Stop) => break 'run,
                        }
                    }
                    Err(error) => warning(
                        &sink,
                        format!("ACP protocol warning: ignoring malformed line: {error}"),
                    ),
                }
            }
            // The chunk's bytes now live in the framer or the decode-ahead
            // queue, which are bounded on their own.
            shared.gates.stdout.release(len);
            shared.note_decode_ahead(&ahead, framer.buffered());
        }
        drop(framer);
        drop(ahead);
        // End of stream or cancellation. Queued updates and requests stay
        // readable after a clean end; cancellation already freed them.
        shared.control.close();
        shared.bulk.close();
        shared.gates.stdout.close();
        shared.fail_pending();
        shared.reader_stopped.store(true, Ordering::SeqCst);
    });
}

/// Session updates decoded while the bulk queue was full, in order.
#[derive(Default)]
struct DecodeAhead {
    updates: VecDeque<(SessionUpdate, usize)>,
    bytes: usize,
}

impl DecodeAhead {
    fn push(&mut self, update: SessionUpdate, cost: usize) {
        self.bytes += cost;
        self.updates.push_back((update, cost));
    }
    fn pop(&mut self) -> Option<(SessionUpdate, usize)> {
        let (update, cost) = self.updates.pop_front()?;
        self.bytes -= cost;
        Some((update, cost))
    }
    fn unpop(&mut self, update: SessionUpdate, cost: usize) {
        self.bytes += cost;
        self.updates.push_front((update, cost));
    }
    fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }
}

impl Shared {
    fn decode_ahead_has_room(&self, ahead: &DecodeAhead, partial: usize) -> bool {
        let usage = self.decode_ahead.lock().unwrap_or_else(|e| e.into_inner());
        ahead.updates.len() < usage.max_items && ahead.bytes + partial < usage.max_bytes
    }

    fn note_decode_ahead(&self, ahead: &DecodeAhead, partial: usize) {
        let mut usage = self.decode_ahead.lock().unwrap_or_else(|e| e.into_inner());
        usage.items = ahead.updates.len();
        usage.bytes = ahead.bytes + partial;
        usage.high_items = usage.high_items.max(usage.items);
        usage.high_bytes = usage.high_bytes.max(usage.bytes);
    }
}

/// The session is gone (closed queue or cancellation): stop dispatching.
struct Stop;

async fn dispatch(
    shared: &Arc<Shared>,
    value: Value,
    cost: usize,
    filesystem: &dyn ContainerFileSystem,
    sink: &SharedSink,
) -> Result<Option<(SessionUpdate, usize)>, Stop> {
    let Some(object) = value.as_object() else {
        warning(
            sink,
            "ACP protocol warning: ignoring non-object JSON-RPC frame".into(),
        );
        return Ok(None);
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        warning(
            sink,
            "ACP protocol warning: ignoring frame without jsonrpc: 2.0".into(),
        );
        return Ok(None);
    }
    if object.contains_key("result") || object.contains_key("error") {
        let Some(id) = object.get("id").and_then(parse_id) else {
            warning(
                sink,
                "ACP protocol warning: response has no usable id".into(),
            );
            return Ok(None);
        };
        let sender = shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if let Some(sender) = sender {
            // Charge the response to the control budget until its caller
            // takes it; a caller that never does cannot grow memory past it.
            match shared.control.try_reserve(cost) {
                Ok(()) => {}
                Err(QueueError::Full) => {
                    shared.overflow("control");
                    let _ = sender.send(Answer::failed(EngineError::AcpOverflow {
                        queue: "control",
                    }));
                    return Err(Stop);
                }
                Err(QueueError::Closed) => {
                    let _ = sender.send(Answer::failed(shared.closed_error()));
                    return Err(Stop);
                }
            }
            let credit = ControlCredit {
                shared: Arc::downgrade(shared),
                cost,
            };
            let result = if let Some(error) = object.get("error") {
                Err(EngineError::Acp(format!("agent RPC error: {error}")))
            } else {
                Ok(object.get("result").cloned().unwrap_or(Value::Null))
            };
            // A caller that already stopped waiting drops the answer (and
            // its credit) right here.
            let _ = sender.send(Answer {
                result,
                _credit: Some(credit),
            });
        }
        return Ok(None);
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        warning(
            sink,
            "ACP protocol warning: request/notification has no method".into(),
        );
        return Ok(None);
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => match serde_json::from_value::<SessionUpdateNotification>(params) {
            Ok(notification) => {
                // Bulk: the dispatcher queues it, coalescing replaceable
                // state, and keeps framing control traffic meanwhile.
                return Ok(Some((notification.update, cost)));
            }
            Err(e) => warning(
                sink,
                format!("ACP protocol warning: invalid session/update: {e}"),
            ),
        },
        "session/request_permission" => {
            let Some(id) = object.get("id").and_then(parse_id) else {
                warning(
                    sink,
                    "ACP protocol warning: permission request has no id".into(),
                );
                return Ok(None);
            };
            match serde_json::from_value::<PermissionRequestWire>(params) {
                Ok(request) => {
                    let request = PermissionRequest {
                        request_id: id,
                        session_id: request.session_id,
                        tool_call: request.tool_call,
                        options: request.options,
                    };
                    // Control never waits behind anything: a full control
                    // queue is an explicit failure, never a silent drop.
                    match shared
                        .control
                        .try_push(IncomingRequest::Permission(request), cost)
                    {
                        Ok(()) => {}
                        Err(QueueError::Full) => {
                            shared.overflow("control");
                            return Err(Stop);
                        }
                        Err(QueueError::Closed) => return Err(Stop),
                    }
                }
                Err(e) => {
                    warning(
                        sink,
                        format!("ACP protocol warning: invalid permission request: {e}"),
                    );
                    send_error(shared, id, -32602, "invalid permission request").await?;
                }
            }
        }
        "fs/read_text_file" => {
            let Some(id) = object.get("id").and_then(parse_id) else {
                warning(sink, "ACP protocol warning: file request has no id".into());
                return Ok(None);
            };
            match serde_json::from_value::<ReadTextFileRequest>(params).and_then(|r| {
                filesystem
                    .read_text_file(&r)
                    .map_err(|e| serde_json::Error::io(std::io::Error::other(e.to_string())))
            }) {
                Ok(content) => {
                    send_result(shared, id, serde_json::json!({"content": content})).await?
                }
                Err(e) => send_error(shared, id, -32603, &e.to_string()).await?,
            }
        }
        "fs/write_text_file" => {
            let Some(id) = object.get("id").and_then(parse_id) else {
                warning(sink, "ACP protocol warning: file request has no id".into());
                return Ok(None);
            };
            match serde_json::from_value::<WriteTextFileRequest>(params).and_then(|r| {
                filesystem
                    .write_text_file(&r)
                    .map_err(|e| serde_json::Error::io(std::io::Error::other(e.to_string())))
            }) {
                Ok(()) => send_result(shared, id, serde_json::json!({})).await?,
                Err(e) => send_error(shared, id, -32603, &e.to_string()).await?,
            }
        }
        _ => {
            if let Some(id) = object.get("id").and_then(parse_id) {
                warning(
                    sink,
                    format!("ACP protocol warning: unsupported agent request '{method}'"),
                );
                send_error(shared, id, -32601, "method not supported by awman").await?;
            }
        }
    }
    Ok(None)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionRequestWire {
    session_id: String,
    tool_call: crate::engine::acp::protocol::ToolCallUpdate,
    options: Vec<crate::engine::acp::protocol::PermissionOption>,
}

fn parse_id(value: &Value) -> Option<JsonRpcId> {
    serde_json::from_value(value.clone()).ok()
}

/// Answer an agent-initiated request. Waiting for stdin budget is the
/// backpressure for an agent that floods requests without reading replies.
async fn send_result(shared: &Shared, id: JsonRpcId, result: Value) -> Result<(), Stop> {
    respond(
        shared,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        },
    )
    .await
}

async fn send_error(shared: &Shared, id: JsonRpcId, code: i64, message: &str) -> Result<(), Stop> {
    respond(
        shared,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        },
    )
    .await
}

async fn respond(shared: &Shared, response: JsonRpcResponse) -> Result<(), Stop> {
    match shared.send(&response).await {
        Ok(()) => Ok(()),
        Err(_) if shared.cancel.is_cancelled() => Err(Stop),
        // stdin closed while stdout is still readable: keep reading so the
        // remaining output and the exit still reach the driver.
        Err(_) => Ok(()),
    }
}

fn warning(sink: &SharedSink, text: String) {
    if let Ok(mut sink) = sink.lock() {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text,
        });
    }
}
