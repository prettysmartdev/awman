//! Bounded, sequenced frontend WebSocket transport. Never log frame contents.
use super::error::ApiError;
use crate::{
    command::{daemon_api::DaemonApi, instance::host::EngineInstanceHost},
    data::{
        frontend_protocol::{Codec, FrontendFrame, FRONTEND_SUBPROTOCOL, MAX_FRAME_BYTES},
        ids::InstanceId,
    },
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
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
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
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
    ws.protocols([FRONTEND_SUBPROTOCOL])
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| serve_frontend_socket(socket, host, id))
}
async fn refuse(socket: &mut WebSocket, code: &str, message: &str) {
    let _ = socket
        .send(Message::Text(
            serde_json::json!({"code":code,"error":message})
                .to_string()
                .into(),
        ))
        .await;
    let _ = socket.send(Message::Close(None)).await;
}
pub(super) async fn serve_frontend_socket(
    mut socket: WebSocket,
    host: Arc<EngineInstanceHost>,
    id: InstanceId,
) {
    let mut incoming = Codec::new();
    // Transport libraries trace raw WebSocket payloads at TRACE. Hello
    // carries a process environment, so suppress payload-library diagnostics
    // while polling reads/writes; our own diagnostics contain frame types only.
    let first = tokio::time::timeout(
        Duration::from_secs(5),
        socket
            .recv()
            .with_subscriber(tracing::subscriber::NoSubscriber::default()),
    )
    .await;
    let Ok(Some(Ok(Message::Text(first)))) = first else {
        refuse(&mut socket, "protocol_error", "expected Hello").await;
        return;
    };
    let hello = match incoming.decode::<FrontendFrame>(&first) {
        Ok((_, FrontendFrame::Hello(hello))) => hello,
        _ => {
            refuse(&mut socket, "protocol_error", "expected Hello").await;
            return;
        }
    };
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
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                let Ok(text) = outgoing.encode(&frame) else { refuse(&mut socket,"protocol_error","engine frame exceeds limit").await; break; };
            if socket.send(Message::Text(text.into())).with_subscriber(tracing::subscriber::NoSubscriber::default()).await.is_err() { break; }
            }
        }
    }
    session.disconnected();
}
