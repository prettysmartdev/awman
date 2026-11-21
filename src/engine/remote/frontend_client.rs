//! Client transport for the `awman.frontend.v1` WebSocket over the daemon's
//! local socket or named pipe, modelled on [`super::terminal_client`]
//! (WI 0142 Part B §2).
//!
//! Layer 1: connect, send/receive framed `data::frontend_protocol` values
//! through the shared `Codec`, close. Reconnect policy, sequence recovery and
//! command/event mapping belong to Layer 2's `RemoteInstanceBackend`.
//!
//! A connection is split into a [`FrontendSender`] and a [`FrontendReceiver`]
//! so one task can wait for engine frames while another sends; each half keeps
//! its own direction's sequence counter (a `Codec` per direction, both
//! starting at 1 for every new connection).
//!
//! Errors are typed so the caller can tell the cases apart:
//! - [`FrontendStreamError::Protocol`]: an oversized, malformed, unknown or
//!   out-of-order frame. Never retried (security invariant I8).
//! - [`FrontendStreamError::Refused`]: the daemon answered the upgrade or the
//!   `Hello` with `{error, code}` (`primary_exists`, `protocol_mismatch`,
//!   `not_found`, …). Never retried.
//! - [`FrontendStreamError::Transport`]: the socket failed or could not be
//!   reached. The caller may reconnect.
//!
//! Frame contents are never logged here: a `Hello` carries the frontend's
//! whole environment.

use std::path::Path;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hyper::http::{Request, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::instrument::WithSubscriber;

use crate::data::frontend_protocol::{
    Codec, EngineFrame, FrontendFrame, ProtocolError, Seq, FRONTEND_SUBPROTOCOL, MAX_FRAME_BYTES,
};
use crate::data::ids::InstanceId;

use super::terminal_client::{connect_local_stream, LocalStream};

/// Why a frontend socket operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrontendStreamError {
    /// A frame broke the protocol. The connection must be closed.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// The daemon refused the connection or the `Hello`.
    #[error("{message}")]
    Refused {
        /// HTTP status of a refused upgrade; `None` when the refusal came as
        /// a WebSocket text frame after the upgrade.
        status: Option<u16>,
        code: String,
        message: String,
    },
    /// The transport failed; reconnecting may help.
    #[error("frontend socket: {0}")]
    Transport(String),
}

impl FrontendStreamError {
    /// Whether reconnecting can help.
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

type Ws = WebSocketStream<Box<dyn LocalStream>>;

/// One open frontend socket, before it is split.
pub struct FrontendStream {
    inner: Ws,
}

impl std::fmt::Debug for FrontendStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontendStream").finish_non_exhaustive()
    }
}

/// The sending half: assigns outbound sequence numbers.
pub struct FrontendSender {
    sink: SplitSink<Ws, Message>,
    codec: Codec,
}

/// The receiving half: checks inbound sequence numbers and the size cap.
pub struct FrontendReceiver {
    stream: SplitStream<Ws>,
    codec: Codec,
}

/// The URL path of an instance's frontend socket.
pub fn frontend_path(instance: &InstanceId) -> String {
    format!("/v1/engine/instances/{instance}/frontend")
}

impl FrontendStream {
    /// Connect to `GET /v1/engine/instances/{id}/frontend` on the local
    /// endpoint and negotiate `awman.frontend.v1`.
    pub async fn connect_local(
        endpoint: &Path,
        instance: &InstanceId,
    ) -> Result<Self, FrontendStreamError> {
        let stream = connect_local_stream(endpoint)
            .await
            .map_err(|error| FrontendStreamError::Transport(error.to_string()))?;
        let request = Request::builder()
            .method("GET")
            .uri(format!("ws://localhost{}", frontend_path(instance)))
            .header("host", "localhost")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "YXdtYW4tZnJvbnRlbmQta2V5")
            .header("sec-websocket-protocol", FRONTEND_SUBPROTOCOL)
            .body(())
            .map_err(|error| FrontendStreamError::Transport(error.to_string()))?;
        // Tungstenite's own cap sits above the codec's, so a frame just over
        // MAX_FRAME_BYTES reaches the codec and is reported as `Oversized`;
        // anything far larger is refused by tungstenite and mapped the same.
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES * 2))
            .max_frame_size(Some(MAX_FRAME_BYTES * 2));
        let (inner, response) = match tokio_tungstenite::client_async_with_config(
            request,
            stream,
            Some(config),
        )
        .await
        {
            Ok(pair) => pair,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let status = response.status().as_u16();
                let body = response
                    .body()
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                return Err(refusal_from_body(Some(status), &body).unwrap_or_else(|| {
                    FrontendStreamError::Refused {
                        status: Some(status),
                        code: status_code_name(status).into(),
                        message: format!("engine refused the frontend: HTTP {status}"),
                    }
                }));
            }
            Err(error) => return Err(FrontendStreamError::Transport(error.to_string())),
        };
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(FrontendStreamError::Transport(format!(
                "frontend upgrade answered HTTP {}",
                response.status()
            )));
        }
        if response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            != Some(FRONTEND_SUBPROTOCOL)
        {
            return Err(FrontendStreamError::Refused {
                status: None,
                code: "protocol_mismatch".into(),
                message: format!("daemon did not negotiate {FRONTEND_SUBPROTOCOL}"),
            });
        }
        Ok(Self { inner })
    }

    /// Split into independently usable halves.
    pub fn split(self) -> (FrontendSender, FrontendReceiver) {
        let (sink, stream) = self.inner.split();
        (
            FrontendSender {
                sink,
                codec: Codec::new(),
            },
            FrontendReceiver {
                stream,
                codec: Codec::new(),
            },
        )
    }
}

impl FrontendSender {
    /// Encode (assigning the next seq) and send one frame. Returns its seq.
    pub async fn send(&mut self, frame: &FrontendFrame) -> Result<Seq, FrontendStreamError> {
        let text = self.codec.encode(frame)?;
        let seq = self.codec.last_sent();
        self.sink
            .send(Message::Text(text.into()))
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await
            .map_err(|error| FrontendStreamError::Transport(error.to_string()))?;
        Ok(seq)
    }

    /// The seq the next [`Self::send`] will use.
    pub fn next_seq(&self) -> Seq {
        self.codec.last_sent() + 1
    }

    /// Close the socket (best effort).
    pub async fn close(mut self) {
        let _ = self.sink.send(Message::Close(None)).await;
        let _ = self.sink.close().await;
    }
}

impl FrontendReceiver {
    /// The next engine frame; `None` when the daemon closed the socket.
    ///
    /// A `{error, code}` text frame (the daemon's refusal of a `Hello`) is
    /// [`FrontendStreamError::Refused`].
    pub async fn next(&mut self) -> Result<Option<(Seq, EngineFrame)>, FrontendStreamError> {
        loop {
            match self
                .stream
                .next()
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
            {
                Some(Ok(Message::Text(text))) => {
                    if let Some(refusal) = refusal_from_body(None, &text) {
                        return Err(refusal);
                    }
                    return self.codec.decode(&text).map(Some).map_err(Into::into);
                }
                Some(Ok(Message::Close(frame))) => {
                    // A refusal may also travel only in the close reason.
                    if let Some(frame) = frame {
                        if let Some(refusal) = refusal_from_body(None, &frame.reason) {
                            return Err(refusal);
                        }
                    }
                    return Ok(None);
                }
                None => return Ok(None),
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Binary(_))) => {
                    return Err(ProtocolError::Malformed("binary frame".into()).into())
                }
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                    return Err(ProtocolError::Oversized {
                        bytes: MAX_FRAME_BYTES * 2,
                        max: MAX_FRAME_BYTES,
                    }
                    .into())
                }
                Some(Err(error)) => return Err(FrontendStreamError::Transport(error.to_string())),
            }
        }
    }
}

/// The `{error, code}` body the daemon sends instead of a frame when it
/// refuses a frontend. A frame always has `seq` and `type`, so an object with
/// `code` and `error` and no `type` is unambiguous.
fn refusal_from_body(status: Option<u16>, body: &str) -> Option<FrontendStreamError> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    if object.contains_key("type") {
        return None;
    }
    let code = object.get("code")?.as_str()?.to_string();
    let message = object
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or(&code)
        .to_string();
    Some(FrontendStreamError::Refused {
        status,
        code,
        message,
    })
}

fn status_code_name(status: u16) -> &'static str {
    match status {
        400 => "invalid_request",
        404 => "not_found",
        409 => "primary_exists",
        426 => "protocol_mismatch",
        503 => "feature_disabled",
        _ => "refused",
    }
}
