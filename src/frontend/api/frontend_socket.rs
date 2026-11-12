//! Bounded, sequenced frontend WebSocket transport. Never log frame contents.
use super::error::ApiError;
use crate::engine::auth::browser_grants::GrantWatch;
use crate::{
    command::{
        daemon_api::DaemonApi,
        instance::{admission::AttachRefusal, host::EngineInstanceHost},
    },
    data::{
        browser_auth::BROWSER_HELLO_LIMIT,
        frontend_protocol::{Codec, FrontendFrame, FRONTEND_SUBPROTOCOL, MAX_FRAME_BYTES},
        ids::InstanceId,
    },
};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use std::{sync::Arc, time::Duration};
use tracing::instrument::WithSubscriber;

pub(super) async fn upgrade(
    State(api): State<Arc<DaemonApi>>,
    Path(id): Path<String>,
    axum::Extension(principal): axum::Extension<crate::data::daemon_state::Principal>,
    grant: Option<axum::Extension<GrantWatch>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // WI 0160 C5: a browser frontend is admitted under its live grant, the
    // only authority that may later resume its primary.
    let grant = grant.map(|g| g.0);
    if let crate::data::daemon_state::Principal::Browser { grant_id, .. } = &principal {
        if grant
            .as_ref()
            .is_none_or(|g| g.grant_id().as_str() != grant_id.as_str())
        {
            return ApiError::browser_ticket_invalid("the browser grant has ended").into_response();
        }
    }
    let Ok(id) = InstanceId::parse(&id) else {
        return ApiError::not_found("instance not found").into_response();
    };
    if api.runtime.engine_host().get(&id).is_none() {
        return ApiError::not_found("instance not found").into_response();
    }
    let offered = headers
        .get_all(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .any(|s| s.trim() == FRONTEND_SUBPROTOCOL);
    if !offered {
        return ApiError::invalid_request("frontend WebSockets require awman.frontend.v1")
            .into_response();
    }
    let host = api.runtime.engine_host().clone();
    // Only the real subprotocol is selected; a browser's `awman.ticket.<t>`
    // offer is never echoed.
    ws.protocols([FRONTEND_SUBPROTOCOL])
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| serve_frontend_socket(socket, host, id, grant))
}
async fn refuse(socket: &mut WebSocket, code: &str, message: &str) {
    let _ = socket
        .send(Message::Text(
            serde_json::json!({"code":code,"error":message})
                .to_string()
                .into(),
        ))
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
    let _ = socket
        .send(Message::Close(None))
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
}
/// Close with a policy code and reason (grant revoked, listener closed, …).
async fn close_policy(socket: &mut WebSocket, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1008,
            reason: reason.into(),
        })))
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
}
/// Resolves when a browser grant ends; never for owner/API-key sockets.
async fn grant_ended(grant: Option<&GrantWatch>) -> &'static str {
    match grant {
        Some(grant) => grant.ended().await.close_reason(),
        None => std::future::pending().await,
    }
}
pub(super) async fn serve_frontend_socket(
    mut socket: WebSocket,
    host: Arc<EngineInstanceHost>,
    id: InstanceId,
    grant: Option<GrantWatch>,
) {
    let browser = grant.is_some();
    let mut incoming = Codec::new();
    // Transport libraries trace raw WebSocket payloads at TRACE. Hello
    // carries a process environment, so suppress payload-library diagnostics
    // while polling reads/writes; our own diagnostics contain frame types only.
    // Biased: when a Hello and the grant's ending are both ready, the
    // ending wins and the Hello is never read.
    let first = tokio::select! {
        biased;
        reason = grant_ended(grant.as_ref()) => {
            close_policy(&mut socket, reason).await;
            return;
        }
        first = tokio::time::timeout(
            Duration::from_secs(5),
            socket
                .recv()
                .with_subscriber(tracing::subscriber::NoSubscriber::default()),
        ) => first,
    };
    let Ok(Some(Ok(Message::Text(first)))) = first else {
        refuse(&mut socket, "protocol_error", "expected Hello").await;
        return;
    };
    // A browser Hello carries no environment; bound it tightly.
    if browser && first.len() > BROWSER_HELLO_LIMIT {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 1009,
                reason: "Hello too large".into(),
            })))
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await;
        return;
    }
    let hello = match incoming.decode::<FrontendFrame>(&first) {
        Ok((_, FrontendFrame::Hello(hello))) => hello,
        _ => {
            refuse(&mut socket, "protocol_error", "expected Hello").await;
            return;
        }
    };
    if browser && hello.kind != crate::data::frontend_protocol::FrontendKindWire::Web {
        refuse(
            &mut socket,
            "protocol_error",
            "browser connections require web frontend kind",
        )
        .await;
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let session = match host.attach_frontend_as(id, hello, tx, grant.clone()).await {
        Ok(session) => session,
        // The grant ended before admission finished: the same 1008 close
        // a live socket gets, with nothing admitted.
        Err(AttachRefusal::GrantEnded(reason)) => {
            close_policy(&mut socket, reason.close_reason()).await;
            return;
        }
        Err(e) => {
            refuse(&mut socket, e.code(), &e.to_string()).await;
            return;
        }
    };
    let mut outgoing = Codec::new();
    loop {
        tokio::select! {
        message = socket.recv().with_subscriber(tracing::subscriber::NoSubscriber::default()) => match message {
                Some(Ok(Message::Text(text))) => match incoming.decode::<FrontendFrame>(&text) {
                    Ok((seq,frame)) => if session.handle(seq,frame).await.is_err() { refuse(&mut socket,"protocol_error","invalid frontend frame").await; break; },
                    Err(_) => { refuse(&mut socket,"protocol_error","invalid frontend frame").await; break; }
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                _ => break,
            },
            reason = grant_ended(grant.as_ref()) => {
                close_policy(&mut socket, reason).await;
                break;
            }
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                let Ok(text) = outgoing.encode(&frame) else { refuse(&mut socket,"protocol_error","engine frame exceeds limit").await; break; };
            if socket.send(Message::Text(text.into())).with_subscriber(tracing::subscriber::NoSubscriber::default()).await.is_err() { break; }
            }
        }
    }
    session.disconnected();
}

#[cfg(test)]
mod tests {
    //! The real frontend socket loop over a real WebSocket, with a manual
    //! grant clock: an ended grant closes a waiting browser socket with
    //! 1008, and wins over a Hello that is ready at the same time.
    use super::*;
    use crate::command::instance::host::test_support::{host, RefusingOpener};
    use crate::data::annotations::Annotations;
    use crate::data::browser_auth::{BrowserScope, GrantRevocationReason, GRANT_WATCH_RECHECK};
    use crate::engine::auth::browser_grants::{
        BrowserGrantStore, GrantClock, ManualClock, SamplePoint,
    };
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

    struct Fixture {
        sandbox: awman_test_support::TestSandbox,
        clock: Arc<ManualClock>,
        grants: Arc<BrowserGrantStore>,
        opener: Arc<RefusingOpener>,
        host: Arc<EngineInstanceHost>,
        instance: InstanceId,
        servers: std::sync::Mutex<Vec<tokio::task::JoinHandle<std::io::Result<()>>>>,
    }

    fn fixture(id: &'static str) -> Fixture {
        let sandbox =
            awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(id));
        let clock = ManualClock::new();
        let path = sandbox.work().join("daemon").join("grants.json");
        let grants = BrowserGrantStore::open(path).unwrap();
        grants.set_clock(clock.clone());
        let opener = Arc::new(RefusingOpener::default());
        let host = host(opener.clone());
        let instance = host.create_instance(Annotations::new()).unwrap();
        Fixture {
            sandbox,
            clock,
            grants,
            opener,
            host,
            instance,
            servers: Default::default(),
        }
    }

    impl Fixture {
        /// Stop every server before the sandbox checks for leaks.
        async fn finish(self) {
            let servers = std::mem::take(&mut *self.servers.lock().unwrap());
            for server in servers {
                server.abort();
                let _ = server.await;
            }
        }

        fn grant(&self) -> GrantWatch {
            let (now, _) = self.clock.sample(SamplePoint::Ordinary);
            let (record, _) = self
                .grants
                .create(
                    "http://127.0.0.1:3000",
                    "l",
                    BrowserScope::Engine,
                    None,
                    now,
                )
                .unwrap();
            self.grants.watch(&record.id).unwrap()
        }

        /// Serve one browser socket under `grant` on a fresh port and
        /// connect to it. With `start`, the daemon side begins only once
        /// the test fires it (after the client has sent its first frame).
        async fn connect(
            &self,
            grant: GrantWatch,
            start: Option<tokio::sync::oneshot::Receiver<()>>,
        ) -> tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        > {
            let (host, id) = (self.host.clone(), self.instance.clone());
            let start = Arc::new(std::sync::Mutex::new(start));
            let app = axum::Router::new().route(
                "/ws",
                axum::routing::get(move |ws: WebSocketUpgrade| async move {
                    let start = start.lock().unwrap().take();
                    ws.protocols([FRONTEND_SUBPROTOCOL])
                        .on_upgrade(move |socket| async move {
                            if let Some(start) = start {
                                let _ = start.await;
                            }
                            serve_frontend_socket(socket, host, id, Some(grant)).await
                        })
                }),
            );
            let listener = self.sandbox.ports().tcp_listener();
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            self.servers.lock().unwrap().push(server);
            let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                FRONTEND_SUBPROTOCOL.parse().unwrap(),
            );
            tokio_tungstenite::connect_async(request).await.unwrap().0
        }
    }

    /// The first message the daemon sends, within `deadline`.
    async fn first_message(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        deadline: Duration,
    ) -> Option<tungstenite::Message> {
        tokio::time::timeout(deadline, ws.next())
            .await
            .ok()
            .flatten()
            .map(Result::unwrap)
    }

    fn policy_close(reason: &str) -> tungstenite::Message {
        tungstenite::Message::Close(Some(tungstenite::protocol::CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::Policy,
            reason: reason.to_owned().into(),
        }))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_suspend_past_expiry_closes_a_live_browser_socket_within_one_recheck() {
        let f = fixture("frontend::api::frontend_socket::tests::a_suspend_past_expiry_closes_a_live_browser_socket_within_one_recheck");
        let grant = f.grant();
        let mut ws = f.connect(grant, None).await;
        // The socket is open and waiting (no Hello yet, nothing sent).
        assert!(first_message(&mut ws, Duration::from_millis(200))
            .await
            .is_none());
        // Suspend past expiry (timers do not run), then one recheck interval.
        f.clock.jump_wall(chrono::Duration::hours(13));
        f.clock.advance(GRANT_WATCH_RECHECK);
        assert_eq!(
            first_message(&mut ws, Duration::from_secs(3)).await,
            Some(policy_close("grant expired")),
        );
        assert_eq!(f.opener.opened.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(ws);
        f.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revocation_after_upgrade_and_before_hello_closes_without_admission() {
        let f = fixture("frontend::api::frontend_socket::tests::revocation_after_upgrade_and_before_hello_closes_without_admission");
        let grant = f.grant();
        let id = grant.grant_id().clone();
        let mut ws = f.connect(grant, None).await;
        f.grants
            .delete(&id, GrantRevocationReason::Deleted)
            .unwrap();
        assert_eq!(
            first_message(&mut ws, Duration::from_secs(3)).await,
            Some(policy_close("grant revoked")),
        );
        let detail = f.host.get(&f.instance).unwrap();
        assert_eq!(detail.summary.primary_frontend_kind, None);
        drop(ws);
        f.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ended_grant_wins_over_a_hello_that_is_ready_at_the_same_time() {
        let f = fixture("frontend::api::frontend_socket::tests::an_ended_grant_wins_over_a_hello_that_is_ready_at_the_same_time");
        for _ in 0..20 {
            let grant = f.grant();
            let id = grant.grant_id().clone();
            f.grants
                .delete(&id, GrantRevocationReason::Deleted)
                .unwrap();
            let (start, started) = tokio::sync::oneshot::channel();
            let mut ws = f.connect(grant, Some(started)).await;
            // Not a valid Hello: if it were read first, the daemon would
            // answer `protocol_error` instead of the policy close. It is sent
            // before the daemon side starts, so both are ready at once.
            let _ = ws
                .send(tungstenite::Message::Text("not a hello".into()))
                .await;
            start.send(()).unwrap();
            assert_eq!(
                first_message(&mut ws, Duration::from_secs(3)).await,
                Some(policy_close("grant revoked")),
            );
        }
        assert_eq!(f.opener.opened.load(std::sync::atomic::Ordering::SeqCst), 0);
        f.finish().await;
    }
}
