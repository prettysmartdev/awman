//! Multiplexed 0141 frames carried by a WSP lease terminal connection.
use serde::{Deserialize, Serialize};
pub const SUBPROTOCOL: &str = "awman.terminal.v1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalFrame {
    Snapshot {
        /// 0141 distinguishes a terminal with no output from an empty screen.
        #[serde(default = "default_has_output")]
        has_output: bool,
        cols: u16,
        rows: u16,
        screen: String,
    },
    Output {
        bytes: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Exit {
        code: Option<i32>,
    },
    Lagged,
    Error {
        code: String,
        error: String,
    },
    Input {
        bytes: String,
    },
    TakeControl,
    Detached {
        reason: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseTerminalFrame {
    pub terminal_id: String,
    #[serde(flatten)]
    pub frame: TerminalFrame,
}

impl TerminalFrame {
    /// WSP carries only ordinary terminal controls toward a worker.
    pub fn is_client_frame(&self) -> bool {
        matches!(
            self,
            Self::Input { .. } | Self::Resize { .. } | Self::TakeControl | Self::Detached { .. }
        )
    }
    pub fn is_server_frame(&self) -> bool {
        matches!(
            self,
            Self::Snapshot { .. }
                | Self::Output { .. }
                | Self::Resize { .. }
                | Self::Exit { .. }
                | Self::Lagged
                | Self::Error { .. }
        )
    }
}
impl LeaseTerminalFrame {
    pub fn decode_input(text: &str) -> Result<Self, &'static str> {
        let frame: Self = serde_json::from_str(text).map_err(|_| "invalid_frame")?;
        if !frame.frame.is_client_frame() {
            return Err("invalid_frame");
        }
        Ok(frame)
    }
}
fn default_has_output() -> bool {
    true
}

#[cfg(feature = "terminal")]
mod transport {
    use super::*;
    use crate::{client::path_id, Client, Error, VERSION_HEADER, WSP_VERSION};
    use futures_util::{Sink, Stream};
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::net::TcpStream;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{
            client::IntoClientRequest,
            http::{HeaderValue, StatusCode},
            Message,
        },
        MaybeTlsStream, WebSocketStream,
    };

    /// A lease terminal WebSocket. It records when the peer was last heard
    /// from (any message, including a pong) so a holder can send keepalive
    /// pings and notice a peer that vanished without closing the connection.
    pub struct TerminalSocket {
        socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
        last_seen: tokio::time::Instant,
        last_ping: Option<tokio::time::Instant>,
    }
    impl TerminalSocket {
        fn new(socket: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
            Self {
                socket,
                last_seen: tokio::time::Instant::now(),
                last_ping: None,
            }
        }
        /// When any message last arrived from the peer.
        pub fn last_seen(&self) -> tokio::time::Instant {
            self.last_seen
        }
        /// When [`Self::ping`] last sent a ping.
        pub fn last_ping(&self) -> Option<tokio::time::Instant> {
            self.last_ping
        }
        /// Send a keepalive ping; the peer's pong refreshes [`Self::last_seen`].
        pub async fn ping(&mut self) -> Result<(), Error> {
            self.last_ping = Some(tokio::time::Instant::now());
            futures_util::SinkExt::send(&mut self.socket, Message::Ping(Default::default()))
                .await
                .map_err(Error::from)
        }
    }
    impl std::fmt::Debug for TerminalSocket {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TerminalSocket").finish_non_exhaustive()
        }
    }
    impl Stream for TerminalSocket {
        type Item = Result<LeaseTerminalFrame, Error>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            loop {
                let message = std::task::ready!(Pin::new(&mut self.socket).poll_next(cx));
                if matches!(message, Some(Ok(_))) {
                    self.last_seen = tokio::time::Instant::now();
                }
                match message {
                    // A frame this worker cannot decode (a newer frame type, a
                    // server-only frame or a binary message) is skipped: failing
                    // the stream would only reconnect into the same frame again.
                    Some(Ok(Message::Text(text))) => {
                        match LeaseTerminalFrame::decode_input(&text) {
                            Ok(frame) => return Poll::Ready(Some(Ok(frame))),
                            Err(_) => continue,
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return Poll::Ready(None),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                }
            }
        }
    }
    impl Sink<LeaseTerminalFrame> for TerminalSocket {
        type Error = Error;
        fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Pin::new(&mut self.socket)
                .poll_ready(cx)
                .map_err(Error::from)
        }
        fn start_send(mut self: Pin<&mut Self>, frame: LeaseTerminalFrame) -> Result<(), Error> {
            let text = serde_json::to_string(&frame).map_err(|_| Error::InvalidResponse)?;
            Pin::new(&mut self.socket)
                .start_send(Message::Text(text.into()))
                .map_err(Error::from)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Pin::new(&mut self.socket)
                .poll_flush(cx)
                .map_err(Error::from)
        }
        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Pin::new(&mut self.socket)
                .poll_close(cx)
                .map_err(Error::from)
        }
    }
    impl Client {
        pub async fn open_terminal(&self, lease_id: &str) -> Result<TerminalSocket, Error> {
            let id = path_id(lease_id)?;
            let mut url = self.url(&format!("v1/leases/{id}/terminal"))?;
            let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
            url.set_scheme(scheme).map_err(|_| Error::InvalidUrl)?;
            let epoch = self.lease_epoch(id).await;
            for retry in 0..2 {
                let token = self.token().await?;
                let mut request = url.as_str().into_client_request()?;
                request
                    .headers_mut()
                    .insert(VERSION_HEADER, HeaderValue::from_static(WSP_VERSION));
                request.headers_mut().insert(
                    "x-wsp-epoch",
                    HeaderValue::from_str(&epoch.to_string())
                        .map_err(|_| Error::InvalidResponse)?,
                );
                let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| Error::InvalidResponse)?;
                auth.set_sensitive(true);
                request.headers_mut().insert("authorization", auth);
                request.headers_mut().insert(
                    "sec-websocket-protocol",
                    HeaderValue::from_static(SUBPROTOCOL),
                );
                match connect_async(request).await {
                    Ok((socket, response)) => {
                        if response
                            .headers()
                            .get("sec-websocket-protocol")
                            .and_then(|v| v.to_str().ok())
                            != Some(SUBPROTOCOL)
                        {
                            return Err(Error::InvalidResponse);
                        }
                        return Ok(TerminalSocket::new(socket));
                    }
                    Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                        if response.status() == StatusCode::UNAUTHORIZED && retry == 0 {
                            self.invalidate_access(&token).await;
                            continue;
                        }
                        let body: crate::ApiError = response
                            .body()
                            .as_ref()
                            .and_then(|b| serde_json::from_slice(b).ok())
                            .or_else(|| {
                                (response.status() == StatusCode::NOT_FOUND).then(|| {
                                    crate::ApiError {
                                        code: "not_found".into(),
                                        error: "lease terminals are unavailable".into(),
                                    }
                                })
                            })
                            .ok_or(Error::InvalidResponse)?;
                        return Err(crate::client::response_error(response.status(), body));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            unreachable!()
        }
    }
}
#[cfg(feature = "terminal")]
pub use transport::TerminalSocket;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_fixture_pins_the_ordinary_wsp_subset_and_rejects_guarded_input() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../web/test/lib/fixtures/terminal-frames.json"
        ))
        .unwrap();
        assert_eq!(SUBPROTOCOL, "awman.terminal.v1");
        for frame in fixture["server"].as_array().unwrap() {
            let allowed = fixture["wsp"]["server"]
                .as_array()
                .unwrap()
                .contains(&frame["type"]);
            let decoded = serde_json::from_value::<TerminalFrame>(frame.clone());
            assert_eq!(decoded.is_ok(), allowed);
            if let Ok(decoded) = decoded {
                assert!(decoded.is_server_frame());
            }
        }
        for frame in fixture["client"].as_array().unwrap() {
            let mut input = frame.clone();
            input["terminal_id"] = "term_11111111111111111111111111111111".into();
            let allowed = fixture["wsp"]["client"]
                .as_array()
                .unwrap()
                .contains(&frame["type"]);
            assert_eq!(
                LeaseTerminalFrame::decode_input(&input.to_string()).is_ok(),
                allowed
            );
        }
        for frame in fixture["server"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["type"] != "resize")
        {
            let mut input = frame.clone();
            input["terminal_id"] = "term_11111111111111111111111111111111".into();
            assert_eq!(
                LeaseTerminalFrame::decode_input(&input.to_string()).unwrap_err(),
                "invalid_frame"
            );
        }
    }
}
