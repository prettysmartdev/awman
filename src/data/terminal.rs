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

use crate::data::daemon_state::{CommandListener, FeatureName};
use crate::data::execution_event::EventSource;
use crate::data::ids::{CommandId, IdError, LeaseId, RunId, TabId, TaskId, TerminalId};

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
    Resize {
        cols: u16,
        rows: u16,
    },
    Exit {
        code: i32,
    },
    /// This viewer missed output; a `Snapshot` follows.
    Lagged,
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
mod tests {
    use super::*;

    #[test]
    fn owner_and_target_parse_by_prefix() {
        let cmd = CommandId::new();
        assert_eq!(
            TerminalOwner::parse(cmd.as_str()).unwrap(),
            TerminalOwner::Command(cmd.clone())
        );
        assert!(TerminalOwner::parse("task_00000000000000000000000000000000").is_err());
        assert_eq!(
            AttachTarget::parse(cmd.as_str()).unwrap(),
            AttachTarget::Command(cmd)
        );
        assert!(AttachTarget::parse("nope").is_err());
    }

    #[test]
    fn frames_round_trip_with_base64_bytes() {
        let frame = TerminalFrame::Output {
            bytes: vec![0x1b, b'[', 0xff],
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"output\""));
        assert_eq!(serde_json::from_str::<TerminalFrame>(&json).unwrap(), frame);
        let detached = TerminalClientFrame::Detached {
            reason: DetachReason::Hangup,
        };
        assert_eq!(
            serde_json::to_string(&detached).unwrap(),
            r#"{"type":"detached","reason":"hangup"}"#
        );
    }

    #[test]
    fn annotations_validate_limits_and_preserve_values() {
        let parsed = parse_annotations(&["rex/block=blk_42", "app/view=primary"]).unwrap();
        assert_eq!(parsed.get("rex/block"), Some("blk_42"));
        assert!(matches!(
            validate_annotation("Rex/block", "x"),
            Err(AnnotationError::InvalidKey(_))
        ));
        assert!(matches!(
            validate_annotation("rex/block", &"x".repeat(MAX_ANNOTATION_VALUE_BYTES + 1)),
            Err(AnnotationError::ValueTooLong(_))
        ));
        assert!(matches!(
            parse_annotations(&["missing-separator"]),
            Err(AnnotationError::InvalidKey(_))
        ));
    }

    /// Review finding: the header form of every valid annotation is a legal
    /// HTTP header and decodes back to the same logical key and value.
    #[test]
    fn annotation_header_encoding_is_legal_and_round_trips() {
        for (key, value) in [
            ("rex/block", "blk_1"),
            ("a.b-c/d.e", "with space, 100% & ünïcode"),
            ("x/y", ""),
        ] {
            validate_annotation(key, value).unwrap();
            let (name, encoded) = annotation_header(key, value);
            hyper::http::HeaderName::from_bytes(name.as_bytes()).expect("legal name");
            hyper::http::HeaderValue::from_str(&encoded).expect("legal value");
            assert_eq!(
                annotation_from_header(&name, encoded.as_bytes()).unwrap(),
                Some((key.to_owned(), value.to_owned()))
            );
        }
        assert_eq!(
            annotation_from_header("x-awman-actor", b"me").unwrap(),
            None
        );
        for (name, value) in [
            ("x-awman-annotation-rex~block", b"bad%zz".as_slice()),
            ("x-awman-annotation-rex~block", b"trunc%4"),
            ("x-awman-annotation-rex~block", b"\xff"),
            ("x-awman-annotation-rex~block", b"%1B"),
            ("x-awman-annotation-rex/block", b"x"),
            ("x-awman-annotation-rexblock", b"x"),
        ] {
            assert!(
                annotation_from_header(name, value).is_err(),
                "{name}: {value:?}"
            );
        }
    }
}
