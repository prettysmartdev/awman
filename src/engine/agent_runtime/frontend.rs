//! `AgentFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Shared by both runtime tiers (container-class and sandbox-class): the
//! engine calls back into the frontend for PTY size, stdin, and status
//! reporting regardless of the underlying isolation paradigm.

use std::time::Duration;

use async_trait::async_trait;

use crate::data::message::UserMessageSink;
use crate::engine::container::timing::{DEFAULT_GRACE_TIMEOUT, DEFAULT_STUCK_TIMEOUT};

/// What stage an agent execution is in, as reported to
/// [`AgentFrontend::report_status`].
///
/// What a frontend can expect, on every runtime:
///
/// - `Running { container_name }` exactly once per `run_with_frontend`
///   (fresh launch or attach), after the agent process or exec session exists
///   and before any output is bridged. This is the only status every backend
///   emits, and the only one a frontend may rely on.
/// - Everything else is advisory. Builtin additionally reports `Starting`
///   (sandbox boot, before `Running`) and a terminal `Exited(code)` or
///   `Failed(reason)` from its exec event stream. Docker, Apple and the
///   sandbox tier do not: their frontend is consumed by `take_io` and dropped
///   when `run_with_frontend` returns, and their exit status comes from the
///   host CLI child, which only [`AgentExecution::wait`] observes.
/// - The authoritative end of an execution is `AgentExecution::wait()` (an
///   `AgentExitInfo` or an error), on every runtime. A frontend must not wait
///   for `Exited` or `Failed` to learn that an agent finished.
/// - `Building`, `Pulling` and `Stopping` are not emitted by any backend
///   today. Image build and import progress travels through the
///   `UserMessageSink` / image-preparation sink instead.
/// - OSC 7501 attention events travel on `StuckEvent`, on every bridged
///   runtime. Accepted reports supersede silence while records remain; clear
///   resumes silence from the clear. The reporting latch survives RIS and
///   lasts until execution end or the next step/container. Sandbox publishes
///   reports only, without silence/grace detection. Startup grace is unchanged.
///
/// [`AgentExecution::wait`]: crate::engine::agent_runtime::AgentExecution::wait
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Building,
    Pulling,
    /// Builtin only: the sandbox is booting.
    Starting,
    /// Every backend, once per `run_with_frontend`.
    Running {
        container_name: String,
    },
    Stopping,
    /// Builtin only: the guest process exited with this code. Others report
    /// the exit through `AgentExecution::wait`.
    Exited(i32),
    /// Builtin only: the exec stream failed. `wait` returns the error too.
    Failed(String),
}

/// A unit of progress reported during a long-running runtime action
/// (image pull, build step, layer extract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProgress {
    pub stage: String,
    pub message: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
}

/// Byte-stream I/O channels detached from a frontend so the engine can
/// bridge them to the agent process.
///
/// Every frontend must provide an `AgentIo` via `take_io()`.
/// The engine uses these channels exclusively for all agent I/O:
///
/// - **PTY path** (`resize`/`initial_size` are `Some`): the engine opens a
///   PTY via `portable-pty` and bridges it through these channels.
/// - **Piped path** (`resize`/`initial_size` are `None`): the engine spawns
///   the agent with `Stdio::piped()` and bridges through these channels.
///
/// The stdin direction has both ends because the frontend needs a sender
/// (for keystrokes) and the engine retains its own sender clone — used by
/// `AgentExecution::try_inject_stdin` to send a fresh prompt into a
/// still-running agent during workflow `ContinueInCurrentContainer`
/// advances.
pub struct AgentIo {
    /// Engine sends agent stdout bytes here.
    pub stdout: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Engine sends agent stderr bytes here.
    pub stderr: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Sender side of the stdin channel — engine retains a clone for
    /// `try_inject_stdin`; frontend also keeps its own clone for keystrokes.
    pub stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Receiver side of the stdin channel — consumed by the engine's writer
    /// task. Both the frontend (keystrokes) and the engine
    /// (`try_inject_stdin`) push into the matching sender.
    pub stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    /// PTY resize requests from the frontend. `Some` for interactive
    /// frontends (TUI, CLI with TTY), `None` for headless (CLI
    /// `--headless`, API). When `None`, the engine uses
    /// `Stdio::piped()` for the agent process.
    pub resize: Option<tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>>,
    /// Initial PTY size at spawn time. `Some` for interactive frontends,
    /// `None` for headless. When `None`, the engine uses
    /// `Stdio::piped()`.
    pub initial_size: Option<(u16, u16)>,
}

/// Why a [`StreamGate`] refused an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    /// The consumer is gone or the session was cancelled. A producer treats
    /// this like a closed channel.
    Closed,
    /// No space, and the caller asked not to wait.
    Full,
}

/// Snapshot of a [`StreamGate`]'s occupancy and high-water marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateUsage {
    pub items: usize,
    pub bytes: usize,
    pub high_items: usize,
    pub high_bytes: usize,
    pub max_items: usize,
    pub max_bytes: usize,
}

#[derive(Default)]
struct GateState {
    items: usize,
    bytes: usize,
    high_items: usize,
    high_bytes: usize,
    closed: bool,
    /// A non-waiting producer found the gate full (see [`send_bounded`]).
    overflowed: bool,
    /// Bytes a lossy producer dropped at ingress (see [`send_lossy`]),
    /// not yet reported by the consumer.
    dropped: u64,
}

struct GateInner {
    max_items: usize,
    max_bytes: usize,
    attached: std::sync::atomic::AtomicBool,
    state: std::sync::Mutex<GateState>,
    space: std::sync::Condvar,
    notify: tokio::sync::Notify,
}

/// An item and byte budget shared by the two ends of one [`AgentIo`] stream.
///
/// `AgentIo` keeps unbounded channel types (every runtime constructs it); a
/// stream that must be bounded carries a gate alongside. The producer
/// acquires an item's bytes before sending it and the consumer releases them
/// once it has processed the item, so at most `max_items` / `max_bytes` are in
/// flight. An item is always admitted into an empty gate, so one maximum-size
/// item can never deadlock it.
///
/// The far side honours a gate only after [`attach`](Self::attach). The ACP
/// transport charges an unattached producer itself and fails explicitly when
/// it overruns, instead of letting the channel grow.
#[derive(Clone)]
pub struct StreamGate(std::sync::Arc<GateInner>);

impl std::fmt::Debug for StreamGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StreamGate").field(&self.usage()).finish()
    }
}

impl StreamGate {
    pub fn new(max_items: usize, max_bytes: usize) -> Self {
        Self(std::sync::Arc::new(GateInner {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(1),
            attached: std::sync::atomic::AtomicBool::new(false),
            state: std::sync::Mutex::new(GateState::default()),
            space: std::sync::Condvar::new(),
            notify: tokio::sync::Notify::new(),
        }))
    }

    /// Declare that the far end of the stream honours this gate (a producer
    /// that acquires, or a consumer that releases). Call before the first
    /// item moves.
    pub fn attach(&self) {
        self.0
            .attached
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_attached(&self) -> bool {
        self.0.attached.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn admit(&self, state: &mut GateState, bytes: usize) -> bool {
        let fits = state.items == 0
            || (state.items < self.0.max_items && state.bytes + bytes <= self.0.max_bytes);
        if fits {
            state.items += 1;
            state.bytes += bytes;
            state.high_items = state.high_items.max(state.items);
            state.high_bytes = state.high_bytes.max(state.bytes);
        }
        fits
    }

    /// Wait, blocking the thread, until `bytes` fit. For reader threads.
    pub fn acquire_blocking(&self, bytes: usize) -> Result<(), GateError> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return Err(GateError::Closed);
            }
            if self.admit(&mut state, bytes) {
                return Ok(());
            }
            state = self.0.space.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Wait until `bytes` fit. Callers select this against their
    /// cancellation token; dropping the future acquires nothing.
    pub async fn acquire(&self, bytes: usize) -> Result<(), GateError> {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                if state.closed {
                    return Err(GateError::Closed);
                }
                if self.admit(&mut state, bytes) {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    /// Admit `bytes` now or report [`GateError::Full`].
    pub fn try_acquire(&self, bytes: usize) -> Result<(), GateError> {
        let mut state = self.lock();
        if state.closed {
            return Err(GateError::Closed);
        }
        if self.admit(&mut state, bytes) {
            Ok(())
        } else {
            Err(GateError::Full)
        }
    }

    /// Return one item of `bytes` to the budget.
    pub fn release(&self, bytes: usize) {
        {
            let mut state = self.lock();
            state.items = state.items.saturating_sub(1);
            state.bytes = state.bytes.saturating_sub(bytes);
        }
        self.wake();
    }

    /// Refuse every further acquire and wake all waiters. In-flight
    /// accounting is dropped with the data it described.
    pub fn close(&self) {
        {
            let mut state = self.lock();
            state.closed = true;
            state.items = 0;
            state.bytes = 0;
        }
        self.wake();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Close the gate because a producer that cannot wait found it full.
    /// The consumer reports this as an overflow instead of a clean end.
    pub fn fail_overflow(&self) {
        {
            let mut state = self.lock();
            if !state.closed {
                state.overflowed = true;
            }
            state.closed = true;
            state.items = 0;
            state.bytes = 0;
        }
        self.wake();
    }

    pub fn is_overflowed(&self) -> bool {
        self.lock().overflowed
    }

    /// Count bytes a lossy producer discarded because the gate was full.
    pub fn record_dropped(&self, bytes: usize) {
        self.lock().dropped += bytes as u64;
    }

    /// Bytes dropped at ingress since the previous call.
    pub fn take_dropped(&self) -> u64 {
        std::mem::take(&mut self.lock().dropped)
    }

    /// Resolves once the gate is closed (by either side).
    pub async fn closed(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().closed {
                return;
            }
            notified.await;
        }
    }

    pub fn usage(&self) -> GateUsage {
        let state = self.lock();
        GateUsage {
            items: state.items,
            bytes: state.bytes,
            high_items: state.high_items,
            high_bytes: state.high_bytes,
            max_items: self.0.max_items,
            max_bytes: self.0.max_bytes,
        }
    }

    fn wake(&self) {
        self.0.space.notify_all();
        self.0.notify.notify_waiters();
    }
}

/// Send `bytes` from a reader thread, honouring `gate` when there is one.
/// Returns `false` once the consumer is gone (closed gate or channel); the
/// caller then keeps draining its pipe and discards, as for a dead sink.
pub fn send_gated(
    gate: Option<&StreamGate>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    bytes: Vec<u8>,
) -> bool {
    let len = bytes.len();
    if let Some(gate) = gate {
        if gate.acquire_blocking(len).is_err() {
            return false;
        }
    }
    if tx.send(bytes).is_err() {
        if let Some(gate) = gate {
            gate.release(len);
        }
        return false;
    }
    true
}

/// Send `bytes` from a producer that must not wait (an async event loop
/// that also serves other consumers). A full gate is a hard overflow: the
/// gate is failed so the consumer ends the session explicitly, and the
/// producer treats the sink as gone. Nothing is queued beyond the gate's
/// budget.
pub fn send_bounded(
    gate: Option<&StreamGate>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    bytes: Vec<u8>,
) -> bool {
    let len = bytes.len();
    if let Some(gate) = gate {
        match gate.try_acquire(len) {
            Ok(()) => {}
            Err(GateError::Full) => {
                gate.fail_overflow();
                return false;
            }
            Err(GateError::Closed) => return false,
        }
    }
    if tx.send(bytes).is_err() {
        if let Some(gate) = gate {
            gate.release(len);
        }
        return false;
    }
    true
}

/// Send diagnostic `bytes` without waiting: when the gate is full the chunk
/// is dropped and counted (the consumer reports the count), so a stalled
/// consumer never lets the channel grow beyond the gate's budget. Returns
/// `false` once the consumer is gone.
pub fn send_lossy(
    gate: Option<&StreamGate>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    bytes: Vec<u8>,
) -> bool {
    let len = bytes.len();
    if let Some(gate) = gate {
        match gate.try_acquire(len) {
            Ok(()) => {}
            Err(GateError::Full) => {
                gate.record_dropped(len);
                return true;
            }
            Err(GateError::Closed) => return false,
        }
    }
    if tx.send(bytes).is_err() {
        if let Some(gate) = gate {
            gate.release(len);
        }
        return false;
    }
    true
}

/// The byte budgets of an ACP transport, offered by its runtime frontend so a
/// bridge applies backpressure instead of queueing without bound.
#[derive(Debug, Clone)]
pub struct AcpIoGates {
    /// Container stdout → ACP client. The bridge acquires before sending;
    /// the client releases once it has decoded the bytes.
    pub stdout: StreamGate,
    /// ACP client → container stdin. The client acquires before sending; the
    /// bridge's writer releases after writing each item to the child.
    pub stdin: StreamGate,
    /// Container stderr → ACP client. The bridge sends through
    /// [`send_lossy`] (full ⇒ dropped and counted at ingress); the client
    /// releases once the chunk reached its stderr ring.
    pub stderr: StreamGate,
}

/// Abstract agent-side I/O. Implementations live in Layer 3 (CLI binds
/// stdio, TUI binds a PTY, API binds an SSE/WebSocket stream).
///
/// The engine exclusively uses the channels from `take_io()` for
/// all agent I/O — stdout, stderr, and stdin.
#[async_trait]
pub trait AgentFrontend: UserMessageSink + Send {
    /// Already translated native output. True when the frontend consumed it;
    /// other frontends receive its ordinary rendered lines through AgentIo.
    fn translated_output(&mut self, _item: crate::engine::agent_output::Translated) -> bool {
        false
    }

    fn output_redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        Default::default()
    }
    fn terminal_launch(&self) -> Option<crate::engine::agent_runtime::execution::TerminalLaunch> {
        None
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        None
    }
    /// Engine launch configuration; wrappers may supply the resolved option
    /// for runtimes whose launch plans do not carry bridge configuration.
    fn program_status_config(
        &self,
    ) -> crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
        Default::default()
    }
    fn report_status(&mut self, status: AgentStatus);
    fn report_progress(&mut self, progress: AgentProgress);

    /// Byte budgets for an ACP transport. `None` for every other frontend;
    /// wrappers forward it. Read before `take_io`.
    fn acp_io_gates(&self) -> Option<AcpIoGates> {
        None
    }

    /// Detach the byte-stream I/O channels for engine bridging.
    ///
    /// The engine takes ownership of these channels in `run_with_frontend`
    /// and spawns reader/writer tasks. Every frontend must implement this.
    fn take_io(&mut self) -> AgentIo;

    /// Which structured output format the agent about to run will print on
    /// stdout (WI 0137 §4). The engine calls this once per launch, before
    /// `report_status(Running)` and therefore before `take_io`, when it has
    /// enabled a structured mode (`Text` means plain output). A frontend
    /// that translates agent output (the API) picks its translator from it;
    /// every other frontend keeps the default and ignores it.
    fn set_agent_output_format(
        &mut self,
        _format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
    }

    /// Startup-grace timeout: how long the agent has to emit its first
    /// byte of output before it is considered failed-to-start and killed.
    /// The regular stuck timer does not begin until the first byte is seen
    /// and the grace timer has been discarded.
    ///
    /// CLI/TUI keep the default (30s). API overrides to 15 minutes so an
    /// agent that needs to pull a large image or warm up a model isn't
    /// killed before it has a chance to produce output.
    fn grace_timeout(&self) -> Duration {
        DEFAULT_GRACE_TIMEOUT
    }

    /// Which startup clock applies (WI 0070). Automation launches return
    /// `LifecycleDeadline`, which disables the first-byte grace kill in every
    /// bridge; wrappers must forward it.
    fn startup_timing(&self) -> crate::data::automation::StartupTiming {
        Default::default()
    }

    /// The automation binding a launch registers with its terminal.
    fn terminal_automation(
        &self,
    ) -> Option<crate::engine::agent_runtime::execution::TerminalAutomationBinding> {
        None
    }

    /// Stuck timeout: how long the agent can go without producing any
    /// further output (after its first byte) before the engine publishes
    /// `StuckEvent::Stuck`. This is advisory only: it never kills the agent
    /// or changes readiness. Layer 2 snapshots `agents.stuckTimeout` at launch.
    fn stuck_timeout(&self) -> Duration {
        DEFAULT_STUCK_TIMEOUT
    }
}

/// Forward a shared, mutex-guarded frontend to the frontend it wraps.
///
/// The companion to the `WorkflowFrontend` impl of the same shape (F-36):
/// Layer 2 holds its frontend as `Arc<Mutex<Box<dyn …>>>` so an engine and an
/// execution factory can share it, and this impl lets that handle be used as
/// an `AgentFrontend` without a hand-written proxy struct that can drop a
/// method to its trait default.
///
/// `take_io` forwards straight through, so a caller that needs to pair one
/// container's I/O with that container's `Running` callback still wraps this
/// handle in its own stateful proxy.
#[async_trait]
impl<F: AgentFrontend + ?Sized> AgentFrontend for std::sync::Arc<std::sync::Mutex<Box<F>>> {
    fn terminal_launch(&self) -> Option<crate::engine::agent_runtime::execution::TerminalLaunch> {
        self.lock().unwrap().terminal_launch()
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        self.lock().unwrap().terminal_source()
    }
    fn program_status_config(
        &self,
    ) -> crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
        self.lock().unwrap().program_status_config()
    }
    fn report_status(&mut self, status: AgentStatus) {
        self.lock().unwrap().report_status(status);
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        self.lock().unwrap().report_progress(progress);
    }

    fn take_io(&mut self) -> AgentIo {
        self.lock().unwrap().take_io()
    }
    fn acp_io_gates(&self) -> Option<AcpIoGates> {
        self.lock().unwrap().acp_io_gates()
    }

    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        self.lock().unwrap().set_agent_output_format(format);
    }

    fn grace_timeout(&self) -> Duration {
        self.lock().unwrap().grace_timeout()
    }
    fn startup_timing(&self) -> crate::data::automation::StartupTiming {
        self.lock().unwrap().startup_timing()
    }
    fn terminal_automation(
        &self,
    ) -> Option<crate::engine::agent_runtime::execution::TerminalAutomationBinding> {
        self.lock().unwrap().terminal_automation()
    }

    fn stuck_timeout(&self) -> Duration {
        self.lock().unwrap().stuck_timeout()
    }
}

/// Bind a launch's resolved advisory timeout to a detached frontend sink.
pub fn with_stuck_timeout(
    frontend: Box<dyn AgentFrontend>,
    timeout: Duration,
) -> Box<dyn AgentFrontend> {
    Box::new(InactivityFrontend { frontend, timeout })
}
struct InactivityFrontend {
    frontend: Box<dyn AgentFrontend>,
    timeout: Duration,
}
impl UserMessageSink for InactivityFrontend {
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
#[async_trait]
impl AgentFrontend for InactivityFrontend {
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
    fn acp_io_gates(&self) -> Option<AcpIoGates> {
        self.frontend.acp_io_gates()
    }
    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        self.frontend.set_agent_output_format(format);
    }
    fn grace_timeout(&self) -> Duration {
        self.frontend.grace_timeout()
    }
    fn startup_timing(&self) -> crate::data::automation::StartupTiming {
        self.frontend.startup_timing()
    }
    fn terminal_automation(
        &self,
    ) -> Option<crate::engine::agent_runtime::execution::TerminalAutomationBinding> {
        self.frontend.terminal_automation()
    }
    fn stuck_timeout(&self) -> Duration {
        self.timeout
    }
}
