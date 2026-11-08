//! WebSocket client for `GET /v1/terminals/{id}` (`awman.terminal.v1`) over
//! the daemon's owner-only local socket. Layer 1 transport only: it moves
//! `TerminalFrame`s in and `TerminalClientFrame`s out. What to do with them
//! (`awman attach`'s hang-up, read-only and grant rules) is Layer 2's.

use futures_util::{SinkExt, StreamExt};
use hyper::http::{HeaderName, HeaderValue, Request, StatusCode};
use std::path::Path;
use tokio_tungstenite::tungstenite::Message;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

use crate::data::ids::TerminalId;
use crate::data::terminal::{Annotations, TerminalClientFrame, TerminalFrame};
use crate::engine::error::EngineError;

/// Any duplex byte stream the client can run a WebSocket over: a Unix
/// socket on Unix, the named pipe on Windows.
pub trait LocalStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> LocalStream for T {}

/// What the client tells the server when it connects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalClientOptions {
    pub read_only: bool,
    /// `--annotation k=v`, sent as request headers the server copies into
    /// the subscription.
    pub annotations: Annotations,
    /// `x-awman-actor` for network clients; `None` on the local socket.
    pub actor: Option<String>,
}

/// One open terminal socket.
pub struct TerminalStream {
    inner: WebSocketStream<Box<dyn LocalStream>>,
}

impl std::fmt::Debug for TerminalStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalStream").finish_non_exhaustive()
    }
}

impl TerminalStream {
    /// Connect to `/v1/terminals/{id}` on the local endpoint and negotiate
    /// the `awman.terminal.v1` subprotocol.
    pub async fn connect_local(
        endpoint: &Path,
        id: &TerminalId,
        options: &TerminalClientOptions,
    ) -> Result<Self, EngineError> {
        let stream = connect_local_stream(endpoint).await?;
        let mut request = Request::builder()
            .method("GET")
            .uri(format!(
                "ws://localhost/v1/terminals/{id}?read_only={}",
                options.read_only
            ))
            .header("host", "localhost")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "YXdtYW4tYXR0YWNoLWtleQ==")
            .header(
                "sec-websocket-protocol",
                crate::data::terminal::TERMINAL_SUBPROTOCOL,
            )
            .body(())
            .map_err(|error| failure(error.to_string()))?;
        for (key, value) in options.annotations.iter() {
            let (name, value) = crate::data::terminal::annotation_header(key, value);
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| failure(error.to_string()))?,
                HeaderValue::from_str(&value).map_err(|error| failure(error.to_string()))?,
            );
        }
        let (inner, response) = tokio_tungstenite::client_async(request, stream)
            .await
            .map_err(|error| failure(error.to_string()))?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(failure(format!(
                "terminal attach refused: HTTP {}",
                response.status()
            )));
        }
        if response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            != Some(crate::data::terminal::TERMINAL_SUBPROTOCOL)
        {
            return Err(failure("daemon did not negotiate awman.terminal.v1".into()));
        }
        Ok(Self { inner })
    }

    /// The next server frame; `None` when the server closed the socket.
    pub async fn next(&mut self) -> Result<Option<TerminalFrame>, EngineError> {
        loop {
            match self.inner.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text)
                        .map(Some)
                        .map_err(|error| failure(error.to_string()))
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Binary(_))) => {
                    return Err(failure("unexpected binary terminal frame".into()))
                }
                Some(Err(error)) => return Err(failure(error.to_string())),
                Some(Ok(_)) => return Err(failure("unexpected terminal websocket frame".into())),
            }
        }
    }

    pub async fn send(&mut self, frame: TerminalClientFrame) -> Result<(), EngineError> {
        let json = serde_json::to_string(&frame).map_err(|error| failure(error.to_string()))?;
        self.inner
            .send(Message::Text(json.into()))
            .await
            .map_err(|error| failure(error.to_string()))
    }

    /// Say `Detached{reason}` and close.
    pub async fn close(mut self) -> Result<(), EngineError> {
        self.inner
            .close(None)
            .await
            .map_err(|error| failure(error.to_string()))
    }
}

#[cfg(unix)]
pub(super) async fn connect_local_stream(
    endpoint: &Path,
) -> Result<Box<dyn LocalStream>, EngineError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let uid = nix::unistd::geteuid().as_raw();
    let socket = std::fs::symlink_metadata(endpoint).map_err(|error| failure(error.to_string()))?;
    if !socket.file_type().is_socket() || socket.uid() != uid || socket.mode() & 0o077 != 0 {
        return Err(failure(format!(
            "refusing local terminal endpoint {}: it is not a private socket owned by this user",
            endpoint.display()
        )));
    }
    let parent = endpoint
        .parent()
        .ok_or_else(|| failure("local terminal endpoint has no parent".into()))?;
    let directory =
        std::fs::symlink_metadata(parent).map_err(|error| failure(error.to_string()))?;
    if !directory.file_type().is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
        return Err(failure(format!(
            "refusing local terminal endpoint {}: its directory is not private to this user",
            endpoint.display()
        )));
    }
    let stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .map_err(|error| failure(error.to_string()))?;
    if !stream.peer_cred().is_ok_and(|peer| peer.uid() == uid) {
        return Err(failure(format!(
            "refusing local terminal endpoint {}: daemon runs as a different user",
            endpoint.display()
        )));
    }
    Ok(Box::new(stream))
}

#[cfg(windows)]
pub(super) async fn connect_local_stream(
    endpoint: &Path,
) -> Result<Box<dyn LocalStream>, EngineError> {
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(endpoint)
        .map(|stream| Box::new(stream) as Box<dyn LocalStream>)
        .map_err(|error| failure(error.to_string()))
}

fn failure(message: String) -> EngineError {
    EngineError::Other(message)
}

#[cfg(all(test, unix))]
mod review_regressions {
    use super::*;
    use crate::data::terminal::{DetachReason, TERMINAL_SUBPROTOCOL};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    #[allow(clippy::result_large_err)] // tungstenite fixes the callback error response type
    async fn readonly_handshake_and_operation_errors_preserve_the_output_stream_and_detach_reason()
    {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(socket, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.uri().query(), Some("read_only=true"));
                let key = crate::data::terminal::ANNOTATION_HEADER_PREFIX.to_owned() + "rex~block";
                assert!(request.headers().contains_key(key));
                response.headers_mut().insert("sec-websocket-protocol", TERMINAL_SUBPROTOCOL.parse().unwrap());
                Ok(response)
            }).await.unwrap();
            for frame in [
                TerminalFrame::Error {
                    code: "control_held".into(),
                    error: "busy".into(),
                },
                TerminalFrame::Output {
                    bytes: b"still watching".to_vec(),
                },
                TerminalFrame::Exit { code: 7 },
            ] {
                socket
                    .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                    .await
                    .unwrap();
            }
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!()
            };
            assert!(matches!(
                serde_json::from_str::<TerminalClientFrame>(&text).unwrap(),
                TerminalClientFrame::Detached {
                    reason: DetachReason::User
                }
            ));
            let _ = socket.next().await;
        });
        let options = TerminalClientOptions {
            read_only: true,
            annotations: Annotations::try_from_pairs([("rex/block", "blk_1")]).unwrap(),
            actor: None,
        };
        let mut stream = TerminalStream::connect_local(&path, &TerminalId::new(), &options)
            .await
            .unwrap();
        assert!(
            matches!(stream.next().await.unwrap(), Some(TerminalFrame::Error { code, .. }) if code == "control_held")
        );
        assert!(
            matches!(stream.next().await.unwrap(), Some(TerminalFrame::Output { bytes }) if bytes == b"still watching")
        );
        assert!(matches!(
            stream.next().await.unwrap(),
            Some(TerminalFrame::Exit { code: 7 })
        ));
        stream
            .send(TerminalClientFrame::Detached {
                reason: DetachReason::User,
            })
            .await
            .unwrap();
        stream.close().await.unwrap();
        server.await.unwrap();
    }
}
