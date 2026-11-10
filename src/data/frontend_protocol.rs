//! The local awman.frontend.v1 JSON contract and bounded, sequenced codec.
use crate::data::{
    annotations::Annotations,
    dialog::{DialogRequest, DialogResponse},
    ids::{DialogId, FrontendId, InstanceId, TabId},
    instance_snapshot::{InstanceDelta, InstanceSnapshot, SlotId},
    message::MessageLevel,
    program_status::EffectiveStatus,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub const FRONTEND_SUBPROTOCOL: &str = "awman.frontend.v1";
/// Encoded size of one WebSocket text frame, inclusive. Oversized → ProtocolError::Oversized → close + fatal path.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Payload target per `SnapshotChunk` so the envelope stays under MAX_FRAME_BYTES.
pub const SNAPSHOT_CHUNK_BYTES: usize = 768 * 1024;
pub const PRIMARY_RECONNECT_GRACE: Duration = Duration::from_secs(2);
pub const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(1);
pub type Seq = u64; // first frame of each direction is seq 1

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendKindWire {
    Tui,
    Cli,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendRole {
    Primary,
    Secondary,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewScope {
    Tab(TabId),
    All,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
} // the agent-window PTY size (D8)

/// The frontend's full environment. `Debug` prints only the variable count; never logged.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloEnv(BTreeMap<String, String>);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: String, // must equal FRONTEND_SUBPROTOCOL
    pub build: String,    // frontend's awman version string
    pub kind: FrontendKindWire,
    pub role: FrontendRole,
    pub annotations: Annotations,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub env: HelloEnv,
    pub term_size: TermSize,
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub view: Option<ViewScope>,    // Secondary only
    pub resume: Option<FrontendId>, // reconnect within PRIMARY_RECONNECT_GRACE
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabKindWire {
    #[default]
    Normal,
    Squad,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupSpawn {
    #[default]
    None,
    Ready,
    AgentsStatusWatch,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTabOptions {
    pub kind: TabKindWire,
    pub label: Option<String>,
    pub annotations: Annotations,
    pub startup: StartupSpawn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SubmitInput {
    Lease { lease_id: String, args: Vec<String> },
    CommandBox { raw: String },
    Argv { args: Vec<String> },
} // D3
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum WorkflowRequestWire {
    OpenControlBoard { step_name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendFrame {
    WorkerQueue {
        request_id: u64,
        tab_id: TabId,
        request: crate::data::worker::queue::QueueRequest,
    },
    Hello(Hello),
    OpenTab {
        dir: PathBuf,
        options: OpenTabOptions,
    },
    CloseTab {
        tab_id: TabId,
    },
    FocusTab {
        tab_id: TabId,
    }, // active-tab hint only
    Submit {
        tab_id: TabId,
        input: SubmitInput,
        pty_size: TermSize,
    },
    DialogResponse {
        dialog_id: DialogId,
        response: DialogResponse,
    },
    Cancel {
        tab_id: TabId,
        repeat: bool,
    }, // repeat = second Ctrl-C
    CancelCountdown {
        tab_id: TabId,
        step_name: Option<String>,
    },
    WorkflowRequest {
        tab_id: TabId,
        request: WorkflowRequestWire,
    },
    Activity {
        tab_id: TabId,
    }, // user typed/scrolled; coalesced ≤ 1/s
    Resize(TermSize),
    UiRequest(UiRequest),
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerWindowStateWire {
    Hidden,
    Minimized,
    Maximized,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum UiRequest {
    OpenTab {
        dir: PathBuf,
    },
    NextTab,
    PrevTab,
    CloseTab {
        tab_id: TabId,
    },
    SetContainerWindow {
        tab_id: TabId,
        slot_id: SlotId,
        state: ContainerWindowStateWire,
    },
    OpenOverview {
        tab_id: TabId,
    },
    FocusWorkflowWindow {
        tab_id: TabId,
    },
    FocusWork {
        target_id: String,
    }, // task_… | cmd_… | lease_… (0158)
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiOutcome {
    pub claimed_by_host: bool,
    pub claimed_by: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum AnsweredBy {
    Frontend { frontend_id: FrontendId },
    Api,
    Engine,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorWire {
    pub code: String,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResultWire {
    pub exit_code: i32,
    pub outcome: Option<serde_json::Value>,
    pub error: Option<ErrorWire>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DetachReasonWire {
    Quit,
    EngineDisabled,
    InstanceEnded,
    Replaced,
    DaemonShutdown,
    ProtocolError { detail: String },
}

// Wire frames are decoded once and moved, never stored in bulk; boxing the
// snapshot/delta payloads would only add indirection to the codec.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineFrame {
    WorkerQueue {
        request_id: u64,
        reply: crate::data::worker::queue::QueueReply,
    },
    CommandBytes {
        tab_id: TabId,
        stream: OutputStream,
        #[serde(with = "crate::data::terminal::base64_bytes")]
        bytes: Vec<u8>,
    },
    Welcome {
        frontend_id: FrontendId,
        instance_id: InstanceId,
        protocol: String,
        build: String,
        role: FrontendRole,
    },
    Snapshot(InstanceSnapshot), // when it fits
    SnapshotChunk {
        chunk: u32,
        of: u32,
        #[serde(with = "crate::data::terminal::base64_bytes")]
        bytes: Vec<u8>,
    },
    Delta(InstanceDelta),
    CommandOutput {
        tab_id: TabId,
        stream: OutputStream,
        level: MessageLevel,
        text: String,
    },
    CommandFinished {
        tab_id: TabId,
        result: CommandResultWire,
    },
    DialogRequest {
        dialog_id: DialogId,
        tab_id: TabId,
        request: DialogRequest,
    },
    DialogClosed {
        dialog_id: DialogId,
        answered_by: AnsweredBy,
    },
    ProgramStatus {
        tab_id: TabId,
        slot_id: SlotId,
        status: Option<EffectiveStatus>,
    },
    Notice {
        level: MessageLevel,
        text: String,
    },
    UiAck {
        request_seq: Seq,
        outcome: UiOutcome,
    },
    Detach(DetachReasonWire),
}

/// `{ "seq": n, "type": …, … }` — seq is flattened beside the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<F> {
    pub seq: Seq,
    #[serde(flatten)]
    pub frame: F,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame of {bytes} bytes exceeds {max}")]
    Oversized { bytes: usize, max: usize },
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("unknown frame type `{0}`")]
    UnknownType(String),
    #[error("out-of-order frame: expected seq {expected}, got {got}")]
    OutOfOrder { expected: Seq, got: Seq },
    #[error("wrong protocol `{offered}`; this awman speaks {FRONTEND_SUBPROTOCOL}")]
    WrongProtocol { offered: String },
    #[error("snapshot chunk {chunk} of {of} does not continue the current snapshot")]
    SnapshotChunkMismatch { chunk: u32, of: u32 },
    #[error("snapshot larger than {0} bytes")]
    SnapshotTooLarge(usize),
}

/// Per-connection, per-direction sequencing and size enforcement. Pure.
impl HelloEnv {
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}
impl std::fmt::Debug for HelloEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HelloEnv(<{} variables redacted>)", self.len())
    }
}
impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("protocol", &self.protocol)
            .field("build", &self.build)
            .field("kind", &self.kind)
            .field("role", &self.role)
            .field("annotations", &self.annotations)
            .field("cwd", &self.cwd)
            .field("argv", &self.argv)
            .field("env", &self.env)
            .field("term_size", &self.term_size)
            .field("stdin_is_tty", &self.stdin_is_tty)
            .field("stdout_is_tty", &self.stdout_is_tty)
            .field("view", &self.view)
            .field("resume", &self.resume)
            .finish()
    }
}
impl UiRequest {
    pub fn name(&self) -> &'static str {
        match self {
            Self::OpenTab { .. } => "open_tab",
            Self::NextTab => "next_tab",
            Self::PrevTab => "prev_tab",
            Self::CloseTab { .. } => "close_tab",
            Self::SetContainerWindow { .. } => "set_container_window",
            Self::OpenOverview { .. } => "open_overview",
            Self::FocusWorkflowWindow { .. } => "focus_workflow_window",
            Self::FocusWork { .. } => "focus_work",
        }
    }
    pub fn parse_name(s: &str) -> Option<&'static str> {
        match s {
            "open_tab" => Some("open_tab"),
            "next_tab" => Some("next_tab"),
            "prev_tab" => Some("prev_tab"),
            "close_tab" => Some("close_tab"),
            "set_container_window" => Some("set_container_window"),
            "open_overview" => Some("open_overview"),
            "focus_workflow_window" => Some("focus_workflow_window"),
            "focus_work" => Some("focus_work"),
            _ => None,
        }
    }
}
impl UiOutcome {
    pub fn unclaimed() -> Self {
        Self::default()
    }
    pub fn claimed(by: &str) -> Self {
        Self {
            claimed_by_host: true,
            claimed_by: Some(by.into()),
            detail: None,
        }
    }
}
impl DetachReasonWire {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::EngineDisabled => "engine disabled",
            Self::InstanceEnded => "instance ended",
            Self::Replaced => "replaced",
            Self::DaemonShutdown => "daemon shutdown",
            Self::ProtocolError { .. } => "protocol error",
        }
    }
}

/// Each connection owns one codec. Invalid frames leave both counters untouched.
#[derive(Debug)]
pub struct Codec {
    next_out: Seq,
    expect_in: Seq,
}
impl Default for Codec {
    fn default() -> Self {
        Self::new()
    }
}
impl Codec {
    pub fn new() -> Self {
        Self {
            next_out: 1,
            expect_in: 1,
        }
    }
    pub fn last_sent(&self) -> Seq {
        self.next_out - 1
    }
    pub fn encode<F: Serialize>(&mut self, frame: &F) -> Result<String, ProtocolError> {
        let next = self
            .next_out
            .checked_add(1)
            .ok_or_else(|| ProtocolError::Malformed("sequence exhausted".into()))?;
        let text = serde_json::to_string(&Envelope {
            seq: self.next_out,
            frame,
        })
        .map_err(|e| ProtocolError::Malformed(e.to_string()))?;
        check_size(text.len())?;
        self.next_out = next;
        Ok(text)
    }
    pub fn decode<F: DeserializeOwned>(&mut self, text: &str) -> Result<(Seq, F), ProtocolError> {
        check_size(text.len())?;
        // Deserialize once. Inspect only the envelope and protocol, never log the env payload.
        let mut value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| ProtocolError::Malformed("expected an object".into()))?;
        let seq = object
            .remove("seq")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| ProtocolError::Malformed("missing or invalid seq".into()))?;
        let kind = object
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ProtocolError::Malformed("missing or invalid type".into()))?
            .to_owned();
        if seq != self.expect_in {
            return Err(ProtocolError::OutOfOrder {
                expected: self.expect_in,
                got: seq,
            });
        }
        let next = seq
            .checked_add(1)
            .ok_or_else(|| ProtocolError::Malformed("sequence exhausted".into()))?;
        if kind == "hello" {
            if let Some(offered) = object.get("protocol").and_then(|p| p.as_str()) {
                if offered != FRONTEND_SUBPROTOCOL {
                    return Err(ProtocolError::WrongProtocol {
                        offered: offered.into(),
                    });
                }
            }
        }
        let frame = serde_json::from_value(value).map_err(|e| {
            if e.to_string().starts_with("unknown variant") {
                ProtocolError::UnknownType(kind)
            } else {
                ProtocolError::Malformed(e.to_string())
            }
        })?;
        self.expect_in = next;
        Ok((seq, frame))
    }
}
fn check_size(bytes: usize) -> Result<(), ProtocolError> {
    if bytes > MAX_FRAME_BYTES {
        Err(ProtocolError::Oversized {
            bytes,
            max: MAX_FRAME_BYTES,
        })
    } else {
        Ok(())
    }
}

/// Bound the complete reassembled snapshot independently of the individual frame cap.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub fn chunk_snapshot(snapshot: &InstanceSnapshot) -> Result<Vec<EngineFrame>, ProtocolError> {
    let bytes =
        serde_json::to_vec(snapshot).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(ProtocolError::SnapshotTooLarge(MAX_SNAPSHOT_BYTES));
    }
    let frame = EngineFrame::Snapshot(snapshot.clone());
    // Allow for the largest seq, rather than the first seq of a fresh connection.
    let encoded = serde_json::to_vec(&Envelope {
        seq: Seq::MAX,
        frame: &frame,
    })
    .map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    if encoded.len() <= MAX_FRAME_BYTES {
        return Ok(vec![frame]);
    }
    // Base64 expands 768 KiB to exactly 1 MiB. Reserve space for the envelope.
    let payload = SNAPSHOT_CHUNK_BYTES - 256;
    let of = bytes.len().div_ceil(payload) as u32;
    Ok(bytes
        .chunks(payload)
        .enumerate()
        .map(|(chunk, bytes)| EngineFrame::SnapshotChunk {
            chunk: chunk as u32,
            of,
            bytes: bytes.to_vec(),
        })
        .collect())
}
#[derive(Debug, Default)]
pub struct SnapshotAssembler {
    next: u32,
    of: u32,
    buffer: Vec<u8>,
}
impl SnapshotAssembler {
    pub fn push(
        &mut self,
        chunk: u32,
        of: u32,
        bytes: &[u8],
    ) -> Result<Option<InstanceSnapshot>, ProtocolError> {
        if of == 0 || chunk >= of || chunk != self.next || (self.next != 0 && of != self.of) {
            return Err(ProtocolError::SnapshotChunkMismatch { chunk, of });
        }
        if bytes.is_empty()
            || bytes.len() > SNAPSHOT_CHUNK_BYTES
            || of as usize > MAX_SNAPSHOT_BYTES
            || self.buffer.len().saturating_add(bytes.len()) > MAX_SNAPSHOT_BYTES
        {
            return Err(ProtocolError::SnapshotTooLarge(MAX_SNAPSHOT_BYTES));
        }
        self.of = of;
        self.buffer.extend_from_slice(bytes);
        self.next += 1;
        if self.next == of {
            let result = serde_json::from_slice(&self.buffer)
                .map_err(|e| ProtocolError::Malformed(e.to_string()));
            *self = Self::default();
            result.map(Some)
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::instance_snapshot::{LabelColorWire, SquadHealthWire, TabLabelWire};
    use chrono::Utc;
    fn hello() -> Hello {
        Hello {
            protocol: FRONTEND_SUBPROTOCOL.into(),
            build: "test".into(),
            kind: FrontendKindWire::Tui,
            role: FrontendRole::Primary,
            annotations: Annotations::new(),
            cwd: "/repo".into(),
            argv: vec!["awman".into()],
            env: HelloEnv::from_pairs([("GITHUB_TOKEN", "secret")]),
            term_size: TermSize { cols: 80, rows: 24 },
            stdin_is_tty: true,
            stdout_is_tty: true,
            view: None,
            resume: None,
        }
    }
    pub(crate) fn snapshot() -> InstanceSnapshot {
        InstanceSnapshot {
            instance_id: InstanceId::new(),
            created_at: Utc::now(),
            annotations: Annotations::new(),
            primary_frontend_kind: Some(FrontendKindWire::Tui),
            frontends: vec![],
            tabs: vec![],
            active_tab_hint: None,
            squad_health: SquadHealthWire::Unknown,
            pending_dialogs: vec![],
            running_jobs: 0,
        }
    }
    fn ui_requests() -> Vec<UiRequest> {
        let tab_id = TabId::new();
        vec![
            UiRequest::OpenTab {
                dir: "/repo".into(),
            },
            UiRequest::NextTab,
            UiRequest::PrevTab,
            UiRequest::CloseTab {
                tab_id: tab_id.clone(),
            },
            UiRequest::SetContainerWindow {
                tab_id: tab_id.clone(),
                slot_id: SlotId(uuid::Uuid::new_v4()),
                state: ContainerWindowStateWire::Maximized,
            },
            UiRequest::OpenOverview {
                tab_id: tab_id.clone(),
            },
            UiRequest::FocusWorkflowWindow { tab_id },
            UiRequest::FocusWork {
                target_id: "task_example".into(),
            },
        ]
    }
    fn frontend_frames(text: &str) -> Vec<FrontendFrame> {
        let tab_id = TabId::new();
        let size = TermSize { cols: 80, rows: 24 };
        let mut frames = vec![
            FrontendFrame::Hello(hello()),
            FrontendFrame::OpenTab {
                dir: text.into(),
                options: OpenTabOptions::default(),
            },
            FrontendFrame::CloseTab {
                tab_id: tab_id.clone(),
            },
            FrontendFrame::FocusTab {
                tab_id: tab_id.clone(),
            },
            FrontendFrame::Submit {
                tab_id: tab_id.clone(),
                input: SubmitInput::CommandBox { raw: text.into() },
                pty_size: size,
            },
            FrontendFrame::Submit {
                tab_id: tab_id.clone(),
                input: SubmitInput::Argv {
                    args: vec![text.into()],
                },
                pty_size: size,
            },
            FrontendFrame::Cancel {
                tab_id: tab_id.clone(),
                repeat: true,
            },
            FrontendFrame::CancelCountdown {
                tab_id: tab_id.clone(),
                step_name: Some(text.into()),
            },
            FrontendFrame::WorkflowRequest {
                tab_id: tab_id.clone(),
                request: WorkflowRequestWire::OpenControlBoard {
                    step_name: text.into(),
                },
            },
            FrontendFrame::Activity { tab_id },
            FrontendFrame::Resize(size),
            FrontendFrame::Quit,
        ];
        frames.extend(ui_requests().into_iter().map(FrontendFrame::UiRequest));
        frames.extend(
            crate::data::dialog::tests::responses(text)
                .into_iter()
                .map(|response| FrontendFrame::DialogResponse {
                    dialog_id: DialogId::new(),
                    response,
                }),
        );
        frames
    }
    fn engine_frames(text: &str) -> Vec<EngineFrame> {
        let tab_id = TabId::new();
        let mut frames = vec![
            EngineFrame::Welcome {
                frontend_id: FrontendId::new(),
                instance_id: InstanceId::new(),
                protocol: FRONTEND_SUBPROTOCOL.into(),
                build: text.into(),
                role: FrontendRole::Secondary,
            },
            EngineFrame::Snapshot(snapshot()),
            EngineFrame::SnapshotChunk {
                chunk: 0,
                of: 1,
                bytes: text.as_bytes().to_vec(),
            },
            EngineFrame::Delta(InstanceDelta::TabLabel {
                tab_id: tab_id.clone(),
                label: TabLabelWire {
                    text: text.into(),
                    color: LabelColorWire::Remote,
                },
            }),
            EngineFrame::CommandOutput {
                tab_id: tab_id.clone(),
                stream: OutputStream::Stdout,
                level: MessageLevel::Info,
                text: text.into(),
            },
            EngineFrame::CommandOutput {
                tab_id: tab_id.clone(),
                stream: OutputStream::Stderr,
                level: MessageLevel::Error,
                text: text.into(),
            },
            EngineFrame::CommandFinished {
                tab_id: tab_id.clone(),
                result: CommandResultWire {
                    exit_code: 2,
                    outcome: Some(serde_json::json!({"value":text})),
                    error: Some(ErrorWire {
                        code: "test".into(),
                        message: text.into(),
                    }),
                },
            },
            EngineFrame::ProgramStatus {
                tab_id: tab_id.clone(),
                slot_id: SlotId(uuid::Uuid::new_v4()),
                status: None,
            },
            EngineFrame::Notice {
                level: MessageLevel::Warning,
                text: text.into(),
            },
            EngineFrame::UiAck {
                request_seq: 1,
                outcome: UiOutcome::claimed("test"),
            },
            EngineFrame::DialogClosed {
                dialog_id: DialogId::new(),
                answered_by: AnsweredBy::Frontend {
                    frontend_id: FrontendId::new(),
                },
            },
            EngineFrame::DialogClosed {
                dialog_id: DialogId::new(),
                answered_by: AnsweredBy::Api,
            },
            EngineFrame::DialogClosed {
                dialog_id: DialogId::new(),
                answered_by: AnsweredBy::Engine,
            },
        ];
        frames.extend(
            crate::data::dialog::tests::requests(text)
                .into_iter()
                .map(|request| EngineFrame::DialogRequest {
                    dialog_id: DialogId::new(),
                    tab_id: tab_id.clone(),
                    request,
                }),
        );
        frames.extend(
            [
                DetachReasonWire::Quit,
                DetachReasonWire::EngineDisabled,
                DetachReasonWire::InstanceEnded,
                DetachReasonWire::Replaced,
                DetachReasonWire::DaemonShutdown,
                DetachReasonWire::ProtocolError {
                    detail: text.into(),
                },
            ]
            .into_iter()
            .map(EngineFrame::Detach),
        );
        frames
    }
    fn round_trip<F: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(frames: Vec<F>) {
        let mut sender = Codec::new();
        let mut receiver = Codec::default();
        for (index, frame) in frames.into_iter().enumerate() {
            let text = sender.encode(&frame).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["seq"], (index + 1) as u64);
            assert!(value["type"].is_string());
            assert_eq!(
                receiver.decode::<F>(&text).unwrap(),
                ((index + 1) as u64, frame)
            );
        }
    }
    #[test]
    fn every_frame_and_nested_dialog_round_trips_over_generated_values() {
        for seed in 0..96 {
            let text = format!("case-{seed}:{}", "é🙂\n\t\"\\".repeat(seed % 17));
            round_trip(frontend_frames(&text));
            round_trip(engine_frames(&text));
        }
    }
    #[test]
    fn ui_requests_use_the_same_snake_case_shape_as_rest() {
        for request in ui_requests() {
            let json = serde_json::to_value(&request).unwrap();
            assert_eq!(json["request"], request.name());
            assert_eq!(UiRequest::parse_name(request.name()), Some(request.name()));
            assert_eq!(serde_json::from_value::<UiRequest>(json).unwrap(), request);
        }
        assert_eq!(UiRequest::parse_name("OpenOverview"), None);
    }
    #[test]
    fn hello_debug_redacts_all_environment_values() {
        let hello = hello();
        assert!(!format!("{hello:?}").contains("secret"));
        assert!(format!("{hello:?}").contains("redacted"));
        assert!(!format!("{:?}", FrontendFrame::Hello(hello.clone())).contains("secret"));
        assert_eq!(hello.env.names(), vec!["GITHUB_TOKEN"]);
        assert_eq!(
            serde_json::from_str::<Hello>(&serde_json::to_string(&hello).unwrap()).unwrap(),
            hello
        );
    }
    #[test]
    fn invalid_frames_are_errors_and_do_not_advance_sequence() {
        let mut codec = Codec::new();
        for text in [
            "",
            "null",
            "[]",
            "{}",
            r#"{"seq":1,"type":null}"#,
            r#"{"seq":-1,"type":"quit"}"#,
            r#"{"seq":1,"type":"submit"}"#,
        ] {
            assert!(codec.decode::<FrontendFrame>(text).is_err());
        }
        assert!(matches!(
            codec.decode::<FrontendFrame>(r#"{"seq":1,"type":"unknown"}"#),
            Err(ProtocolError::UnknownType(_))
        ));
        assert!(matches!(
            codec.decode::<FrontendFrame>(r#"{"seq":2,"type":"quit"}"#),
            Err(ProtocolError::OutOfOrder {
                expected: 1,
                got: 2
            })
        ));
        assert!(matches!(
            codec.decode::<FrontendFrame>(&" ".repeat(MAX_FRAME_BYTES + 1)),
            Err(ProtocolError::Oversized { .. })
        ));
        assert_eq!(
            codec
                .decode::<FrontendFrame>(r#"{"seq":1,"type":"quit"}"#)
                .unwrap()
                .0,
            1
        );
        assert!(matches!(
            codec.decode::<FrontendFrame>(r#"{"seq":1,"type":"quit"}"#),
            Err(ProtocolError::OutOfOrder { .. })
        ));
        let mut offered = hello();
        offered.protocol = "awman.frontend.v0".into();
        let text = Codec::new().encode(&FrontendFrame::Hello(offered)).unwrap();
        assert!(matches!(
            Codec::new().decode::<FrontendFrame>(&text),
            Err(ProtocolError::WrongProtocol { .. })
        ));
        let huge = EngineFrame::Notice {
            level: MessageLevel::Info,
            text: "x".repeat(MAX_FRAME_BYTES),
        };
        assert!(matches!(
            codec.encode(&huge),
            Err(ProtocolError::Oversized { .. })
        ));
        assert_eq!(codec.last_sent(), 0);
        assert!(codec.encode(&FrontendFrame::Quit).is_ok());
    }
    #[test]
    fn generated_untrusted_input_never_panics() {
        let mut random = 0x142u64;
        for len in 0..2048 {
            let text: String = (0..len % 257)
                .map(|_| {
                    random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                    char::from_u32((random % 0x110000) as u32).unwrap_or('\0')
                })
                .collect();
            let _ = Codec::new().decode::<FrontendFrame>(&text);
            let _ = Codec::new().decode::<EngineFrame>(&text);
        }
        for seq in [0, 2, 100, u64::MAX] {
            assert!(Codec::new()
                .decode::<FrontendFrame>(&format!(r#"{{"seq":{seq},"type":"quit"}}"#))
                .is_err());
        }
        let mut codec = Codec {
            next_out: u64::MAX,
            expect_in: u64::MAX,
        };
        assert!(codec.encode(&FrontendFrame::Quit).is_err());
        assert!(codec
            .decode::<FrontendFrame>(&format!(r#"{{"seq":{},"type":"quit"}}"#, u64::MAX))
            .is_err());
    }
    #[test]
    fn chunked_snapshots_fit_and_reassemble_and_reject_bad_chunks() {
        let mut snapshot = snapshot();
        assert!(matches!(
            &chunk_snapshot(&snapshot).unwrap()[..],
            [EngineFrame::Snapshot(_)]
        ));
        snapshot
            .pending_dialogs
            .push(crate::data::instance_snapshot::PendingDialog {
                dialog_id: DialogId::new(),
                tab_id: TabId::new(),
                request: DialogRequest::YesNo {
                    title: "large".into(),
                    body: "🙂".repeat(MAX_FRAME_BYTES),
                },
                raised_at: Utc::now(),
            });
        let chunks = chunk_snapshot(&snapshot).unwrap();
        assert!(chunks.len() > 1);
        let mut assembler = SnapshotAssembler::default();
        let mut result = None;
        for frame in chunks {
            let text = Codec::new().encode(&frame).unwrap();
            assert!(text.len() <= MAX_FRAME_BYTES);
            let (_, frame) = Codec::new().decode::<EngineFrame>(&text).unwrap();
            let EngineFrame::SnapshotChunk { chunk, of, bytes } = frame else {
                panic!("expected chunks")
            };
            result = assembler.push(chunk, of, &bytes).unwrap();
        }
        assert_eq!(result, Some(snapshot));
        assert!(matches!(
            assembler.push(1, 2, b"x"),
            Err(ProtocolError::SnapshotChunkMismatch { .. })
        ));
        assert!(assembler.push(0, 0, b"x").is_err());
        assert!(assembler.push(0, 1, b"not json").is_err());
        assert!(assembler.push(0, 2, b"x").unwrap().is_none());
        assert!(assembler.push(1, 3, b"x").is_err());
        assert!(SnapshotAssembler::default()
            .push(0, 1, &vec![0; SNAPSHOT_CHUNK_BYTES + 1])
            .is_err());
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn byte_cap_is_inclusive_in_each_direction() {
        let blank = EngineFrame::Notice {
            level: MessageLevel::Info,
            text: String::new(),
        };
        let overhead = Codec::new().encode(&blank).unwrap().len();
        let mut frame = EngineFrame::Notice {
            level: MessageLevel::Info,
            text: "x".repeat(MAX_FRAME_BYTES - overhead),
        };
        let text = Codec::new().encode(&frame).unwrap();
        assert_eq!(text.len(), MAX_FRAME_BYTES);
        assert_eq!(Codec::new().decode::<EngineFrame>(&text).unwrap().1, frame);
        if let EngineFrame::Notice { text, .. } = &mut frame {
            text.push('x');
        }
        assert!(
            matches!(Codec::new().encode(&frame),Err(ProtocolError::Oversized{bytes,..}) if bytes == MAX_FRAME_BYTES+1)
        );
        let mut text = String::from(r#"{"seq":1,"type":"quit"}"#);
        text.extend(std::iter::repeat_n(' ', MAX_FRAME_BYTES - text.len()));
        assert_eq!(
            Codec::new().decode::<FrontendFrame>(&text).unwrap().1,
            FrontendFrame::Quit
        );
        text.push(' ');
        assert!(matches!(
            Codec::new().decode::<FrontendFrame>(&text),
            Err(ProtocolError::Oversized { .. })
        ));
    }
}
