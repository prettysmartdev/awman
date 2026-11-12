//! `RemoteInstanceBackend`: the daemon-backed [`super::backend::InstanceBackend`]
//! (WI 0142 Part B §2 "On the client side"). Owns the `awman.frontend.v1`
//! transport (`engine::remote::frontend_client`), reconnect with backoff
//! (50 ms → 1 s, 2 s total), sequence checking, snapshot re-sync, the per-tab
//! producer ends of `TabFeeds`, and hub terminal subscriptions for agent
//! slots. It never builds commands and never imports a TUI type.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.7.
//!
//! # Shape
//!
//! The presenter (the TUI) holds exactly the channel ends and shared feeds it
//! holds for the builtin backend; this backend is the producer behind them.
//! Engine frames are mapped back onto those feeds, so a presenter cannot tell
//! which backend it holds:
//!
//! | Engine frame / delta | Presenter-side feed |
//! |---|---|
//! | `CommandFinished` | `command_result_rx` |
//! | `DialogRequest` | `dialog_request_rx` (ids kept in a per-tab FIFO) |
//! | `dialog_response_tx` answers | `DialogResponse{dialog_id}` |
//! | `SlotLaunched`, `SlotContainerName`, `SlotExited`, group, stuck, yolo deltas | `ContainerSlotEvent`s on the tab's `ContainerLifecycle` |
//! | a slot's hub terminal (`/v1/terminals/{id}`, 0141's `awman.terminal.v1` client) | that slot's `ContainerSlotIo` |
//! | `StatusLine`, `StatusLogReset` | `status_log` |
//! | `Workflow`, `Yolo`, `AgentsDashboard` | the matching `TabCommandFeeds` slot |
//! | `Phase` | the tab's mirrored `Session` state (`session_snapshot`) |
//! | `Notice`, `UiAck`, `Annotations`, frontends, `ProgramStatus`, `Detach` | [`BackendEvent`]s from `poll` |
//!
//! Presenter inputs that never cross a channel are read from the shared
//! feeds every 10 ms: the command's cancel token (`Cancel`), the yolo cancel
//! flags (`CancelCountdown`), the engine-request sender handed out in
//! `EngineAttached` (`WorkflowRequest`), and `AgentSignals::touch_user`
//! (`Activity`, at most one per second).
//!
//! # Threads
//!
//! The trait is synchronous (the TUI event loop is), so the backend runs its
//! socket on a private runtime on its own thread. Calls that need an answer
//! (`open_tab`, `submit`, `quit`) block the caller on a reply channel; they
//! never block a runtime the caller might be running on.
//!
//! # Reconnect
//!
//! When the socket fails without a `Detach`, a primary reconnects with
//! `Hello{resume: Some(frontend_id)}`: first after 50 ms, doubling up to 1 s,
//! for at most `PRIMARY_RECONNECT_GRACE` (2 s). The `Welcome` and snapshot
//! that follow re-sync every tab (missed status lines, slots, dialogs, a run
//! that finished during the gap); frames the presenter sent meanwhile were
//! queued and go out after the re-sync. Nothing of this reaches the
//! presenter. A `Detach`, a refusal, a protocol violation (security
//! invariant I8) or an exhausted grace is fatal: `health()` returns the
//! [`BackendLoss`] and `poll` reports `BackendEvent::Detached` where there
//! was one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::indicator::SharedSquadHealth;
use crate::command::commands::squad::supervisor::{SquadStartError, SquadStartupResult};
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::{CommandOutcome, LaunchDisplay};
use crate::command::error::CommandError;
use crate::command::git_summary::GitSummaryPoller;
use crate::command::stats_sampler::ContainerStatsSampler;
use crate::data::config::env::EnvSnapshot;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::frontend_protocol::{
    AnsweredBy, CommandResultWire, DetachReasonWire, EngineFrame, FrontendFrame, FrontendRole,
    Hello, OpenTabOptions, ProtocolError, Seq, SnapshotAssembler, SubmitInput, TabKindWire,
    TermSize, UiOutcome, UiRequest, WorkflowRequestWire, PRIMARY_RECONNECT_GRACE,
    RECONNECT_BACKOFF_INITIAL, RECONNECT_BACKOFF_MAX,
};
use crate::data::ids::{DialogId, FrontendId, InstanceId, TabId, TerminalId};
use crate::data::instance_snapshot::{
    ExecutionPhase, InstanceDelta, InstanceSnapshot, SlotId, SlotSnapshot, SquadHealthWire,
    TabSnapshot,
};
use crate::data::message::MessageLevel;
use crate::data::program_status::EffectiveStatus;
use crate::data::session::{
    CommandInvocation, InvocationStatus, Session, SessionId, SessionOpenOptions,
};
use crate::data::terminal::{TerminalClientFrame, TerminalFrame};
use crate::engine::agent_runtime::execution::{AgentStats, StuckEvent};
use crate::engine::remote::frontend_client::{
    FrontendReceiver, FrontendSender, FrontendStream, FrontendStreamError,
};
use crate::engine::remote::local::{LocalClient, LocalEndpoint};
use crate::engine::remote::terminal_client::{TerminalClientOptions, TerminalStream};
use crate::engine::squad::supervisor::SquadHealth;
use crate::engine::workflow::EngineRequest;

use super::backend::{
    BackendEvent, BackendKind, BackendLoss, InstanceBackend, PresenterSlot, UiTicket,
};
use super::feeds::{
    ContainerSlotEvent, ContainerSlotIo, SharedYoloCancelFlag, TabCommandFeeds, TabFeeds,
};
use super::{
    CloseTabOutcome, InitialTabOpened, InitialTabSpec, OpenTabRequest, OpenedTab, QuitPlan,
    StartupRun, SubmitRequest, SubmittedCommand,
};

/// How long a call that needs the engine's answer (`open_tab`, `submit`)
/// waits. It covers a reconnect inside the grace.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the first `Welcome` and snapshot may take: the engine opens the
/// instance (startup, runtime detection) before it answers a primary.
pub const WELCOME_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `quit` waits for the engine's `Detach`.
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How often presenter-side inputs (cancel tokens, flags, answers) are read.
// The facade retains std::mpsc response senders for builtin parity. Poll
// those inputs within the socket's 5 ms budget; Tokio's timer yields between
// polls, including when no presenter input is pending.
const INPUT_POLL: Duration = Duration::from_millis(1);
/// `Activity` is coalesced to at most one frame per tab per interval.
const ACTIVITY_INTERVAL: Duration = Duration::from_secs(1);

// ─── Public surface ─────────────────────────────────────────────────────────

/// What [`RemoteInstanceBackend::connect`] needs.
pub struct RemoteOpen {
    /// The daemon's local socket or named pipe.
    pub address: PathBuf,
    /// The daemon's PID (reported by [`BackendKind::Daemon`]).
    pub pid: u32,
    /// The frontend's `Hello`: kind, role, merged annotations, cwd, argv,
    /// env, term size and TTY facts. `resume` is managed by the backend.
    pub hello: Hello,
    /// `POST /v1/engine/instances` first (a primary). `false` attaches to
    /// `instance_id` (a secondary, `awman view`).
    pub create: bool,
    pub instance_id: Option<InstanceId>,
    /// The catalogue the presenter parses against; submissions are re-sent
    /// as argv derived from it so the engine re-parses them (D3).
    pub catalogue: &'static CommandCatalogue,
    /// The frontend's own environment: the presenter-side session mirrors
    /// are opened with it. Never sent anywhere except inside `hello`.
    pub env: EnvSnapshot,
}

impl RemoteOpen {
    /// A primary that creates its instance on `endpoint`.
    pub fn primary(
        endpoint: &LocalEndpoint,
        hello: Hello,
        env: EnvSnapshot,
        catalogue: &'static CommandCatalogue,
    ) -> Self {
        Self {
            address: endpoint.address.clone(),
            pid: endpoint.pid,
            hello,
            create: true,
            instance_id: None,
            catalogue,
            env,
        }
    }

    /// A secondary view of an existing instance (`awman view`).
    pub fn secondary(
        endpoint: &LocalEndpoint,
        instance_id: InstanceId,
        hello: Hello,
        env: EnvSnapshot,
        catalogue: &'static CommandCatalogue,
    ) -> Self {
        Self {
            address: endpoint.address.clone(),
            pid: endpoint.pid,
            hello,
            create: false,
            instance_id: Some(instance_id),
            catalogue,
            env,
        }
    }
}

/// Why [`RemoteInstanceBackend::connect`] could not attach. A refusal is
/// final (the hand-off reports it and does not retry); `Unreachable` means
/// nothing answered on the endpoint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteConnectError {
    /// The daemon refused: `code` is its error code (`primary_exists`,
    /// `protocol_mismatch`, `not_found`, `feature_disabled`,
    /// `invalid_annotation`, `instance_start_failed`, `protocol_error`).
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("engine unreachable: {0}")]
    Unreachable(String),
    #[error("{0}")]
    Failed(String),
}

impl RemoteConnectError {
    /// The daemon's refusal code, when it refused.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } => Some(code),
            Self::Unreachable(_) | Self::Failed(_) => None,
        }
    }
}

impl From<RemoteConnectError> for CommandError {
    fn from(error: RemoteConnectError) -> Self {
        CommandError::Other(error.to_string())
    }
}

/// p95 latencies the parity suite asserts on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LatencyStats {
    /// `Submit` sent → the tab's first `CommandOutput`.
    pub submit_to_first_output_p95: Option<Duration>,
    /// `DialogRequest` received → its `DialogResponse` sent.
    pub dialog_round_trip_p95: Option<Duration>,
    pub submit_samples: usize,
    pub dialog_samples: usize,
}

/// The daemon-backed backend. See the module docs.
pub struct RemoteInstanceBackend {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for RemoteInstanceBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteInstanceBackend")
            .field("instance_id", &self.shared.instance_id)
            .finish_non_exhaustive()
    }
}

impl RemoteInstanceBackend {
    /// `POST /v1/engine/instances` (when `create`), connect the WebSocket,
    /// send `Hello`, await `Welcome` and the snapshot. A refusal (`409
    /// primary_exists`, `426 protocol_mismatch`, `404 not_found`, …) is
    /// [`RemoteConnectError::Refused`] (it converts into a `CommandError` with the same text).
    pub async fn connect(open: RemoteOpen) -> Result<Arc<Self>, RemoteConnectError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        spawn_driver(
            open,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        )?;
        rx.await
            .map_err(|_| RemoteConnectError::Failed("engine frontend thread ended".into()))?
    }

    /// [`Self::connect`] for a caller outside any async runtime.
    pub fn connect_blocking(open: RemoteOpen) -> Result<Arc<Self>, RemoteConnectError> {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_driver(
            open,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        )?;
        rx.recv()
            .map_err(|_| RemoteConnectError::Failed("engine frontend thread ended".into()))?
    }

    /// This frontend's id (stable across reconnects within the grace).
    pub fn frontend_id(&self) -> FrontendId {
        self.shared.lock().frontend_id.clone()
    }

    /// The role the engine admitted this frontend as.
    pub fn role(&self) -> FrontendRole {
        self.shared.role
    }

    /// The latest instance-wide snapshot facts the backend holds (tab ids in
    /// engine order). Mostly for tests and `awman view`.
    pub fn engine_tab_ids(&self) -> Vec<TabId> {
        self.shared
            .lock()
            .tabs
            .iter()
            .map(|tab| tab.tab_id.clone())
            .collect()
    }

    /// How many times the socket was re-established inside the grace.
    pub fn reconnects(&self) -> u64 {
        self.shared.reconnects.load(Ordering::Relaxed)
    }

    /// p95 of Submit→first CommandOutput and DialogRequest→DialogResponse.
    pub fn latency_probe(&self) -> LatencyStats {
        let state = self.shared.lock();
        LatencyStats {
            submit_to_first_output_p95: p95(&state.latency.submit),
            dialog_round_trip_p95: p95(&state.latency.dialog),
            submit_samples: state.latency.submit.len(),
            dialog_samples: state.latency.dialog.len(),
        }
    }

    /// Send a frame and wait for the driver's answer on `rx`.
    fn wait<T>(
        &self,
        rx: std::sync::mpsc::Receiver<Result<T, CommandError>>,
        what: &str,
    ) -> Result<T, CommandError> {
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                if let Err(loss) = self.health() {
                    return Err(CommandError::Other(loss.to_string()));
                }
                Err(CommandError::Other(format!(
                    "the engine did not answer the {what} request"
                )))
            }
        }
    }
}

impl Drop for RemoteInstanceBackend {
    fn drop(&mut self) {
        // The socket closes; a primary's instance gets the 2 s grace and
        // then today's killed-TUI policy.
        self.shared.stop.cancel();
    }
}

// ─── Shared state ───────────────────────────────────────────────────────────

/// What the backend object and its driver task share.
struct Shared {
    instance_id: InstanceId,
    pid: u32,
    role: FrontendRole,
    address: PathBuf,
    catalogue: &'static CommandCatalogue,
    env: EnvSnapshot,
    hello: Hello,
    state: Mutex<State>,
    outbound: UnboundedSender<Outbound>,
    stop: CancellationToken,
    next_ticket: AtomicU64,
    reconnects: AtomicU64,
    squad: SharedSquadHealth,
    /// The private runtime the driver runs on; hub terminal subscriptions
    /// opened on demand run here too.
    runtime: tokio::runtime::Handle,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn send(&self, frame: FrontendFrame) {
        let _ = self.outbound.send(Outbound::Frame(frame));
    }
}

/// A frame the presenter side queued for the driver.
enum Outbound {
    Frame(FrontendFrame),
    Ui {
        ticket: UiTicket,
        request: UiRequest,
    },
}

#[derive(Default)]
struct State {
    /// Pending queue requests, with the tab each was made from. An entry
    /// outlives a caller that gave up waiting, so a late answer is noticed.
    queue_waiters: HashMap<
        u64,
        (
            TabId,
            std::sync::mpsc::Sender<crate::data::worker::queue::QueueReply>,
        ),
    >,
    frontend_id: FrontendId,
    tabs: Vec<RemoteTab>,
    /// Tabs the engine has that this presenter never asked for (another
    /// frontend's or the API's); tracked so their frames are not errors.
    foreign_tabs: HashSet<TabId>,
    events: VecDeque<BackendEvent>,
    fatal: Option<BackendLoss>,
    quitting: bool,
    quit_waiters: Vec<std::sync::mpsc::Sender<()>>,
    open_waiters: VecDeque<OpenWaiter>,
    tickets: HashMap<Seq, UiTicket>,
    /// The instance-wide agent-window size last reported.
    term_size: Option<TermSize>,
    /// The first snapshot was applied (its tabs are claimable; later
    /// unknown tabs are not this presenter's).
    welcomed: bool,
    latency: Latency,
    assembler: SnapshotAssembler,
}

#[derive(Default)]
struct Latency {
    submit: Vec<Duration>,
    dialog: Vec<Duration>,
}

struct OpenWaiter {
    seed: Option<Session>,
    reply: std::sync::mpsc::Sender<Result<OpenedTab, CommandError>>,
}

/// One engine tab as this presenter shows it.
struct RemoteTab {
    tab_id: TabId,
    /// The presenter-side mirror; its id is the key the presenter uses.
    session: Session,
    kind: TabKindWire,
    /// The feeds the presenter holds for the tab now (from `OpenedTab` or the
    /// last `SubmitRequest`).
    feeds: TabCommandFeeds,
    run: Option<RemoteRun>,
    /// A submission the engine has not acknowledged yet.
    pending: Option<PendingSubmit>,
    /// Startup run parts the presenter has not claimed (`open_initial`).
    unclaimed: Option<(TabCommandFeeds, StartupRun)>,
    status_lines: usize,
    in_group: bool,
    yolo_mode: bool,
    closing: bool,
    pty_size: Option<(u16, u16)>,
    last_input_seen: Option<Instant>,
    last_activity_sent: Option<Instant>,
    last_activity_step: Option<String>,
}

/// One command's producer ends.
struct RemoteRun {
    feeds: TabCommandFeeds,
    dialog_request_tx: std::sync::mpsc::Sender<DialogRequest>,
    dialog_response_rx: std::sync::mpsc::Receiver<DialogResponse>,
    command_result_tx: std::sync::mpsc::Sender<Result<CommandOutcome, CommandError>>,
    /// The engine dialog ids awaiting this presenter's answer, oldest first.
    dialogs: VecDeque<(DialogId, Instant)>,
    /// Dialog ids delivered (so a re-sync does not deliver one twice).
    delivered: HashSet<DialogId>,
    /// Requests raised after a close are held until the TUI receives that
    /// close, so it cannot display the next board before dismissing the old.
    deferred_dialogs: VecDeque<(DialogId, DialogRequest)>,
    /// A dialog closed elsewhere before this presenter observed the close.
    /// Until the UI polls that event, any response still on the untagged
    /// response channel belongs to the closed dialog, not its successor.
    dialog_close_barrier: bool,
    /// Pipes for the first sequential slot (the `slot_io` handed out).
    first_slot: Option<SlotPipes>,
    slots: HashMap<SlotId, RemoteSlot>,
    engine_requests: Option<UnboundedReceiver<EngineRequest>>,
    stuck: Option<tokio::sync::broadcast::Sender<StuckEvent>>,
    cancel_sent: bool,
    finished: bool,
    submitted_at: Option<Instant>,
}

struct PendingSubmit {
    run: RemoteRun,
    saw_yolo_mode: bool,
    expected_command: String,
    reply: std::sync::mpsc::Sender<Result<SubmitAck, CommandError>>,
}

struct SubmitAck {
    display: LaunchDisplay,
    sandboxed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotKind {
    Sequential,
    Step(String),
}

struct RemoteSlot {
    kind: SlotKind,
    container_name: String,
    output: Arc<Mutex<SlotOutput>>,
    yolo_flag: Option<SharedYoloCancelFlag>,
    subscription: Option<CancellationToken>,
}

struct SlotOutput {
    tx: UnboundedSender<Vec<u8>>,
    active: bool,
}

impl SlotOutput {
    fn send_live(&self, bytes: Vec<u8>) -> bool {
        !self.active || self.tx.send(bytes).is_ok()
    }
}

/// The producer ends of one slot's `ContainerSlotIo`.
struct SlotPipes {
    stdout_tx: UnboundedSender<Vec<u8>>,
    stdin_rx: UnboundedReceiver<Vec<u8>>,
    resize_rx: UnboundedReceiver<(u16, u16)>,
}

fn slot_channels() -> (ContainerSlotIo, SlotPipes) {
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel();
    (
        ContainerSlotIo {
            stdout_rx,
            stdin_tx,
            resize_tx,
        },
        SlotPipes {
            stdout_tx,
            stdin_rx,
            resize_rx,
        },
    )
}

/// A fresh run over `feeds`, and the consumer ends the presenter keeps.
fn new_run(feeds: TabCommandFeeds) -> (RemoteRun, TabFeeds, ContainerSlotIo) {
    let (dialog_request_tx, dialog_request_rx) = std::sync::mpsc::channel();
    let (dialog_response_tx, dialog_response_rx) = std::sync::mpsc::channel();
    let (command_result_tx, command_result_rx) = std::sync::mpsc::channel();
    let (slot_io, pipes) = slot_channels();
    (
        RemoteRun {
            feeds,
            dialog_request_tx,
            dialog_response_rx,
            command_result_tx,
            dialogs: VecDeque::new(),
            delivered: HashSet::new(),
            deferred_dialogs: VecDeque::new(),
            dialog_close_barrier: false,
            first_slot: Some(pipes),
            slots: HashMap::new(),
            engine_requests: None,
            stuck: None,
            cancel_sent: false,
            finished: false,
            submitted_at: None,
        },
        TabFeeds {
            command_result_rx,
            dialog_request_rx,
            dialog_response_tx,
        },
        slot_io,
    )
}

impl RemoteRun {
    fn end_slots(&mut self) {
        for slot in self.slots.values() {
            if let Some(token) = &slot.subscription {
                token.cancel();
            }
        }
    }
}

impl Drop for RemoteRun {
    fn drop(&mut self) {
        self.end_slots();
    }
}

// ─── The trait ──────────────────────────────────────────────────────────────

impl InstanceBackend for RemoteInstanceBackend {
    fn queue_source(
        &self,
        session: &Session,
    ) -> Result<Option<crate::command::worker::queue_source::SharedQueueSource>, String> {
        if !session.effective_config().tui_worker_enabled() {
            return Ok(None);
        }
        let tab_id = self
            .tab_id(&session.id())
            .ok_or("queue requires an open tab")?;
        let source = Arc::new(RemoteQueue {
            shared: self.shared.clone(),
            tab_id,
            generation: AtomicU64::new(0),
            snapshot: Default::default(),
        });
        let weak = Arc::downgrade(&source);
        std::thread::Builder::new()
            .name("awman-queue-view".into())
            .spawn(move || {
                while let Some(source) = weak.upgrade() {
                    if source.shared.stop.is_cancelled() {
                        break;
                    }
                    source.update_snapshot();
                    drop(source);
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Some(source))
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Daemon {
            instance_id: self.shared.instance_id.clone(),
            pid: self.shared.pid,
        }
    }

    fn instance_id(&self) -> InstanceId {
        self.shared.instance_id.clone()
    }

    fn catalogue(&self) -> &'static CommandCatalogue {
        self.shared.catalogue
    }

    fn open_initial(&self, spec: InitialTabSpec) -> InitialTabOpened {
        match spec {
            InitialTabSpec::Session {
                session, pty_size, ..
            } => {
                let dir = session.working_dir().to_path_buf();
                match self.claim_initial(TabKindWire::Normal, Some(session.clone())) {
                    Some(tab) => InitialTabOpened::Normal { tab },
                    None => match self.open_tab(OpenTabRequest {
                        dir,
                        options: SessionOpenOptions::default(),
                        pty_size,
                    }) {
                        Ok(tab) => InitialTabOpened::Normal { tab },
                        Err(error) => InitialTabOpened::SquadFailed {
                            tab: Self::placeholder_tab(session),
                            error: SquadStartError::Other(error.to_string()),
                        },
                    },
                }
            }
            InitialTabSpec::Squad { fallback } => {
                if let Some(tab) = self.claim_initial(TabKindWire::Squad, None) {
                    if let Ok(Ok(startup)) = self.start_squad_gateway().recv() {
                        return InitialTabOpened::Squad {
                            session: tab.session,
                            gateway: startup.gateway,
                        };
                    }
                }
                let tab = self
                    .claim_initial(TabKindWire::Normal, Some(fallback.clone()))
                    .unwrap_or_else(|| Self::placeholder_tab(fallback));
                InitialTabOpened::SquadFailed {
                    tab,
                    error: SquadStartError::Other("the engine could not open the squad tab".into()),
                }
            }
        }
    }

    fn open_tab(&self, request: OpenTabRequest) -> Result<OpenedTab, CommandError> {
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut state = self.shared.lock();
            if let Some(loss) = &state.fatal {
                return Err(CommandError::Other(loss.to_string()));
            }
            state.open_waiters.push_back(OpenWaiter {
                reply: tx,
                seed: None,
            });
        }
        self.shared.send(FrontendFrame::OpenTab {
            dir: request.dir,
            options: OpenTabOptions {
                startup: crate::data::frontend_protocol::StartupSpawn::Ready,
                ..Default::default()
            },
        });
        self.wait(rx, "open tab")
    }

    fn adopt_session(&self, session: &Session) {
        if self.tab_id(&session.id()).is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.shared.lock().open_waiters.push_back(OpenWaiter {
            reply: tx,
            seed: Some(session.clone()),
        });
        self.shared.send(FrontendFrame::OpenTab {
            dir: session.working_dir().to_path_buf(),
            options: OpenTabOptions::default(),
        });
        if let Err(error) = self.wait(rx, "adopt session") {
            self.shared.lock().events.push_back(BackendEvent::Notice {
                level: MessageLevel::Error,
                text: error.to_string(),
            });
        }
    }

    fn session_snapshot(&self, session_id: &SessionId) -> Option<Session> {
        let state = self.shared.lock();
        state
            .tabs
            .iter()
            .find(|tab| tab.session.id() == *session_id)
            .map(|tab| tab.session.clone())
    }

    fn close_tab(&self, session_id: &SessionId) -> CloseTabOutcome {
        let mut state = self.shared.lock();
        let Some(index) = state
            .tabs
            .iter()
            .position(|tab| tab.session.id() == *session_id)
        else {
            return CloseTabOutcome::Closed;
        };
        let mut tab = state.tabs.remove(index);
        tab.closing = true;
        let tab_id = tab.tab_id.clone();
        drop(tab);
        let last = state.tabs.is_empty();
        if last {
            // The engine ends the instance when its last tab closes.
            state.quitting = true;
        }
        drop(state);
        self.shared.send(FrontendFrame::CloseTab { tab_id });
        if last {
            CloseTabOutcome::ClosedLastTab
        } else {
            CloseTabOutcome::Closed
        }
    }

    fn tab_id(&self, session_id: &SessionId) -> Option<TabId> {
        self.shared
            .lock()
            .tabs
            .iter()
            .find(|tab| tab.session.id() == *session_id)
            .map(|tab| tab.tab_id.clone())
    }

    fn focus_hint(&self, session_id: &SessionId) {
        if let Some(tab_id) = self.tab_id(session_id) {
            self.shared.send(FrontendFrame::FocusTab { tab_id });
        }
    }

    fn resize_tab(&self, session_id: &SessionId, pty_size: (u16, u16)) {
        let mut state = self.shared.lock();
        if let Some(tab) = state
            .tabs
            .iter_mut()
            .find(|tab| tab.session.id() == *session_id)
        {
            tab.pty_size = Some(pty_size);
        }
        // The engine keeps one agent-window size per frontend (D8).
        let size = TermSize {
            cols: pty_size.0,
            rows: pty_size.1,
        };
        if state.term_size != Some(size) {
            state.term_size = Some(size);
            drop(state);
            self.shared.send(FrontendFrame::Resize(size));
        }
    }

    fn admit(&self, _path: &[String]) -> Result<(), CommandError> {
        // Runtime-tier admission runs engine-side on `Submit`; a refusal
        // comes back from `submit` exactly as the builtin backend's does.
        Ok(())
    }

    fn submit(&self, request: SubmitRequest) -> Result<SubmittedCommand, CommandError> {
        let path: Vec<&str> = request.parsed.path.iter().map(String::as_str).collect();
        let spec = self
            .shared
            .catalogue
            .lookup_with_aliases(&path)
            .ok_or_else(|| CommandError::unknown_command(&path))?;
        let mut args = request.parsed.path.clone();
        args.extend(request.parsed.args.to_argv(spec));
        let pty_size = TermSize {
            cols: request.pty_size.0,
            rows: request.pty_size.1,
        };
        let (mut run, feeds, slot_io) = new_run(request.feeds.clone());
        run.submitted_at = Some(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel();
        let tab_id = {
            let mut state = self.shared.lock();
            if let Some(loss) = &state.fatal {
                return Err(CommandError::Other(loss.to_string()));
            }
            let tab = state
                .tabs
                .iter_mut()
                .find(|tab| tab.session.id() == request.session_id)
                .ok_or_else(|| CommandError::Other("no such tab".into()))?;
            tab.feeds = request.feeds;
            tab.last_input_seen = None;
            tab.last_activity_sent = None;
            tab.last_activity_step = None;
            tab.pty_size = Some(request.pty_size);
            // Record the start the way the builtin backend's session does, so
            // the tab never renders an idle frame before the first delta.
            tab.session.state_mut().begin_command(
                request.parsed.path.join(" "),
                args[request.parsed.path.len()..].to_vec(),
            );
            tab.pending = Some(PendingSubmit {
                run,
                saw_yolo_mode: false,
                expected_command: request.parsed.path.join(" "),
                reply: tx,
            });
            tab.tab_id.clone()
        };
        self.shared.send(FrontendFrame::Submit {
            tab_id,
            input: match request.lease_id {
                Some(lease_id) => SubmitInput::Lease { lease_id, args },
                None => SubmitInput::Argv { args },
            },
            pty_size,
        });
        let ack = self.wait(rx, "submit")?;
        Ok(SubmittedCommand {
            display: ack.display,
            sandboxed: ack.sandboxed,
            feeds,
            slot_io,
        })
    }

    fn running_jobs(&self) -> usize {
        self.shared
            .lock()
            .tabs
            .iter()
            .filter(|tab| tab.run.as_ref().is_some_and(|run| !run.finished))
            .count()
    }

    fn stats_sampler(&self) -> ContainerStatsSampler {
        // No local runtime to ask; the engine owns the containers.
        ContainerStatsSampler::inert()
    }

    fn take_runtime_notices(&self) -> Vec<String> {
        // Engine notices arrive as `BackendEvent::Notice`.
        Vec::new()
    }

    fn ui_request(&self, request: UiRequest) -> UiTicket {
        let ticket = UiTicket(self.shared.next_ticket.fetch_add(1, Ordering::Relaxed));
        if self.shared.lock().fatal.is_some() {
            self.shared.lock().events.push_back(BackendEvent::UiAck {
                ticket,
                outcome: UiOutcome::unclaimed(),
            });
        } else {
            let _ = self.shared.outbound.send(Outbound::Ui { ticket, request });
        }
        ticket
    }

    fn poll(&self) -> Vec<BackendEvent> {
        let mut state = self.shared.lock();
        for tab in &mut state.tabs {
            if let Some(run) = tab.run.as_mut() {
                if run.dialog_close_barrier {
                    while run.dialog_response_rx.try_recv().is_ok() {}
                    run.dialog_close_barrier = false;
                    while let Some((_, request)) = run.deferred_dialogs.pop_front() {
                        let _ = run.dialog_request_tx.send(request);
                    }
                }
            }
        }
        state.events.drain(..).collect()
    }

    fn quit(&self) -> QuitPlan {
        let plan = QuitPlan {
            detach_containers: false,
            exit_code: 0,
            cancel_running: true,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut state = self.shared.lock();
            let already = state.quitting && state.quit_waiters.is_empty();
            state.quitting = true;
            if state.fatal.is_some() || already {
                return plan;
            }
            state.quit_waiters.push(tx);
        }
        self.shared.send(FrontendFrame::Quit);
        let _ = rx.recv_timeout(QUIT_TIMEOUT);
        self.shared.stop.cancel();
        plan
    }

    fn open_host_terminal(
        &self,
        terminal_id: &TerminalId,
    ) -> Result<ContainerSlotIo, CommandError> {
        if let Some(loss) = self.shared.lock().fatal.clone() {
            return Err(CommandError::Other(loss.to_string()));
        }
        // The same 0141 subscription a slot uses: screen and output out,
        // keystrokes in after the input grant, resizes to the hub. Dropping
        // the presenter's ends (or the backend stopping) ends it.
        let (io, pipes) = slot_channels();
        let output = Arc::new(Mutex::new(SlotOutput {
            tx: pipes.stdout_tx.clone(),
            active: true,
        }));
        self.shared.runtime.spawn(subscribe_terminal(
            self.shared.address.clone(),
            terminal_id.clone(),
            pipes,
            output,
            self.shared.stop.child_token(),
        ));
        Ok(io)
    }

    fn health(&self) -> Result<(), BackendLoss> {
        let state = self.shared.lock();
        match &state.fatal {
            // A detach this presenter asked for is the normal end.
            Some(_) if state.quitting => Ok(()),
            Some(loss) => Err(loss.clone()),
            None => Ok(()),
        }
    }

    fn squad_health(&self) -> SharedSquadHealth {
        self.shared.squad.clone()
    }

    fn start_squad_indicator(
        &self,
        _cancel: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        // The engine runs its own probe and relays `SquadHealth` deltas.
        None
    }

    fn squad_admission(&self) -> Result<(), SquadStartError> {
        self.health()
            .map_err(|e| SquadStartError::Other(e.to_string()))
    }

    fn squad_enabled_in_config(&self) -> Result<bool, CommandError> {
        // The remote engine owns its config; this frontend never edits it.
        Ok(true)
    }

    fn squad_daemon_is_running(&self) -> Result<bool, CommandError> {
        self.health()
            .map(|_| true)
            .map_err(|e| CommandError::Other(e.to_string()))
    }

    fn start_squad_gateway(&self) -> std::sync::mpsc::Receiver<SquadStartupResult> {
        let (tx, rx) = std::sync::mpsc::channel();
        let endpoint = LocalEndpoint {
            address: self.shared.address.clone(),
            pid: self.shared.pid,
            sidecar: crate::data::fs::daemon_sidecar::ServerSidecar::current(
                self.shared.pid,
                crate::data::fs::daemon_sidecar::SidecarTransport {
                    kind: crate::data::fs::daemon_sidecar::SidecarTransportKind::native(),
                    address: self.shared.address.display().to_string(),
                },
            ),
        };
        let gateway = crate::command::commands::squad::client::DaemonSquadClient::new(
            crate::command::commands::daemon::client::DaemonClient::from_endpoint(endpoint),
        );
        let _ = tx.send(Ok(
            crate::command::commands::squad::supervisor::SquadStartup {
                gateway: Arc::new(gateway),
            },
        ));
        rx
    }

    fn open_squad_session(&self) -> Result<Session, SquadStartError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.shared.lock().open_waiters.push_back(OpenWaiter {
            reply: tx,
            seed: None,
        });
        self.shared.send(FrontendFrame::OpenTab {
            dir: self.shared.env.cwd().to_path_buf(),
            options: OpenTabOptions {
                kind: TabKindWire::Squad,
                ..Default::default()
            },
        });
        self.wait(rx, "open squad tab")
            .map(|tab| tab.session)
            .map_err(|e| SquadStartError::Other(e.to_string()))
    }

    fn set_squad_gateway(&self, _gateway: Arc<dyn TaskGateway>) {}
}

impl RemoteInstanceBackend {
    /// Hand the presenter the first engine tab registered from the welcome
    /// snapshot, adopting `session` as its mirror when given.
    fn claim_initial(&self, kind: TabKindWire, session: Option<Session>) -> Option<OpenedTab> {
        let mut state = self.shared.lock();
        let tab = state
            .tabs
            .iter_mut()
            .find(|tab| tab.unclaimed.is_some() && tab.kind == kind)?;
        let (feeds, startup) = tab.unclaimed.take()?;
        if let Some(session) = session {
            // Keep the presenter's own session (its flags and config) as the
            // mirror, with the engine's view of what runs in it.
            let state = tab.session.state().clone();
            tab.session = session;
            *tab.session.state_mut() = state;
        }
        Some(OpenedTab {
            session: tab.session.clone(),
            git_summary: GitSummaryPoller::default(),
            feeds,
            startup,
        })
    }

    /// A tab view over the presenter's own session with no engine tab behind
    /// it: what the presenter shows next to the error when the engine had no
    /// tab to offer (the squad fallback, a refused first tab).
    fn placeholder_tab(session: Session) -> OpenedTab {
        OpenedTab {
            session,
            git_summary: GitSummaryPoller::default(),
            feeds: TabCommandFeeds::new(),
            startup: StartupRun::NotStarted,
        }
    }
}

// ─── Driver ─────────────────────────────────────────────────────────────────

type ConnectReply = Box<dyn FnOnce(Result<Arc<RemoteInstanceBackend>, RemoteConnectError>) + Send>;

/// The live socket halves and what the handshake learned.
struct Connection {
    sender: FrontendSender,
    receiver: FrontendReceiver,
    frontend_id: FrontendId,
    snapshot: InstanceSnapshot,
    /// Frames that arrived after the snapshot during the handshake.
    early: Vec<EngineFrame>,
}

/// Start the backend's private runtime thread; `reply` receives the backend
/// once the handshake finished (or why it failed).
fn spawn_driver(open: RemoteOpen, reply: ConnectReply) -> Result<(), RemoteConnectError> {
    std::thread::Builder::new()
        .name("awman-engine-frontend".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("awman-engine-frontend")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    reply(Err(RemoteConnectError::Failed(format!(
                        "engine frontend runtime: {error}"
                    ))));
                    return;
                }
            };
            runtime.block_on(async move {
                let instance_id = match open.instance_id.clone() {
                    Some(id) if !open.create => id,
                    _ => match create_instance(&open).await {
                        Ok(id) => id,
                        Err(error) => {
                            reply(Err(error));
                            return;
                        }
                    },
                };
                let connection = match tokio::time::timeout(
                    WELCOME_TIMEOUT,
                    handshake(&open.address, &instance_id, &open.hello),
                )
                .await
                {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => {
                        reply(Err(stream_error_to_connect(error)));
                        return;
                    }
                    Err(_) => {
                        reply(Err(RemoteConnectError::Failed(
                            "the engine did not welcome this frontend in time".into(),
                        )));
                        return;
                    }
                };
                let (outbound, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
                let shared = Arc::new(Shared {
                    instance_id,
                    pid: open.pid,
                    role: open.hello.role,
                    address: open.address,
                    catalogue: open.catalogue,
                    env: open.env,
                    hello: open.hello,
                    state: Mutex::new(State::default()),
                    outbound,
                    stop: CancellationToken::new(),
                    next_ticket: AtomicU64::new(1),
                    reconnects: AtomicU64::new(0),
                    squad: Arc::new(Mutex::new(SquadHealth::Unknown)),
                    runtime: tokio::runtime::Handle::current(),
                });
                let Connection {
                    sender,
                    receiver,
                    frontend_id,
                    snapshot,
                    early,
                } = connection;
                {
                    let mut state = shared.lock();
                    state.frontend_id = frontend_id;
                    resync(&shared, &mut state, snapshot);
                    for frame in early {
                        handle_frame(&shared, &mut state, 0, frame);
                    }
                }
                let backend = Arc::new(RemoteInstanceBackend {
                    shared: Arc::clone(&shared),
                });
                reply(Ok(backend));
                drive(shared, sender, receiver, outbound_rx).await;
            });
        })
        .map(|_| ())
        .map_err(|error| RemoteConnectError::Failed(format!("engine frontend thread: {error}")))
}

/// `POST /v1/engine/instances` with the frontend's annotations.
async fn create_instance(open: &RemoteOpen) -> Result<InstanceId, RemoteConnectError> {
    let client = LocalClient::new(open.address.clone());
    let body = serde_json::json!({ "annotations": open.hello.annotations });
    let response = client
        .send("POST", "/v1/engine/instances", Some(&body))
        .await
        .map_err(|error| RemoteConnectError::Unreachable(error.to_string()))?;
    if response.status >= 400 {
        let code = response
            .body
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or("refused")
            .to_string();
        let message = response
            .body
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "the engine refused a new instance: HTTP {}",
                    response.status
                )
            });
        return Err(RemoteConnectError::Refused { code, message });
    }
    response
        .body
        .get("id")
        .and_then(|id| id.as_str())
        .and_then(|id| id.parse::<InstanceId>().ok())
        .ok_or_else(|| {
            RemoteConnectError::Failed("the engine answered without an instance id".into())
        })
}

/// Connect, say `Hello`, and read up to the end of the first snapshot.
async fn handshake(
    address: &Path,
    instance: &InstanceId,
    hello: &Hello,
) -> Result<Connection, FrontendStreamError> {
    let (mut sender, mut receiver) = FrontendStream::connect_local(address, instance)
        .await?
        .split();
    sender.send(&FrontendFrame::Hello(hello.clone())).await?;
    let mut frontend_id = None;
    let mut assembler = SnapshotAssembler::default();
    loop {
        let Some((_, frame)) = receiver.next().await? else {
            return Err(FrontendStreamError::Transport(
                "the engine closed the socket during the handshake".into(),
            ));
        };
        match frame {
            EngineFrame::Welcome {
                frontend_id: id, ..
            } => frontend_id = Some(id),
            EngineFrame::Snapshot(snapshot) if frontend_id.is_some() => {
                return Ok(Connection {
                    sender,
                    receiver,
                    frontend_id: frontend_id.unwrap_or_default(),
                    snapshot,
                    early: Vec::new(),
                });
            }
            EngineFrame::SnapshotChunk { chunk, of, bytes } if frontend_id.is_some() => {
                if let Some(snapshot) = assembler.push(chunk, of, &bytes)? {
                    return Ok(Connection {
                        sender,
                        receiver,
                        frontend_id: frontend_id.unwrap_or_default(),
                        snapshot,
                        early: Vec::new(),
                    });
                }
            }
            EngineFrame::Detach(reason) => {
                return Err(FrontendStreamError::Refused {
                    status: None,
                    code: "detached".into(),
                    message: format!("engine detached: {}", reason.as_str()),
                })
            }
            other => {
                return Err(ProtocolError::Malformed(format!(
                    "expected welcome and snapshot, got {}",
                    frame_name(&other)
                ))
                .into())
            }
        }
    }
}

fn stream_error_to_connect(error: FrontendStreamError) -> RemoteConnectError {
    match error {
        FrontendStreamError::Refused { code, message, .. } => {
            RemoteConnectError::Refused { code, message }
        }
        FrontendStreamError::Protocol(error) => RemoteConnectError::Refused {
            code: "protocol_error".into(),
            message: error.to_string(),
        },
        FrontendStreamError::Transport(message) => RemoteConnectError::Unreachable(message),
    }
}

/// The socket loop: engine frames in, queued presenter frames out, local
/// inputs polled; reconnects inside the grace; ends on a fatal loss or stop.
async fn drive(
    shared: Arc<Shared>,
    mut sender: FrontendSender,
    mut receiver: FrontendReceiver,
    mut outbound: UnboundedReceiver<Outbound>,
) {
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Inputs are consumed from local flags before sending. Keep every frame
    // not sent when a socket fails, in order, so no cancel/request disappears.
    let mut retry: VecDeque<Outbound> = VecDeque::new();
    loop {
        let lost: Option<String> = if let Some(item) = retry.pop_front() {
            match send_outbound(&shared, &mut sender, &item).await {
                Ok(()) => continue,
                Err(FrontendStreamError::Transport(message)) => {
                    retry.push_front(item);
                    Some(message)
                }
                Err(error) => {
                    fail(&shared, loss_for(&error));
                    sender.close().await;
                    return;
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = shared.stop.cancelled() => {
                    sender.close().await;
                    return;
                }
                next = receiver.next() => match next {
                    Ok(Some((seq, frame))) => {
                        let fatal = {
                            let mut state = shared.lock();
                            handle_frame(&shared, &mut state, seq, frame);
                            state.fatal.is_some()
                        };
                        if fatal {
                            sender.close().await;
                            return;
                        }
                        None
                    }
                    Ok(None) => Some("the engine closed the socket".into()),
                    Err(FrontendStreamError::Transport(message)) => Some(message),
                    Err(error) => {
                        fail(&shared, loss_for(&error));
                        sender.close().await;
                        return;
                    }
                },
                item = outbound.recv() => match item {
                    Some(item) => match send_outbound(&shared, &mut sender, &item).await {
                        Ok(()) => None,
                        Err(FrontendStreamError::Transport(message)) => {
                            retry.push_back(item);
                            Some(message)
                        }
                        Err(error) => {
                            fail(&shared, loss_for(&error));
                            sender.close().await;
                            return;
                        }
                    },
                    None => None,
                },
                _ = tick.tick() => {
                    let mut frames: VecDeque<_> = collect_inputs(&shared).into();
                    let mut lost = None;
                    while let Some(frame) = frames.pop_front() {
                        if let Err(error) = send_outbound(&shared, &mut sender, &frame).await {
                            match error {
                                FrontendStreamError::Transport(message) => {
                                    retry.push_back(frame);
                                    retry.extend(frames);
                                    lost = Some(message);
                                    break;
                                }
                                other => {
                                    fail(&shared, loss_for(&other));
                                    sender.close().await;
                                    return;
                                }
                            }
                        }
                    }
                    lost
                }
            }
        };
        let Some(reason) = lost else {
            continue;
        };
        tracing::debug!(instance = %shared.instance_id, "engine frontend socket lost: {reason}");
        match reconnect(&shared).await {
            Ok(connection) => {
                shared.reconnects.fetch_add(1, Ordering::Relaxed);
                sender = connection.sender;
                receiver = connection.receiver;
                let fatal = {
                    let mut state = shared.lock();
                    state.frontend_id = connection.frontend_id;
                    resync(&shared, &mut state, connection.snapshot);
                    for frame in connection.early {
                        handle_frame(&shared, &mut state, 0, frame);
                    }
                    retry.retain(|item| match item {
                        Outbound::Frame(FrontendFrame::Submit { tab_id, .. }) => state
                            .tabs
                            .iter()
                            .find(|tab| &tab.tab_id == tab_id)
                            .is_some_and(|tab| tab.pending.is_some()),
                        _ => true,
                    });
                    state.fatal.is_some()
                };
                if fatal {
                    sender.close().await;
                    return;
                }
            }
            Err(loss) => {
                fail(&shared, loss);
                return;
            }
        }
    }
}

async fn send_outbound(
    shared: &Shared,
    sender: &mut FrontendSender,
    item: &Outbound,
) -> Result<(), FrontendStreamError> {
    match item {
        Outbound::Frame(frame) => {
            let submit_tab = match frame {
                FrontendFrame::Submit { tab_id, .. } => Some(tab_id.clone()),
                _ => None,
            };
            sender.send(frame).await?;
            if let Some(tab_id) = submit_tab {
                let mut state = shared.lock();
                if let Some(pending) = state
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.tab_id == tab_id)
                    .and_then(|tab| tab.pending.as_mut())
                {
                    pending.run.submitted_at = Some(Instant::now());
                }
            }
        }
        Outbound::Ui { ticket, request } => {
            let seq = sender.next_seq();
            shared.lock().tickets.insert(seq, *ticket);
            sender
                .send(&FrontendFrame::UiRequest(request.clone()))
                .await?;
        }
    }
    Ok(())
}

/// Re-establish the socket within the grace (50 ms → 1 s backoff).
async fn reconnect(shared: &Shared) -> Result<Connection, BackendLoss> {
    let deadline = Instant::now() + PRIMARY_RECONNECT_GRACE;
    let mut delay = RECONNECT_BACKOFF_INITIAL;
    let mut last = String::from("the engine closed the socket");
    loop {
        let now = Instant::now();
        if now + delay > deadline {
            return Err(BackendLoss::Transport(last));
        }
        tokio::select! {
            _ = shared.stop.cancelled() => return Err(BackendLoss::Transport("stopped".into())),
            _ = tokio::time::sleep(delay) => {}
        }
        let mut hello = shared.hello.clone();
        if shared.role == FrontendRole::Primary {
            hello.resume = Some(shared.lock().frontend_id.clone());
        }
        if let Some(size) = shared.lock().term_size {
            hello.term_size = size;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(
            remaining.max(Duration::from_millis(1)),
            handshake(&shared.address, &shared.instance_id, &hello),
        )
        .await
        {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(FrontendStreamError::Transport(message))) => last = message,
            Ok(Err(error)) => return Err(loss_for(&error)),
            Err(_) => return Err(BackendLoss::Transport(last)),
        }
        delay = (delay * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

fn loss_for(error: &FrontendStreamError) -> BackendLoss {
    match error {
        FrontendStreamError::Protocol(error) => {
            BackendLoss::Detached(DetachReasonWire::ProtocolError {
                detail: error.to_string(),
            })
        }
        FrontendStreamError::Refused { code, .. } if code == "not_found" => {
            BackendLoss::Detached(DetachReasonWire::InstanceEnded)
        }
        FrontendStreamError::Refused { message, .. } => BackendLoss::Transport(message.clone()),
        FrontendStreamError::Transport(message) => BackendLoss::Transport(message.clone()),
    }
}

/// Record a fatal loss once; wake every waiter.
fn fail(shared: &Shared, loss: BackendLoss) {
    let mut state = shared.lock();
    if state.fatal.is_none() {
        tracing::info!(instance = %shared.instance_id, "engine frontend lost: {loss}");
        state.fatal = Some(loss.clone());
    }
    wake_all(&mut state, &loss);
    shared.stop.cancel();
}

fn wake_all(state: &mut State, loss: &BackendLoss) {
    for waiter in state.open_waiters.drain(..) {
        let _ = waiter
            .reply
            .send(Err(CommandError::Other(loss.to_string())));
    }
    for tab in &mut state.tabs {
        if let Some(pending) = tab.pending.take() {
            let _ = pending
                .reply
                .send(Err(CommandError::Other(loss.to_string())));
        }
    }
    for waiter in state.quit_waiters.drain(..) {
        let _ = waiter.send(());
    }
}

// ─── Presenter-side inputs ──────────────────────────────────────────────────

/// Read every presenter input that does not travel on a channel the driver
/// can await: dialog answers, cancel tokens, yolo flags, engine requests and
/// activity.
fn collect_inputs(shared: &Shared) -> Vec<Outbound> {
    let mut frames = Vec::new();
    let mut state = shared.lock();
    let State { tabs, latency, .. } = &mut *state;
    for tab in tabs.iter_mut() {
        let tab_id = tab.tab_id.clone();
        if let Some(run) = tab.run.as_mut() {
            while let Ok(response) = run.dialog_response_rx.try_recv() {
                if run.dialog_close_barrier {
                    continue;
                }
                // The command thread answers one dialog at a time, oldest first.
                if let Some((dialog_id, raised)) = run.dialogs.pop_front() {
                    latency.dialog.push(raised.elapsed());
                    frames.push(Outbound::Frame(FrontendFrame::DialogResponse {
                        dialog_id,
                        response,
                    }));
                }
            }
            if !run.finished && !run.cancel_sent && run.feeds.command_cancel.is_cancelled() {
                run.cancel_sent = true;
                frames.push(Outbound::Frame(FrontendFrame::Cancel {
                    tab_id: tab_id.clone(),
                    repeat: false,
                }));
            }
            if run.feeds.yolo_cancel_flag.swap(false, Ordering::SeqCst) {
                frames.push(Outbound::Frame(FrontendFrame::CancelCountdown {
                    tab_id: tab_id.clone(),
                    step_name: None,
                }));
            }
            for (slot, step) in run.slots.values().filter_map(|slot| match &slot.kind {
                SlotKind::Step(step) => Some((slot, step)),
                SlotKind::Sequential => None,
            }) {
                if slot
                    .yolo_flag
                    .as_ref()
                    .is_some_and(|flag| flag.swap(false, Ordering::SeqCst))
                {
                    frames.push(Outbound::Frame(FrontendFrame::CancelCountdown {
                        tab_id: tab_id.clone(),
                        step_name: Some(step.clone()),
                    }));
                }
            }
            if let Some(requests) = run.engine_requests.as_mut() {
                while let Ok(request) = requests.try_recv() {
                    if let EngineRequest::OpenControlBoard { step_name } = request {
                        frames.push(Outbound::Frame(FrontendFrame::WorkflowRequest {
                            tab_id: tab_id.clone(),
                            request: WorkflowRequestWire::OpenControlBoard { step_name },
                        }));
                    }
                    // Stuck/unstuck reports are the engine's own detector's
                    // in this backend; the presenter's are not relayed.
                }
            }
        }
        let touched = tab.feeds.signals.last_user_input();
        let step_name = tab.feeds.signals.last_user_step();
        if touched.is_some() && touched != tab.last_input_seen {
            tab.last_input_seen = touched;
            if step_name != tab.last_activity_step
                || tab
                    .last_activity_sent
                    .is_none_or(|sent| sent.elapsed() >= ACTIVITY_INTERVAL)
            {
                tab.last_activity_sent = Some(Instant::now());
                tab.last_activity_step = step_name.clone();
                frames.push(Outbound::Frame(FrontendFrame::Activity {
                    tab_id,
                    step_name,
                }));
            }
        }
    }
    frames
}

// ─── Engine frames ──────────────────────────────────────────────────────────

fn handle_frame(shared: &Shared, state: &mut State, seq: Seq, frame: EngineFrame) {
    let _ = seq;
    match frame {
        EngineFrame::WorkerQueue { request_id, reply } => {
            if let Some((tab_id, waiter)) = state.queue_waiters.remove(&request_id) {
                use crate::data::worker::queue::{LeaseCloseChoice, QueueReply, QueueRequest};
                // The caller stopped waiting (a slow clone): nobody will run
                // this lease, so hand the item back instead of holding it.
                if let Err(std::sync::mpsc::SendError(QueueReply::Picked(run))) = waiter.send(reply)
                {
                    shared.send(FrontendFrame::WorkerQueue {
                        request_id: shared.next_ticket.fetch_add(1, Ordering::Relaxed),
                        tab_id,
                        request: QueueRequest::Finish {
                            lease_id: run.lease_id,
                            choice: LeaseCloseChoice::Release,
                        },
                    });
                }
            }
        }
        EngineFrame::Welcome { .. } => {}
        EngineFrame::Snapshot(snapshot) => resync(shared, state, snapshot),
        EngineFrame::SnapshotChunk { chunk, of, bytes } => {
            match state.assembler.push(chunk, of, &bytes) {
                Ok(Some(snapshot)) => resync(shared, state, snapshot),
                Ok(None) => {}
                Err(error) => {
                    let loss = BackendLoss::Detached(DetachReasonWire::ProtocolError {
                        detail: error.to_string(),
                    });
                    state.fatal = Some(loss.clone());
                    wake_all(state, &loss);
                }
            }
        }
        EngineFrame::Delta(delta) => apply_delta(shared, state, delta),
        EngineFrame::CommandOutput { tab_id, .. } => {
            let State { tabs, latency, .. } = state;
            if let Some(run) = tabs
                .iter_mut()
                .find(|tab| tab.tab_id == tab_id)
                .and_then(|tab| tab.run.as_mut())
            {
                if let Some(sent) = run.submitted_at.take() {
                    latency.submit.push(sent.elapsed());
                }
            }
        }
        EngineFrame::CommandBytes { .. } => {}
        EngineFrame::CommandFinished { tab_id, result } => {
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.tab_id == tab_id) else {
                return;
            };
            // A finished frame before the submission's acknowledgement is the
            // submission's refusal (the engine answers it synchronously).
            if let Some(pending) = tab.pending.take() {
                if !pending.saw_yolo_mode {
                    let error = result_from_wire(&result)
                        .err()
                        .unwrap_or_else(|| CommandError::Other("the command did not start".into()));
                    // The presenter's tab shows the refusal; the session
                    // mirror forgets the start it recorded.
                    tab.session.state_mut().current_command = None;
                    let _ = pending.reply.send(Err(error));
                    return;
                }
                tab.pending = Some(pending);
            }
            if let Some(run) = tab.run.as_mut() {
                if !run.finished {
                    run.finished = true;
                    let _ = run.command_result_tx.send(result_from_wire(&result));
                }
            }
        }
        EngineFrame::DialogRequest {
            dialog_id,
            tab_id,
            request,
        } => {
            let Some(run) = state
                .tabs
                .iter_mut()
                .find(|tab| tab.tab_id == tab_id)
                .and_then(|tab| tab.run.as_mut())
            else {
                return;
            };
            deliver_dialog(run, dialog_id, request);
        }
        EngineFrame::DialogClosed {
            dialog_id,
            answered_by,
        } => close_dialog(state, &dialog_id, answered_by),
        EngineFrame::ProgramStatus {
            tab_id,
            slot_id,
            status,
        } => {
            if let Some(tab) = state.tabs.iter_mut().find(|tab| tab.tab_id == tab_id) {
                report_program_status(tab, &slot_id, status);
            }
        }
        EngineFrame::Notice { level, text } => {
            // An operation failure answers the frontend that asked; an open
            // waiting for its tab is the one operation answered this way.
            if level == MessageLevel::Error {
                if let Some(waiter) = state.open_waiters.pop_front() {
                    let _ = waiter.reply.send(Err(CommandError::Other(text)));
                    return;
                }
            }
            state.events.push_back(BackendEvent::Notice { level, text });
        }
        EngineFrame::UiAck {
            request_seq,
            outcome,
        } => {
            if let Some(ticket) = state.tickets.remove(&request_seq) {
                state
                    .events
                    .push_back(BackendEvent::UiAck { ticket, outcome });
            }
        }
        EngineFrame::Detach(reason) => {
            let loss = BackendLoss::Detached(reason.clone());
            state.fatal = Some(loss.clone());
            if !state.quitting {
                state.events.push_back(BackendEvent::Detached(reason));
            }
            wake_all(state, &loss);
            shared.stop.cancel();
        }
    }
}

fn deliver_dialog(run: &mut RemoteRun, dialog_id: DialogId, request: DialogRequest) {
    if !run.delivered.insert(dialog_id.clone()) {
        return;
    }
    if request.awaits_answer() {
        run.dialogs.push_back((dialog_id.clone(), Instant::now()));
    }
    if run.dialog_close_barrier {
        run.deferred_dialogs.push_back((dialog_id, request));
    } else {
        let _ = run.dialog_request_tx.send(request);
    }
}

fn close_dialog(state: &mut State, dialog_id: &DialogId, answered_by: AnsweredBy) {
    for tab in &mut state.tabs {
        let Some(run) = tab.run.as_mut() else {
            continue;
        };
        if let Some(index) = run.dialogs.iter().position(|(id, _)| id == dialog_id) {
            run.dialogs.remove(index);
            run.deferred_dialogs.retain(|(id, _)| id != dialog_id);
            run.dialog_close_barrier = true;
            state.events.push_back(BackendEvent::DialogClosed {
                dialog_id: Some(dialog_id.clone()),
                tab_id: tab.tab_id.clone(),
                answered_by,
            });
            return;
        }
    }
}

/// The wire result as the presenter's `command_result_rx` type. The outcome
/// body stays opaque to the TUI (which renders feeds); the exit code and
/// error text must still match the local command result.
fn result_from_wire(result: &CommandResultWire) -> Result<CommandOutcome, CommandError> {
    match &result.error {
        Some(error) => Err(CommandError::Other(error.message.clone())),
        None if result.exit_code == 0 && result.outcome.is_none() => Ok(CommandOutcome::Empty),
        None => Ok(CommandOutcome::Relayed(result.clone())),
    }
}

fn apply_delta(shared: &Shared, state: &mut State, delta: InstanceDelta) {
    match delta {
        InstanceDelta::TabOpened(snapshot) => {
            if state.tabs.iter().any(|tab| tab.tab_id == snapshot.tab_id) {
                return;
            }
            if let Some(waiter) = state.open_waiters.pop_front() {
                let opened = register_tab(shared, state, &snapshot, waiter.seed).map(
                    |(session, feeds, startup)| OpenedTab {
                        session,
                        git_summary: GitSummaryPoller::default(),
                        feeds,
                        startup,
                    },
                );
                let _ = waiter.reply.send(opened);
            } else {
                state.foreign_tabs.insert(snapshot.tab_id);
            }
        }
        InstanceDelta::TabClosed { tab_id } => {
            state.foreign_tabs.remove(&tab_id);
            if let Some(index) = state.tabs.iter().position(|tab| tab.tab_id == tab_id) {
                let tab = state.tabs.remove(index);
                if !tab.closing {
                    state.events.push_back(BackendEvent::TabClosedByEngine {
                        tab_id,
                        reason: "the tab was closed by another frontend".into(),
                    });
                }
            }
        }
        InstanceDelta::Annotations {
            target,
            annotations,
        } => state.events.push_back(BackendEvent::Annotations {
            target,
            annotations,
        }),
        InstanceDelta::FrontendAttached(summary) => {
            if summary.frontend_id != state.frontend_id {
                state
                    .events
                    .push_back(BackendEvent::FrontendAttached(summary));
            }
        }
        InstanceDelta::FrontendDetached { frontend_id, .. } => state
            .events
            .push_back(BackendEvent::FrontendDetached { frontend_id }),
        InstanceDelta::SquadHealth(health) => set_squad_health(shared, &health),
        // Each frontend keeps its own focus; labels and git summaries are
        // computed presenter-side as for the builtin backend; pending dialogs
        // arrive as `DialogRequest` frames.
        InstanceDelta::ActiveTabHint { .. }
        | InstanceDelta::DialogPending(_)
        | InstanceDelta::DialogResolved { .. } => {}
        // The presenter has no container runtime; the engine samples stats.
        InstanceDelta::SlotStats {
            tab_id,
            slot_id,
            stats,
        } => {
            let slot = state
                .tabs
                .iter_mut()
                .find(|tab| tab.tab_id == tab_id)
                .and_then(|tab| presenter_slot(tab, &slot_id));
            if let Some(slot) = slot {
                state.events.push_back(BackendEvent::SlotStats {
                    tab_id,
                    slot,
                    stats: AgentStats {
                        name: stats.name,
                        cpu_percent: stats.cpu_percent,
                        memory_mb: stats.memory_mb,
                    },
                });
            }
        }
        other => {
            let Some(tab_id) = delta_tab(&other) else {
                return;
            };
            let address = shared.address.clone();
            if let Some(tab) = state.tabs.iter_mut().find(|tab| &tab.tab_id == tab_id) {
                apply_tab_delta(&address, tab, other);
            }
        }
    }
}

fn set_squad_health(shared: &Shared, health: &SquadHealthWire) {
    if let Ok(mut current) = shared.squad.lock() {
        *current = SquadHealth::from(*health);
    }
}

fn delta_tab(delta: &InstanceDelta) -> Option<&TabId> {
    match delta {
        InstanceDelta::TabLabel { tab_id, .. }
        | InstanceDelta::Phase { tab_id, .. }
        | InstanceDelta::Workflow { tab_id, .. }
        | InstanceDelta::SlotLaunched { tab_id, .. }
        | InstanceDelta::SlotContainerName { tab_id, .. }
        | InstanceDelta::SlotExited { tab_id, .. }
        | InstanceDelta::SlotStuck { tab_id, .. }
        | InstanceDelta::SlotStats { tab_id, .. }
        | InstanceDelta::SlotCountdownKind { tab_id, .. }
        | InstanceDelta::SlotYolo { tab_id, .. }
        | InstanceDelta::SlotAcpUpdate { tab_id, .. }
        | InstanceDelta::SlotAcpPermission { tab_id, .. }
        | InstanceDelta::GroupStarted { tab_id }
        | InstanceDelta::GroupFinished { tab_id }
        | InstanceDelta::FocusedSlot { tab_id, .. }
        | InstanceDelta::Yolo { tab_id, .. }
        | InstanceDelta::YoloMode { tab_id, .. }
        | InstanceDelta::Stuck { tab_id, .. }
        | InstanceDelta::GitSummary { tab_id, .. }
        | InstanceDelta::StatusLine { tab_id, .. }
        | InstanceDelta::StatusLogReset { tab_id }
        | InstanceDelta::AgentsDashboard { tab_id, .. }
        | InstanceDelta::LastContainerSummary { tab_id, .. } => Some(tab_id),
        InstanceDelta::TabOpened(_)
        | InstanceDelta::TabClosed { .. }
        | InstanceDelta::Annotations { .. }
        | InstanceDelta::FrontendAttached(_)
        | InstanceDelta::FrontendDetached { .. }
        | InstanceDelta::SquadHealth(_)
        | InstanceDelta::ActiveTabHint { .. }
        | InstanceDelta::DialogPending(_)
        | InstanceDelta::DialogResolved { .. } => None,
    }
}

/// Apply one tab-scoped delta to the presenter's feeds.
fn apply_tab_delta(address: &Path, tab: &mut RemoteTab, delta: InstanceDelta) {
    match delta {
        InstanceDelta::Phase {
            phase,
            current_workflow,
            current_container,
            ..
        } => {
            let state = tab.session.state_mut();
            state.current_command = invocation_for(&phase, state.current_command.take());
            state.current_workflow = current_workflow;
            state.current_container = current_container;
        }
        InstanceDelta::YoloMode { on, .. } => {
            tab.yolo_mode = on;
            if let Some(pending) = tab.pending.as_mut() {
                pending.saw_yolo_mode = true;
            }
        }
        InstanceDelta::SlotLaunched { slot, .. } => {
            // The first slot after a submission's acknowledgement is the
            // command's own (bound) slot: the submission is accepted.
            if tab.pending.as_ref().is_some_and(|p| p.saw_yolo_mode) {
                if let Some(pending) = tab.pending.take() {
                    let ack = SubmitAck {
                        display: LaunchDisplay {
                            agent_display_name: slot.agent_display_name.clone(),
                            unattended: tab.yolo_mode,
                        },
                        sandboxed: slot.sandboxed,
                    };
                    tab.run = Some(pending.run);
                    tab.status_lines = 0;
                    tab.in_group = false;
                    let _ = pending.reply.send(Ok(ack));
                }
            }
            let in_group = tab.in_group;
            if let Some(run) = tab.run.as_mut() {
                launch_slot(address, run, &slot, in_group);
            }
        }
        InstanceDelta::SlotContainerName {
            slot_id,
            container_name,
            ..
        } => with_slot(tab, &slot_id, |run, kind| {
            if let Some(slot) = run.slots.get_mut(&slot_id) {
                slot.container_name = container_name.clone();
            }
            match kind {
                SlotKind::Sequential => run.feeds.container.running(container_name),
                SlotKind::Step(step_name) => {
                    run.feeds.container.push(ContainerSlotEvent::ContainerName {
                        step_name,
                        container_name,
                    })
                }
            }
        }),
        InstanceDelta::SlotExited {
            slot_id,
            exit_code,
            final_screen,
            ..
        } => {
            let Some(run) = tab.run.as_mut() else {
                return;
            };
            let Some(slot) = run.slots.remove(&slot_id) else {
                return;
            };
            {
                let mut output = slot
                    .output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                output.active = false;
                if let Some(screen) = final_screen {
                    let _ = output.tx.send(screen);
                }
            }
            if let Some(token) = &slot.subscription {
                // Output already delivered stays; the stream ends.
                token.cancel();
            }
            match slot.kind {
                // An exit without a code is the engine replacing or ending
                // the slot, which the presenter does itself.
                SlotKind::Sequential => {
                    if let Some(code) = exit_code {
                        run.feeds.container.exited(code);
                    }
                }
                SlotKind::Step(step_name) => run
                    .feeds
                    .container
                    .push(ContainerSlotEvent::Exited { step_name }),
            }
        }
        InstanceDelta::SlotStuck { slot_id, stuck, .. } => with_slot(tab, &slot_id, |run, kind| {
            if let SlotKind::Step(step_name) = kind {
                run.feeds.container.push(if stuck {
                    ContainerSlotEvent::Stuck { step_name }
                } else {
                    ContainerSlotEvent::Unstuck { step_name }
                });
            }
        }),
        InstanceDelta::SlotCountdownKind { slot_id, kind, .. } => {
            with_slot(tab, &slot_id, |run, slot_kind| {
                if let SlotKind::Step(step_name) = slot_kind {
                    run.feeds
                        .container
                        .push(ContainerSlotEvent::CountdownKind { step_name, kind });
                }
            })
        }
        InstanceDelta::SlotYolo { slot_id, state, .. } => {
            let Some(run) = tab.run.as_mut() else {
                return;
            };
            let Some(slot) = run.slots.get_mut(&slot_id) else {
                return;
            };
            let SlotKind::Step(step_name) = slot.kind.clone() else {
                return;
            };
            match state {
                Some(yolo) => {
                    if slot.yolo_flag.is_none() {
                        let flag: SharedYoloCancelFlag = Default::default();
                        slot.yolo_flag = Some(flag.clone());
                        run.feeds.container.push(ContainerSlotEvent::YoloStarted {
                            step_name: step_name.clone(),
                            cancel_flag: flag,
                        });
                    }
                    run.feeds.container.push(ContainerSlotEvent::YoloTick {
                        step_name,
                        remaining_secs: yolo.remaining_secs,
                    });
                }
                None => {
                    slot.yolo_flag = None;
                    run.feeds
                        .container
                        .push(ContainerSlotEvent::YoloFinished { step_name });
                }
            }
        }
        InstanceDelta::GroupStarted { .. } => {
            tab.in_group = true;
            if let Some(run) = tab.run.as_ref() {
                run.feeds.container.push(ContainerSlotEvent::GroupStarted);
            }
        }
        InstanceDelta::GroupFinished { .. } => {
            tab.in_group = false;
            if let Some(run) = tab.run.as_mut() {
                run.slots.retain(|_, slot| {
                    let keep = slot.kind == SlotKind::Sequential;
                    if !keep {
                        if let Some(token) = &slot.subscription {
                            token.cancel();
                        }
                    }
                    keep
                });
                run.feeds.container.push(ContainerSlotEvent::GroupFinished);
            }
        }
        InstanceDelta::Workflow {
            view,
            invocation_id,
            context_path,
            worktree_path,
            ..
        } => {
            let workflow = &tab.feeds.workflow;
            match (invocation_id, view) {
                (Some(invocation), Some(view)) => workflow.publish_remote(invocation, view),
                (_, view) => workflow.update_view(|current| *current = view),
            }
            workflow.set_context_path(context_path);
            workflow.set_worktree_path(worktree_path);
        }
        InstanceDelta::Yolo { state, .. } => {
            if let Ok(mut yolo) = tab.feeds.yolo_state.lock() {
                *yolo = state;
            }
        }
        InstanceDelta::Stuck { stuck, .. } => {
            if let Some(sender) = tab.run.as_ref().and_then(|run| run.stuck.as_ref()) {
                let _ = sender.send(if stuck {
                    StuckEvent::Stuck
                } else {
                    StuckEvent::Unstuck
                });
            }
        }
        InstanceDelta::StatusLine { line, .. } => {
            tab.status_lines += 1;
            if let Ok(mut log) = tab.feeds.status_log.lock() {
                log.push(line);
            }
        }
        InstanceDelta::StatusLogReset { .. } => {
            tab.status_lines = 0;
            if let Ok(mut log) = tab.feeds.status_log.lock() {
                log.clear();
            }
        }
        InstanceDelta::AgentsDashboard { data, .. } => {
            let data = data.and_then(|wire| {
                serde_json::from_value(wire.overview).ok().map(|overview| {
                    super::feeds::AgentsDashboardData {
                        overview,
                        watching: wire.watching,
                    }
                })
            });
            if let Ok(mut dashboard) = tab.feeds.agents_dashboard.lock() {
                *dashboard = data;
            }
        }
        // Presenter-computed (labels, git summary, stats, the last
        // container's summary) or not produced by the engine in WI 0142 (ACP
        // feeds); focus is per frontend.
        InstanceDelta::TabLabel { .. }
        | InstanceDelta::SlotStats { .. }
        | InstanceDelta::SlotAcpUpdate { .. }
        | InstanceDelta::SlotAcpPermission { .. }
        | InstanceDelta::FocusedSlot { .. }
        | InstanceDelta::GitSummary { .. }
        | InstanceDelta::LastContainerSummary { .. } => {}
        InstanceDelta::TabOpened(_)
        | InstanceDelta::TabClosed { .. }
        | InstanceDelta::Annotations { .. }
        | InstanceDelta::FrontendAttached(_)
        | InstanceDelta::FrontendDetached { .. }
        | InstanceDelta::SquadHealth(_)
        | InstanceDelta::ActiveTabHint { .. }
        | InstanceDelta::DialogPending(_)
        | InstanceDelta::DialogResolved { .. } => {}
    }
}

/// An agent's self-reported status reaches the presenter on the backbone's
/// stuck channel, as the builtin backend reports it, so the slot's status,
/// the tab colour and the dialog-deferral signals all follow it. The engine
/// reports only for the backbone (as the builtin engine does).
fn report_program_status(tab: &mut RemoteTab, slot_id: &SlotId, status: Option<EffectiveStatus>) {
    with_slot(tab, slot_id, |run, kind| match kind {
        SlotKind::Sequential => {
            if let Some(sender) = run.stuck.as_ref() {
                let _ = sender.send(StuckEvent::Reported(status));
            }
        }
        SlotKind::Step(step_name) => {
            run.feeds
                .container
                .push(ContainerSlotEvent::Reported { step_name, status });
        }
    });
}

/// Replay a tab snapshot's live presentation state through the delta
/// handlers, so an attaching or reconnecting presenter shows what the engine
/// shows now instead of waiting for each value's next change.
fn restore_presentation(address: &Path, tab: &mut RemoteTab, snapshot: &TabSnapshot) {
    let tab_id = &snapshot.tab_id;
    let mut deltas = vec![
        InstanceDelta::Workflow {
            tab_id: tab_id.clone(),
            view: snapshot.workflow.clone(),
            invocation_id: snapshot.workflow_invocation_id,
            context_path: snapshot.context_path.clone(),
            worktree_path: snapshot.worktree_path.clone(),
        },
        InstanceDelta::Yolo {
            tab_id: tab_id.clone(),
            state: snapshot.yolo.clone(),
        },
        InstanceDelta::AgentsDashboard {
            tab_id: tab_id.clone(),
            data: snapshot.agents_dashboard.clone(),
        },
        InstanceDelta::Stuck {
            tab_id: tab_id.clone(),
            stuck: snapshot.stuck,
        },
    ];
    for slot in &snapshot.slots {
        deltas.push(InstanceDelta::SlotStuck {
            tab_id: tab_id.clone(),
            slot_id: slot.slot_id,
            stuck: slot.stuck,
        });
        // The kind first: a countdown's ticks are labelled with it.
        deltas.push(InstanceDelta::SlotCountdownKind {
            tab_id: tab_id.clone(),
            slot_id: slot.slot_id,
            kind: slot.countdown_kind,
        });
        deltas.push(InstanceDelta::SlotYolo {
            tab_id: tab_id.clone(),
            slot_id: slot.slot_id,
            state: slot.yolo.clone(),
        });
    }
    for delta in deltas {
        apply_tab_delta(address, tab, delta);
    }
    for slot in &snapshot.slots {
        report_program_status(tab, &slot.slot_id, slot.program_status.clone());
    }
}

/// The presenter's own name for an engine slot: the backbone by the
/// presenter-side launch id its tab state holds, a group step by name.
fn presenter_slot(tab: &mut RemoteTab, slot_id: &SlotId) -> Option<PresenterSlot> {
    let mut slot = None;
    with_slot(tab, slot_id, |run, kind| {
        slot = Some(match kind {
            SlotKind::Sequential => PresenterSlot::Launch(run.feeds.container.launch_id()),
            SlotKind::Step(step_name) => PresenterSlot::Step(step_name),
        });
    });
    slot
}

fn with_slot(tab: &mut RemoteTab, slot_id: &SlotId, f: impl FnOnce(&mut RemoteRun, SlotKind)) {
    let Some(run) = tab.run.as_mut() else {
        return;
    };
    let Some(kind) = run.slots.get(slot_id).map(|slot| slot.kind.clone()) else {
        return;
    };
    f(run, kind);
}

/// A slot appeared: bind it to the presenter's slot I/O and subscribe to its
/// hub terminal. Inside a parallel group a slot is a step's own; otherwise it
/// is the sequential backbone (the first one is the command's bound slot,
/// whose I/O the presenter already holds).
fn launch_slot(address: &Path, run: &mut RemoteRun, slot: &SlotSnapshot, in_group: bool) {
    if run.slots.contains_key(&slot.slot_id) {
        return;
    }
    let kind = if in_group && !slot.dormant {
        SlotKind::Step(slot.step_name.clone())
    } else {
        SlotKind::Sequential
    };
    let pipes = match &kind {
        SlotKind::Sequential => {
            let pipes = match run.first_slot.take() {
                Some(pipes) => pipes,
                None => {
                    let (io, pipes) = slot_channels();
                    let title = (!slot.step_name.is_empty()).then(|| slot.step_name.clone());
                    run.feeds.container.launch(title, io);
                    pipes
                }
            };
            // A dynamic leader or a later sequential step gets a fresh
            // launch id. Attach the workflow request channel to that launch,
            // just as the builtin presenter does, so Ctrl-W keeps reaching
            // the daemon after the initial bound slot is replaced.
            let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
            let (stuck_tx, stuck_rx) = tokio::sync::broadcast::channel(16);
            run.engine_requests = Some(requests_rx);
            run.stuck = Some(stuck_tx);
            run.feeds
                .container
                .push(ContainerSlotEvent::EngineAttached {
                    launch_id: run.feeds.container.launch_id(),
                    requests: Some(requests_tx),
                    stuck: Some(stuck_rx),
                });
            pipes
        }
        SlotKind::Step(step_name) => {
            let (io, pipes) = slot_channels();
            run.feeds.container.push(ContainerSlotEvent::Launched {
                step_name: step_name.clone(),
                agent: slot.agent_display_name.clone(),
                model: slot.model.clone(),
                io: Some(io),
            });
            pipes
        }
    };
    if !slot.container_name.is_empty() {
        match &kind {
            SlotKind::Sequential => run.feeds.container.running(slot.container_name.clone()),
            SlotKind::Step(step_name) => {
                run.feeds.container.push(ContainerSlotEvent::ContainerName {
                    step_name: step_name.clone(),
                    container_name: slot.container_name.clone(),
                })
            }
        }
    }
    let output = Arc::new(Mutex::new(SlotOutput {
        tx: pipes.stdout_tx.clone(),
        active: true,
    }));
    let subscription = slot.terminal_id.clone().map(|terminal_id| {
        let token = CancellationToken::new();
        tokio::spawn(subscribe_terminal(
            address.to_path_buf(),
            terminal_id,
            pipes,
            Arc::clone(&output),
            token.clone(),
        ));
        token
    });
    run.slots.insert(
        slot.slot_id,
        RemoteSlot {
            kind,
            container_name: slot.container_name.clone(),
            output,
            yolo_flag: None,
            subscription,
        },
    );
}

/// Feed one slot's hub terminal into the presenter's slot I/O: screen
/// snapshot and output into `stdout`, presenter keystrokes and resizes back
/// (after taking the input grant). 0141's terminal client and frames; the
/// subscription survives a socket loss by reconnecting within the grace.
async fn subscribe_terminal(
    address: PathBuf,
    terminal_id: TerminalId,
    mut pipes: SlotPipes,
    output: Arc<Mutex<SlotOutput>>,
    cancel: CancellationToken,
) {
    let options = TerminalClientOptions::default();
    let mut delay = RECONNECT_BACKOFF_INITIAL;
    let mut lost_at: Option<Instant> = None;
    loop {
        let mut stream = tokio::select! {
            _ = cancel.cancelled() => return,
            stream = TerminalStream::connect_local(&address, &terminal_id, &options) => match stream {
                Ok(stream) => stream,
                Err(error) => {
                    let since = *lost_at.get_or_insert_with(Instant::now);
                    if since.elapsed() + delay > PRIMARY_RECONNECT_GRACE {
                        tracing::debug!(terminal = %terminal_id, "slot terminal unreachable: {error}");
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RECONNECT_BACKOFF_MAX);
                    continue;
                }
            },
        };
        delay = RECONNECT_BACKOFF_INITIAL;
        let _ = stream.send(TerminalClientFrame::TakeControl).await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = stream.close().await;
                    return;
                }
                frame = stream.next() => match frame {
                    Ok(Some(TerminalFrame::Snapshot { screen, has_output, .. })) => {
                        // A pristine hub screen is metadata, not agent output.
                        // Feeding its ANSI redraw into the presenter would
                        // open a container overlay for commands with no PTY.
                        if has_output && !output.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).send_live(screen) {
                            return;
                        }
                    }
                    Ok(Some(TerminalFrame::Output { bytes })) => {
                        if !output.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).send_live(bytes) {
                            return;
                        }
                    }
                    Ok(Some(TerminalFrame::Exit { .. })) => return,
                    Ok(Some(TerminalFrame::Error { code, error })) => {
                        tracing::debug!(terminal = %terminal_id, code, "slot terminal refused an operation: {error}");
                    }
                    Ok(Some(TerminalFrame::Resize { .. } | TerminalFrame::Lagged)) => {}
                    Ok(None) | Err(_) => break,
                },
                bytes = pipes.stdin_rx.recv() => match bytes {
                    Some(bytes) => {
                        let _ = stream.send(TerminalClientFrame::Input { bytes }).await;
                    }
                    None => return,
                },
                size = pipes.resize_rx.recv() => {
                    if let Some((cols, rows)) = size {
                        let _ = stream.send(TerminalClientFrame::Resize { cols, rows }).await;
                    }
                }
            }
        }
        lost_at = Some(Instant::now());
    }
}

/// The session's in-flight record for an engine phase.
fn invocation_for(
    phase: &ExecutionPhase,
    previous: Option<CommandInvocation>,
) -> Option<CommandInvocation> {
    let (subcommand, status, exit_code) = match phase {
        ExecutionPhase::Idle => return None,
        ExecutionPhase::Running { command } => (command, InvocationStatus::Running, None),
        ExecutionPhase::Done { command, exit_code } => {
            (command, InvocationStatus::Done, Some(*exit_code))
        }
        ExecutionPhase::Error { command, message } => {
            (command, InvocationStatus::Error(message.clone()), None)
        }
    };
    let mut invocation = match previous {
        Some(previous) if &previous.subcommand == subcommand => previous,
        _ => CommandInvocation {
            id: uuid::Uuid::new_v4(),
            subcommand: subcommand.clone(),
            args: Vec::new(),
            status: InvocationStatus::Pending,
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
        },
    };
    let terminal = !matches!(status, InvocationStatus::Running);
    invocation.status = status;
    invocation.exit_code = exit_code;
    if terminal && invocation.finished_at.is_none() {
        invocation.finished_at = Some(chrono::Utc::now());
    }
    Some(invocation)
}

/// The presenter-side mirror of an engine session: opened over the same
/// directory with the frontend's own environment (same host, same files).
fn mirror_session(cwd: &Path, git_root: &Path, env: &EnvSnapshot) -> Result<Session, CommandError> {
    let options = SessionOpenOptions {
        env: Some(env.clone()),
        ..Default::default()
    };
    let session = if git_root.join(".git").exists() {
        Session::open_at_git_root(cwd.to_path_buf(), git_root.to_path_buf(), options)
    } else {
        Session::open_at_directory_root(cwd.to_path_buf(), options)
    };
    session.map_err(CommandError::from)
}

/// Register an engine tab (from a snapshot or a `TabOpened` delta): its
/// session mirror, presenter feeds and, when the engine already started a
/// startup command in it, that run.
fn register_tab(
    shared: &Shared,
    state: &mut State,
    snapshot: &TabSnapshot,
    session: Option<Session>,
) -> Result<(Session, TabCommandFeeds, StartupRun), CommandError> {
    let mut session = match session {
        Some(session) => session,
        None => mirror_session(
            &snapshot.session.cwd,
            &snapshot.session.git_root,
            &shared.env,
        )?,
    };
    {
        let mirrored = session.state_mut();
        mirrored.current_command = invocation_for(&snapshot.execution_phase, None);
        mirrored.current_workflow = snapshot.current_workflow.clone();
        mirrored.current_container = snapshot.current_container.clone();
    }
    let feeds = TabCommandFeeds::new();
    if let Ok(mut log) = feeds.status_log.lock() {
        log.extend(snapshot.status_log.iter().cloned());
    }
    let mut tab = RemoteTab {
        tab_id: snapshot.tab_id.clone(),
        session: session.clone(),
        kind: snapshot.kind.clone(),
        feeds: feeds.clone(),
        run: None,
        pending: None,
        unclaimed: None,
        status_lines: snapshot.status_log.len(),
        in_group: snapshot.slots.iter().any(|slot| slot.dormant),
        yolo_mode: snapshot.yolo_mode,
        closing: false,
        pty_size: None,
        last_input_seen: None,
        last_activity_sent: None,
        last_activity_step: None,
    };
    // A bound command shows as at least one slot (the engine opens the
    // command's slot when it binds it).
    let startup = match snapshot.slots.first() {
        Some(first) => {
            let (mut run, tab_feeds, slot_io) = new_run(feeds.clone());
            // The backbone (first) slot is the bound one, dormant or not.
            launch_slot(&shared.address, &mut run, first, false);
            if tab.in_group {
                run.feeds.container.push(ContainerSlotEvent::GroupStarted);
            }
            for slot in &snapshot.slots {
                launch_slot(&shared.address, &mut run, slot, tab.in_group);
            }
            let display = LaunchDisplay {
                agent_display_name: first.agent_display_name.clone(),
                unattended: snapshot.yolo_mode,
            };
            let sandboxed = first.sandboxed;
            tab.run = Some(run);
            StartupRun::Started(SubmittedCommand {
                display,
                sandboxed,
                feeds: tab_feeds,
                slot_io,
            })
        }
        None => StartupRun::NotStarted,
    };
    restore_presentation(&shared.address, &mut tab, snapshot);
    state.foreign_tabs.remove(&snapshot.tab_id);
    state.tabs.push(tab);
    Ok((session, feeds, startup))
}

/// Bring every tab in line with a snapshot: the first one after connecting,
/// or the one after a reconnect (catching up on what the gap missed).
fn resync(shared: &Shared, state: &mut State, snapshot: InstanceSnapshot) {
    set_squad_health(shared, &snapshot.squad_health);
    // Tabs that ended during the gap.
    let live: HashSet<&TabId> = snapshot.tabs.iter().map(|tab| &tab.tab_id).collect();
    let mut gone = Vec::new();
    state.tabs.retain(|tab| {
        let keep = live.contains(&tab.tab_id);
        if !keep && !tab.closing {
            gone.push(tab.tab_id.clone());
        }
        keep
    });
    for tab_id in gone {
        state.events.push_back(BackendEvent::TabClosedByEngine {
            tab_id,
            reason: "the tab ended while the engine was unreachable".into(),
        });
    }
    let known: HashSet<TabId> = state.tabs.iter().map(|tab| tab.tab_id.clone()).collect();
    for tab_snapshot in &snapshot.tabs {
        if !known.contains(&tab_snapshot.tab_id) {
            if !state.welcomed {
                // Tabs of the first snapshot are this presenter's to claim
                // (`open_initial`).
                if let Ok((_, feeds, startup)) = register_tab(shared, state, tab_snapshot, None) {
                    if let Some(tab) = state.tabs.last_mut() {
                        tab.unclaimed = Some((feeds, startup));
                    }
                }
            } else if state.foreign_tabs.contains(&tab_snapshot.tab_id) {
                // Another frontend's tab; still not ours.
            } else if let Some(waiter) = state.open_waiters.pop_front() {
                // Our `OpenTab` was answered while the socket was down.
                let opened = register_tab(shared, state, tab_snapshot, None).map(
                    |(session, feeds, startup)| OpenedTab {
                        session,
                        git_summary: GitSummaryPoller::default(),
                        feeds,
                        startup,
                    },
                );
                let _ = waiter.reply.send(opened);
            } else {
                state.foreign_tabs.insert(tab_snapshot.tab_id.clone());
            }
            continue;
        }
        let address = shared.address.clone();
        let Some(tab) = state
            .tabs
            .iter_mut()
            .find(|tab| tab.tab_id == tab_snapshot.tab_id)
        else {
            continue;
        };
        catch_up(&address, tab, tab_snapshot);
    }
    // Stats are presenter-owned state rather than a command feed. Repaint
    // their latest sample after a reconnect (or an initial mid-run attach),
    // even if no new sample arrives before the next render.
    for tab_snapshot in &snapshot.tabs {
        let Some(tab) = state
            .tabs
            .iter_mut()
            .find(|tab| tab.tab_id == tab_snapshot.tab_id)
        else {
            continue;
        };
        for slot in &tab_snapshot.slots {
            let (Some(stats), Some(presenter)) =
                (slot.stats.as_ref(), presenter_slot(tab, &slot.slot_id))
            else {
                continue;
            };
            state.events.push_back(BackendEvent::SlotStats {
                tab_id: tab_snapshot.tab_id.clone(),
                slot: presenter,
                stats: AgentStats {
                    name: stats.name.clone(),
                    cpu_percent: stats.cpu_percent,
                    memory_mb: stats.memory_mb,
                },
            });
        }
    }
    // Dialogs: deliver what is pending and not yet seen; close what was
    // answered during the gap.
    let pending: HashSet<&DialogId> = snapshot
        .pending_dialogs
        .iter()
        .map(|dialog| &dialog.dialog_id)
        .collect();
    let mut closed = Vec::new();
    for tab in &mut state.tabs {
        let Some(run) = tab.run.as_mut() else {
            continue;
        };
        run.dialogs.retain(|(id, _)| {
            let keep = pending.contains(id);
            if !keep {
                closed.push((id.clone(), tab.tab_id.clone()));
            }
            keep
        });
        run.deferred_dialogs.retain(|(id, _)| pending.contains(id));
        if closed.iter().any(|(_, tab_id)| tab_id == &tab.tab_id) {
            run.dialog_close_barrier = true;
        }
        for dialog in snapshot
            .pending_dialogs
            .iter()
            .filter(|dialog| dialog.tab_id == tab.tab_id)
        {
            deliver_dialog(run, dialog.dialog_id.clone(), dialog.request.clone());
        }
    }
    for (dialog_id, tab_id) in closed {
        state.events.push_back(BackendEvent::DialogClosed {
            dialog_id: Some(dialog_id),
            tab_id,
            answered_by: AnsweredBy::Engine,
        });
    }
    state.welcomed = true;
}

/// Catch one known tab up with its snapshot.
fn catch_up(address: &Path, tab: &mut RemoteTab, snapshot: &TabSnapshot) {
    // A Submit may have reached the daemon just before the socket dropped.
    // Its SlotLaunched acknowledgement was then lost, but the snapshot is
    // authoritative: bind that slot to the already-waiting presenter run.
    if tab.pending.as_ref().is_some_and(|pending| {
        snapshot.execution_phase.command_name() == pending.expected_command
            && snapshot.slots.first().is_some_and(|first| {
                !tab.run
                    .as_ref()
                    .is_some_and(|old| old.slots.contains_key(&first.slot_id))
            })
    }) {
        let first = &snapshot.slots[0];
        let pending = tab.pending.take().unwrap();
        tab.run = Some(pending.run);
        tab.in_group = false;
        let _ = pending.reply.send(Ok(SubmitAck {
            display: LaunchDisplay {
                agent_display_name: first.agent_display_name.clone(),
                unattended: snapshot.yolo_mode,
            },
            sandboxed: first.sandboxed,
        }));
    } else if tab.pending.as_ref().is_some_and(|pending| {
        snapshot.execution_phase.command_name() == pending.expected_command
            && snapshot.slots.is_empty()
            && matches!(
                snapshot.execution_phase,
                ExecutionPhase::Done { .. } | ExecutionPhase::Error { .. }
            )
    }) {
        let pending = tab.pending.take().unwrap();
        let _ = pending.reply.send(Err(CommandError::Other(
            "the command finished while the engine was reconnecting; check the session result"
                .into(),
        )));
    }
    catch_up_run(address, tab, snapshot);
    restore_presentation(address, tab, snapshot);
}

/// Phase, status log and the run's slots, from the snapshot.
fn catch_up_run(address: &Path, tab: &mut RemoteTab, snapshot: &TabSnapshot) {
    {
        let state = tab.session.state_mut();
        state.current_command =
            invocation_for(&snapshot.execution_phase, state.current_command.take());
        state.current_workflow = snapshot.current_workflow.clone();
        state.current_container = snapshot.current_container.clone();
    }
    if let Ok(mut log) = tab.feeds.status_log.lock() {
        *log = snapshot.status_log.clone();
    }
    tab.status_lines = snapshot.status_log.len();
    tab.yolo_mode = snapshot.yolo_mode;
    if let Ok(mut yolo) = tab.feeds.yolo_state.lock() {
        *yolo = snapshot.yolo.clone();
    }
    let in_group = snapshot.slots.iter().any(|slot| slot.dormant);
    let was_in_group = tab.in_group;
    tab.in_group = in_group;
    let Some(run) = tab.run.as_mut() else {
        return;
    };
    // Slots that ended during the gap.
    let live: HashSet<SlotId> = snapshot.slots.iter().map(|slot| slot.slot_id).collect();
    let ended: Vec<SlotId> = run
        .slots
        .keys()
        .filter(|id| !live.contains(id))
        .copied()
        .collect();
    for slot_id in ended {
        if let Some(slot) = run.slots.remove(&slot_id) {
            slot.output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .active = false;
            if let Some(token) = &slot.subscription {
                token.cancel();
            }
            if let SlotKind::Step(step_name) = slot.kind {
                run.feeds
                    .container
                    .push(ContainerSlotEvent::Exited { step_name });
            }
        }
    }
    if in_group != was_in_group {
        run.feeds.container.push(if in_group {
            ContainerSlotEvent::GroupStarted
        } else {
            ContainerSlotEvent::GroupFinished
        });
    }
    for slot in &snapshot.slots {
        launch_slot(address, run, slot, in_group);
        if let Some(current) = run.slots.get_mut(&slot.slot_id) {
            if current.container_name != slot.container_name {
                current.container_name = slot.container_name.clone();
                match &current.kind {
                    SlotKind::Sequential => {
                        run.feeds.container.running(slot.container_name.clone())
                    }
                    SlotKind::Step(step_name) => {
                        run.feeds.container.push(ContainerSlotEvent::ContainerName {
                            step_name: step_name.clone(),
                            container_name: slot.container_name.clone(),
                        })
                    }
                }
            }
        }
    }
    // A run that finished during the gap: its result frame was missed.
    if !run.finished
        && run.slots.is_empty()
        && matches!(
            snapshot.execution_phase,
            ExecutionPhase::Done { .. } | ExecutionPhase::Error { .. }
        )
    {
        run.finished = true;
        let result = match &snapshot.execution_phase {
            ExecutionPhase::Error { message, .. } => Err(CommandError::Other(message.clone())),
            _ => Ok(CommandOutcome::Empty),
        };
        let _ = run.command_result_tx.send(result);
    }
}

fn frame_name(frame: &EngineFrame) -> &'static str {
    match frame {
        EngineFrame::WorkerQueue { .. } => "worker_queue",
        EngineFrame::Welcome { .. } => "welcome",
        EngineFrame::Snapshot(_) => "snapshot",
        EngineFrame::SnapshotChunk { .. } => "snapshot_chunk",
        EngineFrame::Delta(_) => "delta",
        EngineFrame::CommandOutput { .. } => "command_output",
        EngineFrame::CommandFinished { .. } => "command_finished",
        EngineFrame::DialogRequest { .. } => "dialog_request",
        EngineFrame::DialogClosed { .. } => "dialog_closed",
        EngineFrame::ProgramStatus { .. } => "program_status",
        EngineFrame::Notice { .. } => "notice",
        EngineFrame::UiAck { .. } => "ui_ack",
        EngineFrame::Detach(_) => "detach",
        EngineFrame::CommandBytes { .. } => "command_bytes",
    }
}

fn p95(samples: &[Duration]) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort();
    let index = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted.get(index.saturating_sub(1)).copied()
}

/// Presentation proxy for the worker owned by the connected engine instance.
struct RemoteQueue {
    shared: Arc<Shared>,
    tab_id: TabId,
    generation: AtomicU64,
    snapshot: Mutex<crate::data::worker::queue::QueueSnapshot>,
}
impl RemoteQueue {
    fn update_snapshot(&self) {
        use crate::data::worker::queue::*;
        let snapshot = match self.call(QueueRequest::Snapshot) {
            QueueReply::Snapshot(snapshot) => snapshot,
            other => QueueSnapshot {
                error: Some(format!("queue unavailable: {other:?}")),
                loaded: true,
                ..Default::default()
            },
        };
        let mut current = self.snapshot.lock().unwrap();
        if *current != snapshot {
            *current = snapshot;
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn call(
        &self,
        request: crate::data::worker::queue::QueueRequest,
    ) -> crate::data::worker::queue::QueueReply {
        use crate::data::worker::queue::{PickError, QueueReply};
        let request_id = self.shared.next_ticket.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.shared
            .lock()
            .queue_waiters
            .insert(request_id, (self.tab_id.clone(), tx));
        self.shared.send(FrontendFrame::WorkerQueue {
            request_id,
            tab_id: self.tab_id.clone(),
            request,
        });
        // On a timeout the waiter stays registered: the engine's eventual
        // answer is then seen by `handle_frame`, which releases a lease that
        // was picked for a caller no longer there.
        let result = rx.recv_timeout(Duration::from_secs(120));
        result.unwrap_or_else(|_| {
            QueueReply::Error(PickError::Failed("engine queue request timed out".into()))
        })
    }
}
impl crate::command::worker::queue_source::QueueSource for RemoteQueue {
    fn snapshot(&self) -> crate::data::worker::queue::QueueSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
    fn mark_seen(&self) {
        let _ = self.call(crate::data::worker::queue::QueueRequest::Seen);
    }
    fn refresh(&self) {
        let _ = self.call(crate::data::worker::queue::QueueRequest::Refresh);
    }
    fn pick(
        &self,
        server: &str,
        item_id: &str,
    ) -> Result<crate::data::worker::queue::PickRun, crate::data::worker::queue::PickError> {
        use crate::data::worker::queue::*;
        match self.call(QueueRequest::Pick {
            server: server.into(),
            item_id: item_id.into(),
        }) {
            QueueReply::Picked(run) => Ok(run),
            QueueReply::Error(error) => Err(error),
            _ => Err(PickError::Failed("invalid engine queue response".into())),
        }
    }
    fn finish_lease(
        &self,
        lease_id: &str,
        choice: crate::data::worker::queue::LeaseCloseChoice,
    ) -> Result<(), String> {
        use crate::data::worker::queue::*;
        match self.call(QueueRequest::Finish {
            lease_id: lease_id.into(),
            choice,
        }) {
            QueueReply::Done => Ok(()),
            other => Err(format!("finish lease: {other:?}")),
        }
    }
}

#[cfg(test)]
#[path = "remote_contracts.rs"]
mod contracts;
