//! `InstanceBackend`: the Layer 2 facade a presenting frontend holds instead of
//! an `Engines` bundle or a `SessionManager` (WI 0142 Part A §5). Implemented
//! by [`super::local::LocalInstanceBackend`] (builtin) and
//! [`super::remote::RemoteInstanceBackend`] (daemon); `tui::run()` picks one and
//! nothing else in the TUI changes.
//!
//! The operations are the ones the TUI performed against its own `Engines`
//! and `SessionManager` before the split, expressed as instance operations:
//! open/close/adopt tabs, admit and submit commands, read session snapshots,
//! the squad gateway and indicator, and the stats sampler. A presenting tab
//! keeps its existing channel types; a backend only produces them.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.7 (the signatures
//! here supersede the draft there; see the "instance-backend-refactor"
//! clarification in that file).

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::indicator::SharedSquadHealth;
use crate::command::commands::squad::supervisor::{SquadStartError, SquadStartupResult};
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::error::CommandError;
use crate::command::stats_sampler::ContainerStatsSampler;
use crate::data::annotations::Annotations;
use crate::data::frontend_protocol::{AnsweredBy, DetachReasonWire, Seq, UiOutcome, UiRequest};
use crate::data::ids::{DialogId, FrontendId, InstanceId, TabId, TerminalId};
use crate::data::instance_snapshot::{AnnotationTarget, FrontendSummary, SlotId};
use crate::data::message::MessageLevel;
use crate::data::program_status::EffectiveStatus;
use crate::data::session::{Session, SessionId};

use super::feeds::ContainerSlotIo;
use super::{
    CloseTabOutcome, InitialTabOpened, InitialTabSpec, OpenTabRequest, OpenedTab, QuitPlan,
    SubmitRequest, SubmittedCommand,
};

/// Which engine a presenter is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    Builtin,
    Daemon { instance_id: InstanceId, pid: u32 },
}

/// A pending `UiRequest`; its outcome arrives as [`BackendEvent::UiAck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiTicket(pub Seq);

/// Something the engine did that the presenter did not ask for in the same
/// call. The builtin backend only ever produces `UiAck`s (always unclaimed).
#[derive(Debug, Clone)]
pub enum BackendEvent {
    TabClosedByEngine {
        tab_id: TabId,
        reason: String,
    },
    DialogClosed {
        dialog_id: Option<DialogId>,
        tab_id: TabId,
        answered_by: AnsweredBy,
    },
    UiAck {
        ticket: UiTicket,
        outcome: UiOutcome,
    },
    Notice {
        level: MessageLevel,
        text: String,
    },
    Annotations {
        target: AnnotationTarget,
        annotations: Annotations,
    },
    FrontendAttached(FrontendSummary),
    FrontendDetached {
        frontend_id: FrontendId,
    },
    Detached(DetachReasonWire),
    ProgramStatus {
        tab_id: TabId,
        slot_id: SlotId,
        status: Option<EffectiveStatus>,
    },
}

/// Why a backend can no longer serve its presenter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendLoss {
    #[error("engine detached: {0:?}")]
    Detached(DetachReasonWire),
    #[error("engine unreachable: {0}")]
    Transport(String),
}

/// The facade a presenting frontend holds.
pub trait InstanceBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn instance_id(&self) -> InstanceId;
    /// The catalogue the instance dispatches against (hints read it too).
    fn catalogue(&self) -> &'static CommandCatalogue;

    // ── Tabs ─────────────────────────────────────────────────────────────
    /// Open the first tab, exactly as `tui::run()` did. The instance starts
    /// its startup command; the presenter binds the returned run.
    fn open_initial(&self, spec: InitialTabSpec) -> InitialTabOpened;
    /// Open and register a directory session for a new tab and start its
    /// startup command (the instance chooses which).
    fn open_tab(&self, request: OpenTabRequest) -> Result<OpenedTab, CommandError>;
    /// Register a session for a tab the presenter built itself. Idempotent.
    fn adopt_session(&self, session: &Session);
    /// The session's current state, or `None` while a command holds its lock.
    fn session_snapshot(&self, session_id: &SessionId) -> Option<Session>;
    /// Close a tab: cancel its command. The presenter removes its view.
    /// Closing the last tab ends the instance.
    fn close_tab(&self, session_id: &SessionId) -> CloseTabOutcome;
    /// The tab's engine id.
    fn tab_id(&self, session_id: &SessionId) -> Option<TabId>;
    /// Which tab the presenter shows (a hint; each frontend keeps its focus).
    fn focus_hint(&self, session_id: &SessionId);
    /// The agent-window PTY size the presenter now shows for a tab; every
    /// later launch in the tab starts at it.
    fn resize_tab(&self, session_id: &SessionId, pty_size: (u16, u16));

    // ── Commands ─────────────────────────────────────────────────────────
    /// Synchronous runtime-tier admission for a command path.
    fn admit(&self, path: &[String]) -> Result<(), CommandError>;
    /// Build and spawn a command for an open tab through Dispatch, replacing
    /// (and cancelling) the tab's running command.
    fn submit(&self, request: SubmitRequest) -> Result<SubmittedCommand, CommandError>;
    /// Commands whose thread is still running.
    fn running_jobs(&self) -> usize;
    /// Container stats sampling for the instance's runtime.
    fn stats_sampler(&self) -> ContainerStatsSampler;
    /// Notices the runtime queued since the last call (a deferred builtin
    /// backend that opened late).
    fn take_runtime_notices(&self) -> Vec<String>;

    // ── Window management and lifecycle ─────────────────────────────────
    /// Route a window-management request to a claiming host presenter. The
    /// builtin backend never claims one: the ack is `claimed_by_host: false`
    /// and the presenter handles the request natively.
    fn ui_request(&self, request: UiRequest) -> UiTicket;
    /// Events since the last poll (once per presenter tick).
    fn poll(&self) -> Vec<BackendEvent>;
    /// Today's TUI exit policy for the whole instance: ends it, cancelling
    /// every running command. Idempotent; every presenter exit calls it.
    fn quit(&self) -> QuitPlan;
    /// `Err` means the engine is gone (the TUI shows its fatal dialog).
    fn health(&self) -> Result<(), BackendLoss>;
    /// The I/O ends of a hub terminal the engine opened for a host process
    /// (`DialogRequest::TerminalRelease { terminal_id: Some(..) }`): its
    /// screen and output, keystrokes and resizes. The presenter bridges its
    /// real terminal to them until `TerminalRestore`; dropping them ends the
    /// subscription. The builtin engine runs host processes on the real TTY
    /// and never names a terminal, so the default refuses.
    fn open_host_terminal(
        &self,
        terminal_id: &TerminalId,
    ) -> Result<ContainerSlotIo, CommandError> {
        Err(CommandError::Other(format!(
            "terminal {terminal_id} is not served by this engine"
        )))
    }

    // ── Squad ────────────────────────────────────────────────────────────
    fn squad_health(&self) -> SharedSquadHealth;
    /// Start the squad indicator poller; `None` when the backend's engine
    /// runs its own.
    fn start_squad_indicator(
        &self,
        cancel: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>>;
    fn squad_admission(&self) -> Result<(), SquadStartError>;
    fn squad_daemon_is_running(&self) -> Result<bool, CommandError>;
    /// Start (or reach) the squad daemon; the result arrives on the channel.
    fn start_squad_gateway(&self) -> std::sync::mpsc::Receiver<SquadStartupResult>;
    /// Open and register the squad tab's session.
    fn open_squad_session(&self) -> Result<Session, SquadStartError>;
    fn set_squad_gateway(&self, gateway: std::sync::Arc<dyn TaskGateway>);
}
