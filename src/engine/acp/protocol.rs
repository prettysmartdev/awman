//! Typed ACP JSON-RPC 2.0 shapes and newline-delimited framing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";
pub const ACP_PROTOCOL_VERSION: u32 = 1;

pub use crate::data::acp_protocol::JsonRpcId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest<T> {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    pub params: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification<T> {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub method: String,
    pub params: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

fn jsonrpc_version() -> String {
    JSONRPC_VERSION.to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementationInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ImplementationInfo>,
}

impl Default for InitializeRequest {
    fn default() -> Self {
        Self {
            protocol_version: ACP_PROTOCOL_VERSION,
            // Do not advertise host file access. A container filesystem
            // adapter may explicitly opt in when that path is available.
            client_capabilities: ClientCapabilities {
                fs: FileSystemCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: Some(ImplementationInfo {
                name: "awman".into(),
                title: Some("awman".into()),
                version: env!("CARGO_PKG_VERSION").into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u32,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<ImplementationInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    /// The session modes the agent advertises, when it has any.
    #[serde(default)]
    pub modes: Option<SessionModeState>,
}

/// `session/set_mode` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModeRequest {
    pub session_id: String,
    pub mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: String,
}

pub use crate::data::acp_protocol::ContentBlock;

pub use crate::data::acp_protocol::ContentChunk;

pub use crate::data::acp_protocol::ToolCall;

pub use crate::data::acp_protocol::ToolCallUpdate;

pub use crate::data::acp_protocol::ToolCallLocation;

pub use crate::data::acp_protocol::PlanEntry;

pub use crate::data::acp_protocol::AvailableCommand;

pub use crate::data::acp_protocol::SessionUpdate;

pub use crate::data::acp_protocol::SessionUpdateNotification;

pub use crate::data::acp_protocol::{SessionMode, SessionModeState};

pub use crate::data::acp_protocol::PermissionOption;

pub use crate::data::acp_protocol::PermissionRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Selected { option_id: String },
    Cancelled,
}

impl PermissionDecision {
    pub fn approve(options: &[PermissionOption]) -> Self {
        // Only ever select an option the agent explicitly marked as an "allow"
        // outcome. If none is offered, fail closed (Cancelled) rather than
        // blindly picking the first option — which could be a "reject" or
        // "always-deny" choice that we'd then be treating as approval.
        options
            .iter()
            .find(|o| o.kind == "allow_once" || o.kind == "allow_always")
            .map(|o| Self::Selected {
                option_id: o.option_id.clone(),
            })
            .unwrap_or(Self::Cancelled)
    }

    pub fn as_result(&self) -> Value {
        match self {
            Self::Selected { option_id } => {
                serde_json::json!({"outcome": {"outcome": "selected", "optionId": option_id}})
            }
            Self::Cancelled => serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileRequest {
    pub session_id: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTextFileRequest {
    pub session_id: String,
    pub path: String,
    pub content: String,
}

/// Maximum bytes buffered for a single unterminated JSON-RPC line. ACP frames
/// are small structured messages; a line larger than this is treated as hostile
/// or broken. The cap bounds host memory against a compromised/buggy container
/// that streams an endless prefix with no newline (a direct containment
/// concern: bytes from inside the container must not grow awman's memory without
/// bound).
pub const MAX_ACP_LINE_BYTES: usize = 8 * 1024 * 1024;

/// One decoded line and the number of wire bytes it occupied (without its
/// newline). Queues charge decoded items by this length.
#[derive(Debug)]
pub struct Frame {
    pub bytes: usize,
    pub value: Result<Value, String>,
}

/// Incremental newline-delimited JSON framing. A bad line is returned to the
/// caller and never poisons later frames. The buffered length is capped at
/// [`MAX_ACP_LINE_BYTES`]; an over-long line is discarded (with a warning) and
/// framing resumes at the next newline.
#[derive(Debug, Default)]
pub struct LineFramer {
    pending: Vec<u8>,
    /// Prefix of `pending` already known to hold no newline, so each push
    /// scans only new bytes (a maximum-size line stays linear).
    scanned: usize,
    /// True while dropping the tail of an over-long line until the next `\n`.
    discarding: bool,
}

impl LineFramer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<Value, String>> {
        self.push_sized(bytes)
            .into_iter()
            .map(|frame| frame.value)
            .collect()
    }

    /// Like [`push`](Self::push), also reporting each line's wire length.
    pub fn push_sized(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut rest = bytes;

        // If a previous push overflowed mid-line, drop bytes until the next
        // newline, then resume normal framing on the remainder.
        if self.discarding {
            match rest.iter().position(|b| *b == b'\n') {
                Some(pos) => {
                    self.discarding = false;
                    rest = &rest[pos + 1..];
                }
                None => return frames,
            }
        }

        self.pending.extend_from_slice(rest);
        let mut start = 0;
        let mut from = self.scanned;
        while let Some(offset) = self.pending[from..].iter().position(|b| *b == b'\n') {
            let end = from + offset;
            let line = &self.pending[start..end];
            start = end + 1;
            from = start;
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            // A completed line is held to the same cap as an unterminated
            // one, before any decoding, however the bytes were partitioned.
            if line.len() > MAX_ACP_LINE_BYTES {
                frames.push(Frame {
                    bytes: 0,
                    value: Err(format!(
                        "ACP line exceeded {MAX_ACP_LINE_BYTES}-byte cap ({} bytes); discarded",
                        line.len()
                    )),
                });
                continue;
            }
            let value = match std::str::from_utf8(line) {
                Ok(text) => serde_json::from_str(text).map_err(|e| e.to_string()),
                Err(e) => Err(format!("invalid UTF-8 ACP line: {e}")),
            };
            frames.push(Frame {
                bytes: line.len(),
                value,
            });
        }
        self.pending.drain(..start);
        self.scanned = self.pending.len();

        // Cap the unterminated remainder. A peer that never emits `\n` (or emits
        // a pathologically long line) must not grow `pending` without bound.
        // Discard the partial line, warn, and enter the discard state so the
        // next newline realigns framing instead of prepending the dropped bytes.
        if self.pending.len() > MAX_ACP_LINE_BYTES {
            let buffered = self.pending.len();
            self.pending = Vec::new();
            self.scanned = 0;
            self.discarding = true;
            frames.push(Frame {
                bytes: 0,
                value: Err(format!(
                    "ACP line exceeded {MAX_ACP_LINE_BYTES}-byte cap ({buffered} bytes buffered \
                     with no newline); discarding until next newline"
                )),
            });
        }

        frames
    }

    /// Bytes currently buffered for an unterminated line.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }
}

pub fn encode_line<T: Serialize>(message: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Neutralise C0/C1 terminal-control characters (everything except `\n` and
/// `\t`) in agent-provided text before it is rendered to a terminal. ACP is a
/// structured, host-rendered UI; without this, a malicious agent could embed
/// ANSI/OSC escape sequences (cursor movement, screen clears, OSC 52 clipboard
/// writes, deceptive titles) in message/tool/plan text and recover raw
/// terminal-control effects the structured renderer is meant to prevent.
pub fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .collect()
}

fn summarize_content(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => sanitize_terminal_text(text),
        ContentBlock::Image { .. } => "(image)".into(),
        ContentBlock::Audio { .. } => "(audio)".into(),
        ContentBlock::ResourceLink { name, .. } => {
            format!("(resource link: {})", sanitize_terminal_text(name))
        }
        ContentBlock::Resource { .. } => "(resource)".into(),
    }
}

/// One-line, human-readable summary of a session update — never a raw `Debug`
/// JSON dump. Used by fallback/log frontends that don't render the full ACP
/// window. All agent-provided text is passed through [`sanitize_terminal_text`].
pub fn summarize_update(update: &SessionUpdate) -> String {
    let status_suffix =
        |s: &Option<String>| s.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default();
    match update {
        SessionUpdate::AgentMessageChunk { chunk } => {
            format!("agent: {}", summarize_content(&chunk.content))
        }
        SessionUpdate::AgentThoughtChunk { chunk } => {
            format!("thinking: {}", summarize_content(&chunk.content))
        }
        SessionUpdate::ToolCall { tool_call } => format!(
            "tool call: {}{}",
            sanitize_terminal_text(&tool_call.title),
            status_suffix(&tool_call.status),
        ),
        SessionUpdate::ToolCallUpdate { tool_call } => format!(
            "tool update: {}{}",
            tool_call
                .title
                .as_deref()
                .map(sanitize_terminal_text)
                .unwrap_or_else(|| tool_call.tool_call_id.clone()),
            status_suffix(&tool_call.status),
        ),
        SessionUpdate::Plan { entries } => format!("plan: {} step(s)", entries.len()),
        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            format!("available commands: {}", available_commands.len())
        }
        SessionUpdate::CurrentModeUpdate { current_mode_id } => {
            format!(
                "mode changed to {}",
                sanitize_terminal_text(current_mode_id)
            )
        }
    }
}

#[cfg(test)]
mod framer_cap_tests {
    use super::*;

    /// A valid JSON line of exactly `len` bytes (without the newline).
    fn line_of(len: usize) -> Vec<u8> {
        let overhead = br#"{"jsonrpc":"2.0","method":"x","params":""}"#.len();
        let mut line = br#"{"jsonrpc":"2.0","method":"x","params":""#.to_vec();
        line.extend(std::iter::repeat_n(b'a', len - overhead));
        line.extend_from_slice(br#""}"#);
        assert_eq!(line.len(), len);
        line
    }

    fn ok_sizes(frames: &[Frame]) -> Vec<usize> {
        frames
            .iter()
            .filter(|f| f.value.is_ok())
            .map(|f| f.bytes)
            .collect()
    }

    #[test]
    fn a_completed_line_over_the_cap_is_refused_however_it_is_partitioned() {
        let over = line_of(MAX_ACP_LINE_BYTES + 130);
        let ok = line_of(64);
        // One push: the whole oversize line plus its newline, then a valid line.
        let mut framer = LineFramer::default();
        let mut bytes = over.clone();
        bytes.push(b'\n');
        bytes.extend_from_slice(&ok);
        bytes.push(b'\n');
        let frames = framer.push_sized(&bytes);
        assert_eq!(ok_sizes(&frames), vec![64], "oversize line must not decode");
        assert!(frames.iter().any(|f| f.value.is_err()));
        // Split exactly at the cap: the remainder check sees MAX bytes (legal),
        // then the final bytes and the newline complete an oversize line.
        let mut framer = LineFramer::default();
        assert!(framer.push_sized(&over[..MAX_ACP_LINE_BYTES]).is_empty());
        let mut tail = over[MAX_ACP_LINE_BYTES..].to_vec();
        tail.push(b'\n');
        let frames = framer.push_sized(&tail);
        assert!(
            ok_sizes(&frames).is_empty(),
            "split oversize line must not decode"
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(framer.buffered(), 0);
        // Framing continues normally afterwards.
        let mut next = ok.clone();
        next.push(b'\n');
        assert_eq!(ok_sizes(&framer.push_sized(&next)), vec![64]);
    }

    #[test]
    fn a_line_of_exactly_the_cap_is_accepted_with_the_newline_in_any_push() {
        let max = line_of(MAX_ACP_LINE_BYTES);
        let mut framer = LineFramer::default();
        let mut bytes = max.clone();
        bytes.push(b'\n');
        assert_eq!(
            ok_sizes(&framer.push_sized(&bytes)),
            vec![MAX_ACP_LINE_BYTES]
        );
        let mut framer = LineFramer::default();
        assert!(framer.push_sized(&max).is_empty());
        assert_eq!(
            ok_sizes(&framer.push_sized(b"\n")),
            vec![MAX_ACP_LINE_BYTES]
        );
    }
}
