//! Bounded, sequenced frontend WebSocket transport. Never log frame contents.
use super::error::ApiError;
use crate::engine::auth::browser_grants::GrantWatch;
use crate::{
    command::{daemon_api::DaemonApi, instance::host::EngineInstanceHost},
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
    let browser = matches!(
        principal,
        crate::data::daemon_state::Principal::Browser { .. }
    );
    if browser && grant.is_none() {
        return ApiError::browser_ticket_invalid("the browser grant has ended").into_response();
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
    let grant = grant.map(|g| g.0);
    ws.protocols([FRONTEND_SUBPROTOCOL])
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| serve_frontend_socket(socket, host, id, grant, browser))
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
    browser: bool,
) {
    let mut incoming = Codec::new();
    // Transport libraries trace raw WebSocket payloads at TRACE. Hello
    // carries a process environment, so suppress payload-library diagnostics
    // while polling reads/writes; our own diagnostics contain frame types only.
    let first = tokio::select! {
        first = tokio::time::timeout(
            Duration::from_secs(5),
            socket
                .recv()
                .with_subscriber(tracing::subscriber::NoSubscriber::default()),
        ) => first,
        reason = grant_ended(grant.as_ref()) => {
            close_policy(&mut socket, reason).await;
            return;
        }
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
    let session = match host.attach_frontend(id, hello, tx).await {
        Ok(session) => session,
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
