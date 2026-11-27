//! The daemon terminal contract (WI 0141): terminal records, the
//! `awman.terminal.v1` wire frames, viewer identity and the host-output
//! policy — every shape that crosses a process or layer boundary.
//!
//! Layer 1's `engine::terminal_hub` produces and consumes these; Layer 2's
//! `command::terminals` decides who may see or drive a terminal; Layer 3
//! (`frontend::api::terminals`, `awman attach`) moves them over WebSockets and
//! host TTYs. Nothing here does I/O.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::data::automation::{StatusSource, StatusSupport};
use crate::data::automation_records::*;
use crate::data::daemon_state::{CommandListener, FeatureName, RequestOrigin};
use crate::data::execution_event::EventSource;
use crate::data::ids::{CommandId, IdError, LeaseId, RunId, TabId, TaskId, TerminalId};
use crate::data::ids::{ExecutionId, TurnId};
use uuid::Uuid;

/// The WebSocket subprotocol every terminal viewer negotiates (0139 §1).
pub const TERMINAL_SUBPROTOCOL: &str = "awman.terminal.v1";

/// Header naming a network client for `terminal_control` events.
pub const ACTOR_HEADER: &str = "x-awman-actor";

/// Who a terminal belongs to. Access to a terminal follows its owner's rules.
///
/// `Run` is the squad owner: a squad evaluation runs inside the daemon with no
/// `cmd_` of its own, so its terminals hang off the run (`awman attach
/// task_…` resolves the task's active run first).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TerminalOwner {
    Command(CommandId),
    Tab(TabId),
    Lease(LeaseId),
    Run(RunId),
}

impl TerminalOwner {
    /// Parse a prefixed id (`cmd_`, `tab_`, `lease_`, `run_`).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.starts_with(concat!("cmd", "_")) {
            return CommandId::parse(s).map(Self::Command);
        }
        if s.starts_with(concat!("tab", "_")) {
            return TabId::parse(s).map(Self::Tab);
        }
        if s.starts_with(concat!("lease", "_")) {
            return LeaseId::parse(s).map(Self::Lease);
        }
        if s.starts_with(concat!("run", "_")) {
            return RunId::parse(s).map(Self::Run);
        }
        Err(IdError::Invalid {
            kind: "terminal owner",
            prefix: "cmd|tab|lease|run",
            value: s.to_owned(),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Command(id) => id.as_str(),
            Self::Tab(id) => id.as_str(),
            Self::Lease(id) => id.as_str(),
            Self::Run(id) => id.as_str(),
        }
    }
}

impl std::fmt::Display for TerminalOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for TerminalOwner {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TerminalOwner {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Whether the PTY behind a terminal is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Open,
    Exited,
}

impl TerminalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Exited => "exited",
        }
    }
}

/// What Layer 2 tells the runtime about the terminal a PTY launch should
/// register. Carried as a resolved container/sandbox option; a launch
/// without one (a CLI `chat`) registers no terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOwnerInfo {
    pub owner: TerminalOwner,
    pub feature: FeatureName,
    /// 0137's event-envelope object (phase, step, container, attempt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EventSource>,
    /// The command's execution number (0136).
    pub attempt: u32,
    /// The per-command half of the network-input gate; the other half is
    /// `daemon.terminal.remoteInput`.
    pub allow_remote_input: bool,
}

/// The record `GET /v1/terminals` returns and the hub keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRecord {
    #[serde(default)]
    pub credential_updates: Option<crate::data::credential_update::EffectiveCredentialUpdates>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<AutomationSnapshot>,
    pub id: TerminalId,
    pub owner_id: TerminalOwner,
    pub feature: FeatureName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EventSource>,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub attempt: u32,
    pub cols: u16,
    pub rows: u16,
    pub state: TerminalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub allow_remote_input: bool,
}

/// The screen as ANSI that redraws it, plus its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    /// Whether the agent has produced any bytes, including control-only output.
    pub has_output: bool,
    pub cols: u16,
    pub rows: u16,
    pub screen: Vec<u8>,
}

/// Bytes on the JSON wire: standard base64 with padding.
pub mod base64_bytes {
    use super::*;

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// Server → client frames of `awman.terminal.v1`. One codec serves local
/// attaches and network viewers; the set mirrors `attach_protocol.rs`.
fn snapshot_has_output_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalFrame {
    /// A refused client operation does not close the output subscription.
    Error {
        code: String,
        error: String,
    },
    /// Sent on connect and after `Lagged`.
    Snapshot {
        #[serde(default = "snapshot_has_output_default")]
        has_output: bool,
        cols: u16,
        rows: u16,
        #[serde(with = "base64_bytes")]
        screen: Vec<u8>,
    },
    Output {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    /// Retained raw output emitted before this viewer connected (WI 0105).
    /// Sent only to a viewer that asked for it (`?history=true`), before the
    /// first `Snapshot`, in order, each at most 16 KiB. It is real output a
    /// viewer may scroll; the following `Snapshot` repaints the current
    /// screen. `truncated` (first frame only) says older output was evicted
    /// or withheld; a withheld history is one empty truncated frame.
    History {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        #[serde(default)]
        truncated: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Exit {
        code: i32,
    },
    /// This viewer missed output; a `Snapshot` follows.
    Lagged,
    AutomationState {
        #[serde(flatten)]
        snapshot: Box<AutomationSnapshot>,
    },
    TurnAccepted {
        request_id: Uuid,
        execution_id: ExecutionId,
        turn_id: TurnId,
        status_support: StatusSupport,
        status_source: StatusSource,
    },
    TurnResult {
        request_id: Uuid,
        execution_id: ExecutionId,
        turn_id: TurnId,
        result: TurnResult,
    },
    ExecutionClosed {
        request_id: Uuid,
        execution_id: ExecutionId,
        result: Box<crate::data::CommandResult>,
    },
    ExecutionReplaced {
        request_id: Uuid,
        previous_execution_id: ExecutionId,
        execution_id: ExecutionId,
        #[serde(deserialize_with = "safe_counter")]
        credential_generation: u64,
    },
    CredentialUpdateState {
        snapshot: Box<crate::data::credential_update::CredentialUpdateState>,
    },
    OperationError {
        request_id: Uuid,
        code: String,
        error: String,
    },
}

/// Why a viewer left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetachReason {
    /// A normal detach (Ctrl-C).
    User,
    /// The attach lost its own terminal (SIGHUP, stdout closed).
    Hangup,
}

impl DetachReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Hangup => "hangup",
        }
    }
}

/// Client → server frames of `awman.terminal.v1`. `Input` and `Resize` are
/// honoured only for the client holding the input grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalClientFrame {
    SubmitTurn {
        #[serde(flatten)]
        request: SubmitTurnRequest,
    },
    CloseExecution {
        #[serde(flatten)]
        request: CloseExecutionRequest,
    },
    Input {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Request the (exclusive) input grant.
    TakeControl,
    /// Said once before the client goes away.
    Detached {
        reason: DetachReason,
    },
}

/// Generic integration metadata on a subscription (`--annotation k=v`).
pub use crate::data::annotations::{
    annotation_from_header, annotation_header, validate_annotation, AnnotationError, Annotations,
    ANNOTATION_HEADER_PREFIX, MAX_ANNOTATIONS, MAX_ANNOTATION_KEY_BYTES,
    MAX_ANNOTATION_VALUE_BYTES,
};

pub fn parse_annotations<S: AsRef<str>>(pairs: &[S]) -> Result<Annotations, AnnotationError> {
    Annotations::parse_cli(pairs)
}

/// Who holds a terminal's input grant, as written to `terminal_control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlHolder {
    /// A local attach (owner socket).
    Local,
    /// A network client, named by its `x-awman-actor` header.
    Actor(String),
}

impl ControlHolder {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Actor(actor) => actor,
        }
    }
}

/// The identity a viewer presents when it opens a terminal. Built by Layer 3
/// from the connection (listener, headers, CLI flags); judged by Layer 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerIdentity {
    pub origin: RequestOrigin,
    pub principal: crate::data::daemon_state::Principal,
    pub listener_name: Option<String>,
    pub listener: CommandListener,
    /// The `x-awman-actor` header, when a network client sent one.
    pub actor: Option<String>,
    pub annotations: Annotations,
    /// `--read-only`: never requests the grant, never resizes.
    pub read_only: bool,
}

impl ViewerIdentity {
    pub fn local() -> Self {
        Self {
            origin: RequestOrigin::LocalProcess,
            listener: CommandListener::Local,
            principal: crate::data::daemon_state::Principal::Owner,
            listener_name: None,
            actor: None,
            annotations: Annotations::new(),
            read_only: false,
        }
    }

    pub fn holder(&self) -> ControlHolder {
        match (&self.listener, &self.actor) {
            (CommandListener::Network, Some(actor)) => ControlHolder::Actor(actor.clone()),
            (CommandListener::Network, None) => ControlHolder::Actor(String::new()),
            (CommandListener::Local, _) => ControlHolder::Local,
        }
    }
}

/// What `awman attach <id>` was given. The prefix says what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachTarget {
    Terminal(TerminalId),
    Command(CommandId),
    Task(TaskId),
}

impl AttachTarget {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.starts_with(concat!("term", "_")) {
            return TerminalId::parse(s).map(Self::Terminal);
        }
        if s.starts_with(concat!("cmd", "_")) {
            return CommandId::parse(s).map(Self::Command);
        }
        if s.starts_with(concat!("task", "_")) {
            return TaskId::parse(s).map(Self::Task);
        }
        Err(IdError::Invalid {
            kind: "attach target",
            prefix: "term|cmd|task",
            value: s.to_owned(),
        })
    }
}

/// What a host-terminal writer does with OSC 7501 reports in agent output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgramStatusMode {
    /// Parse, then re-encode from the sanitised record. Never forward raw.
    #[default]
    Reencode,
    Strip,
}

/// How agent bytes are filtered before they reach a host TTY. Resolved by
/// Layer 2 from config and host-integration state, applied by Layer 3's
/// `HostOutputFilter`. The default strips desktop notifications; Rex (0157)
/// maps `tui.integrations.rex.forwardAgentNotifications` onto
/// `allow_notifications`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostOutputPolicy {
    pub allow_notifications: bool,
    pub program_status: ProgramStatusMode,
}

#[cfg(test)]
mod history_frame_tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../web/test/lib/fixtures/terminal-frames.json"
        ))
        .unwrap()
    }

    #[test]
    fn history_frames_round_trip_and_default_truncated() {
        let fixture = fixture();
        let examples = &fixture["history_examples"];
        let mut bytes = Vec::new();
        for (index, frame) in examples["server"].as_array().unwrap().iter().enumerate() {
            let typed: TerminalFrame = serde_json::from_value(frame.clone()).unwrap();
            let TerminalFrame::History {
                bytes: chunk,
                truncated,
            } = &typed
            else {
                panic!("not a history frame: {frame}");
            };
            assert_eq!(*truncated, index == 0);
            bytes.extend_from_slice(chunk);
            assert_eq!(serde_json::to_value(&typed).unwrap(), *frame);
        }
        assert_eq!(bytes, b"line 1\r\nline 2\r\nline 3\r\n");
        assert_eq!(
            serde_json::from_value::<TerminalFrame>(examples["without_truncated"].clone()).unwrap(),
            TerminalFrame::History {
                bytes: Vec::new(),
                truncated: false
            }
        );
        for frame in examples["invalid"].as_array().unwrap() {
            assert!(
                serde_json::from_value::<TerminalFrame>(frame.clone()).is_err(),
                "{frame}"
            );
        }
    }

    #[test]
    fn a_stream_without_history_still_parses() {
        // Older servers never send `history`; every v1 server frame still
        // decodes and none of them is a history frame.
        for frame in fixture()["server"].as_array().unwrap() {
            let typed: TerminalFrame = serde_json::from_value(frame.clone()).unwrap();
            assert!(!matches!(typed, TerminalFrame::History { .. }));
        }
    }
}

#[cfg(test)]
mod automation_frame_tests {
    use super::*;
    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../web/test/lib/fixtures/terminal-frames.json"
        ))
        .unwrap()
    }
    #[test]
    fn shared_v1_frames_round_trip_and_reject_malformed_identity() {
        let fixture = fixture();
        assert_eq!(TERMINAL_SUBPROTOCOL, "awman.terminal.v1");
        for frame in fixture["server"].as_array().unwrap() {
            let typed: TerminalFrame = serde_json::from_value(frame.clone()).unwrap();
            let encoded = serde_json::to_value(&typed).unwrap();
            for (key, value) in frame.as_object().unwrap() {
                assert_eq!(&encoded[key], value);
            }
            assert_eq!(
                serde_json::from_value::<TerminalFrame>(encoded).unwrap(),
                typed
            );
        }
        for frame in fixture["client"].as_array().unwrap() {
            let typed: TerminalClientFrame = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap(), *frame);
        }
        for frame in fixture["invalid"]["client"].as_array().unwrap() {
            assert!(
                serde_json::from_value::<TerminalClientFrame>(frame.clone()).is_err(),
                "{frame}"
            );
        }
        for frame in fixture["invalid"]["server"].as_array().unwrap() {
            assert!(
                serde_json::from_value::<TerminalFrame>(frame.clone()).is_err(),
                "{frame}"
            );
        }
        let state = serde_json::to_value(TerminalFrame::AutomationState {
            snapshot: serde_json::from_value(fixture["automation_examples"]["snapshot"].clone())
                .unwrap(),
        })
        .unwrap();
        assert!(state.get("snapshot").is_none());
        assert!(state.get("automation").is_none());
        assert!(state.get("execution_id").is_some());
        let types = include_str!("../../web/src/lib/client/types.ts");
        let matrix = include_str!("../../web/acceptance/action-matrix.json");
        for frame in [
            "automation_state",
            "turn_accepted",
            "turn_result",
            "execution_closed",
            "execution_replaced",
            "operation_error",
            "submit_turn",
            "close_execution",
        ] {
            assert!(matrix.contains(&format!("term.{frame}")));
        }
        for direction in ["server", "client"] {
            for frame in fixture[direction].as_array().unwrap() {
                assert!(types.contains(&format!("'{}'", frame["type"].as_str().unwrap())));
            }
        }
    }
    #[test]
    fn uuid_comparison_is_by_value_and_counters_do_not_wrap() {
        let fixture = fixture();
        let mut request = fixture["client"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["type"] == "submit_turn")
            .unwrap()
            .clone();
        request["request_id"] = request["request_id"]
            .as_str()
            .unwrap()
            .to_uppercase()
            .into();
        let typed: TerminalClientFrame = serde_json::from_value(request).unwrap();
        assert_eq!(
            serde_json::to_value(typed).unwrap()["request_id"],
            "12345678-1234-4234-8234-123456789abc"
        );
        assert!(next_counter(MAX_SAFE_COUNTER).is_err());
        assert_eq!(next_counter(MAX_SAFE_COUNTER - 1), Ok(MAX_SAFE_COUNTER));
    }
}
