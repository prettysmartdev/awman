//! `/v1/terminals` HTTP and WebSocket transport (WI 0141, Layer 3).
//!
//! `GET /v1/terminals?owner_id=…` lists; `GET /v1/terminals/{id}` returns the
//! record or, with `Upgrade: websocket` and the `awman.terminal.v1`
//! subprotocol, streams `TerminalFrame`s as JSON text messages and reads
//! `TerminalClientFrame`s. Every decision (admission, grant, resize rights)
//! is Layer 2's `TerminalSession`; this file only builds the viewer identity
//! from the connection, moves frames and maps `TerminalRefusal` onto the API
//! error table (`404 terminal_not_found`, `410 terminal_gone`,
//! `403 input_not_allowed`, `409 control_held`, `429 input_backpressure`).
//!
//! A refused client frame does not close the socket: the server answers
//! with one `{"type":"error","code":…,"error":…}` text message and the
//! viewer stays read-only. A malformed or binary client message closes the
//! socket with close code 1003/1007.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use axum::Json;
use tracing::instrument::WithSubscriber;

use crate::command::daemon_api::browser::{may_view_terminal, BrowserPrincipal};
use crate::command::daemon_api::terminals::{terminals_get, terminals_list, terminals_open};
use crate::command::daemon_api::{DaemonApi, Operation};
use crate::command::terminals::{TerminalRefusal, TerminalSession};
use crate::data::daemon_state::CommandListener;
use crate::data::ids::TerminalId;
use crate::data::terminal::{
    annotation_from_header, AnnotationError, Annotations, DetachReason, TerminalClientFrame,
    TerminalFrame, ViewerIdentity, ACTOR_HEADER, TERMINAL_SUBPROTOCOL,
};
use crate::data::wire::ListQuery;
use crate::engine::auth::browser_grants::GrantWatch;

use super::error::ApiError;

/// Header prefix carrying one subscription annotation each
/// (`x-awman-annotation-rex~block: blk_1`; see [`crate::data::terminal::annotation_header`]).
pub use crate::data::terminal::ANNOTATION_HEADER_PREFIX;

/// Longest `x-awman-actor` value accepted.
pub const MAX_ACTOR_BYTES: usize = 128;

/// Largest client message accepted (an `input` frame is base64).
pub const MAX_CLIENT_MESSAGE_BYTES: usize = 256 * 1024;

/// The router for the two terminal operations, or `None` for any other.
pub(super) fn route(op: Operation, filter: MethodFilter) -> Option<MethodRouter<Arc<DaemonApi>>> {
    match op {
        Operation::CoreTerminalsList => Some(axum::routing::on(filter, handle_list)),
        Operation::CoreTerminalsGet => Some(axum::routing::on(filter, handle_get)),
        _ => None,
    }
}

/// `TerminalRefusal` → the API error table. On the network listener a
/// terminal whose owner is not served there looks exactly like a missing
/// one, the way hidden routes do.
pub fn refusal_error(refusal: &TerminalRefusal) -> ApiError {
    match refusal {
        TerminalRefusal::NotFound | TerminalRefusal::NetworkRefused => {
            ApiError::terminal_not_found("terminal not found")
        }
        TerminalRefusal::NoTerminal(owner) => {
            ApiError::terminal_not_found(format!("{owner} has no terminal"))
        }
        TerminalRefusal::Gone => {
            ApiError::terminal_gone("terminal is gone; its owner may have been re-queued")
        }
        TerminalRefusal::Closed => ApiError::conflict("terminal has exited"),
        TerminalRefusal::InputNotAllowed => {
            ApiError::input_not_allowed("input is not allowed for this viewer")
        }
        TerminalRefusal::ControlHeld => {
            ApiError::control_held("another viewer holds terminal control")
        }
        TerminalRefusal::InputBackpressure => ApiError::input_backpressure(
            "terminal input queue is full; the agent is not reading input",
        ),
        TerminalRefusal::InvalidOwner(_) | TerminalRefusal::Wire(_) => {
            ApiError::invalid_request(refusal.to_string())
        }
    }
}

/// The viewer this connection presents: listener, `x-awman-actor`,
/// `x-awman-annotation-*` headers and `?read_only=true`.
fn viewer_identity(
    listener: CommandListener,
    headers: &HeaderMap,
    read_only: bool,
) -> Result<ViewerIdentity, ApiError> {
    let actor = match headers.get(ACTOR_HEADER) {
        None => None,
        Some(value) => {
            let actor = value
                .to_str()
                .ok()
                .map(str::trim)
                .filter(|a| !a.is_empty() && a.len() <= MAX_ACTOR_BYTES)
                .filter(|a| !a.chars().any(char::is_control))
                .ok_or_else(|| {
                    ApiError::invalid_request(format!(
                        "{ACTOR_HEADER} must be 1-{MAX_ACTOR_BYTES} printable bytes"
                    ))
                })?;
            Some(actor.to_owned())
        }
    };
    let mut annotations = Annotations::new();
    for (name, value) in headers {
        let Some((key, value)) =
            annotation_from_header(name.as_str(), value.as_bytes()).map_err(invalid_annotation)?
        else {
            continue;
        };
        annotations
            .insert(&key, &value)
            .map_err(invalid_annotation)?;
    }
    Ok(ViewerIdentity {
        principal: crate::data::daemon_state::Principal::Owner,
        listener_name: None,
        listener,
        // Local attaches are `local`; the header names network clients only.
        actor: (listener == CommandListener::Network)
            .then_some(actor)
            .flatten(),
        annotations,
        read_only,
    })
}

/// Resolves when a browser grant ends; never for other viewers.
async fn grant_ended(grant: Option<&GrantWatch>) -> &'static str {
    match grant {
        Some(grant) => grant.ended().await.close_reason(),
        None => std::future::pending().await,
    }
}

/// Set the authenticated principal. A browser viewer is an instance
/// frontend named by its grant (D13): `x-awman-actor` is ignored and the
/// input holder is `Actor(<grant id>)`.
fn browser_viewer(viewer: &mut ViewerIdentity, principal: crate::data::daemon_state::Principal) {
    if let Some(browser) = BrowserPrincipal::from_principal(&principal) {
        viewer.actor = Some(browser.grant_id.to_string());
    }
    viewer.principal = principal;
}

fn listener_of(listener: Option<axum::Extension<CommandListener>>) -> CommandListener {
    listener.map(|l| l.0).unwrap_or(CommandListener::Local)
}

fn flag(value: &str) -> Result<bool, ApiError> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ApiError::invalid_request("read_only must be true or false")),
    }
}

/// `GET /v1/terminals?owner_id=…&limit=&cursor=` → `{items, next_cursor}`.
async fn handle_list(
    State(api): State<Arc<DaemonApi>>,
    (listener, receiving, principal): (
        Option<axum::Extension<CommandListener>>,
        Option<axum::Extension<super::routes::ReceivingListener>>,
        axum::Extension<crate::data::daemon_state::Principal>,
    ),
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let mut owner_id = None;
    let mut page = ListQuery::default();
    for (key, value) in pairs {
        match key.as_str() {
            "owner_id" => owner_id = Some(value),
            "cursor" => page.cursor = Some(value),
            "limit" => match value.parse() {
                Ok(n) => page.limit = Some(n),
                Err(_) => {
                    return ApiError::invalid_request("limit must be an integer").into_response()
                }
            },
            _ => {}
        }
    }
    let mut viewer = match viewer_identity(listener_of(listener), &headers, false) {
        Ok(viewer) => viewer,
        Err(error) => return error.into_response(),
    };
    viewer.listener_name = receiving.map(|r| r.0 .0);
    browser_viewer(&mut viewer, principal.0);
    match terminals_list(&api.terminals, owner_id.as_deref(), &page, &viewer) {
        Ok(mut list) => {
            // A browser sees only terminals of resources its grant may use.
            if let Some(browser) = BrowserPrincipal::from_principal(&viewer.principal) {
                list.items
                    .retain(|record| may_view_terminal(&api, &browser, record));
            }
            Json(list).into_response()
        }
        Err(refusal) => refusal_error(&refusal).into_response(),
    }
}

/// `GET /v1/terminals/{id}`: the record, or the `awman.terminal.v1`
/// WebSocket when the request upgrades. Admission happens before the
/// upgrade, so a refused viewer gets an ordinary HTTP error.
async fn handle_get(
    State(api): State<Arc<DaemonApi>>,
    (listener, receiving, principal): (
        Option<axum::Extension<CommandListener>>,
        Option<axum::Extension<super::routes::ReceivingListener>>,
        axum::Extension<crate::data::daemon_state::Principal>,
    ),
    (shutdown, grant): (
        Option<axum::Extension<tokio_util::sync::CancellationToken>>,
        Option<axum::Extension<GrantWatch>>,
    ),
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    // A plain GET (no `Upgrade` header) is the record; a malformed upgrade
    // attempt gets axum's rejection.
    let upgrade = match upgrade {
        Ok(upgrade) => Some(upgrade),
        Err(_) if !headers.contains_key(axum::http::header::UPGRADE) => None,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(id) = TerminalId::parse(&id) else {
        return ApiError::terminal_not_found("terminal not found").into_response();
    };
    let mut read_only = false;
    for (key, value) in pairs {
        if key == "read_only" {
            match flag(&value) {
                Ok(v) => read_only = v,
                Err(error) => return error.into_response(),
            }
        }
    }
    let mut viewer = match viewer_identity(listener_of(listener), &headers, read_only) {
        Ok(viewer) => viewer,
        Err(error) => return error.into_response(),
    };
    viewer.listener_name = receiving.map(|r| r.0 .0);
    browser_viewer(&mut viewer, principal.0);
    // Browsers: ownership is checked on every read and again at upgrade
    // (the ticket was checked when it was issued).
    if let Some(browser) = BrowserPrincipal::from_principal(&viewer.principal) {
        if upgrade.is_some() && grant.is_none() {
            return ApiError::browser_ticket_invalid("the browser grant has ended").into_response();
        }
        let visible = api
            .terminals
            .hub()
            .get(&id)
            .ok()
            .is_some_and(|record| may_view_terminal(&api, &browser, &record));
        if !visible {
            return ApiError::terminal_not_found("terminal not found").into_response();
        }
    }
    let Some(upgrade) = upgrade else {
        return match terminals_get(&api.terminals, &id, &viewer) {
            Ok(record) => Json(record).into_response(),
            Err(refusal) => refusal_error(&refusal).into_response(),
        };
    };
    if !offers_subprotocol(&headers) {
        return ApiError::invalid_request(format!(
            "terminal WebSockets require the {TERMINAL_SUBPROTOCOL} subprotocol"
        ))
        .into_response();
    }
    let session = match terminals_open(&api.terminals, &id, viewer) {
        Ok(session) => session,
        Err(refusal) => return refusal_error(&refusal).into_response(),
    };
    let shutdown = shutdown.map(|s| s.0).unwrap_or_default();
    let grant = grant.map(|g| g.0);
    // Only the real subprotocol is selected; a ticket offer is never echoed.
    upgrade
        .protocols([TERMINAL_SUBPROTOCOL])
        .max_message_size(MAX_CLIENT_MESSAGE_BYTES)
        .max_frame_size(MAX_CLIENT_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve_terminal_socket(socket, session, shutdown, grant))
}

fn offers_subprotocol(headers: &HeaderMap) -> bool {
    headers
        .get_all(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == TERMINAL_SUBPROTOCOL)
}

/// The `error` message a refused client frame gets.
fn error_message(refusal: &TerminalRefusal) -> Message {
    let error = refusal_error(refusal);
    Message::Text(
        serde_json::json!({"type": "error", "code": error.code, "error": error.message})
            .to_string()
            .into(),
    )
}

fn frame_message(frame: &TerminalFrame) -> Message {
    Message::Text(
        serde_json::to_string(frame)
            .expect("terminal frames serialize")
            .into(),
    )
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
}

/// What one client message asks the loop to do.
enum ClientStep {
    Continue,
    Reply(Message),
    Detach,
    Close(u16, &'static str),
}

fn client_step(session: &mut TerminalSession, message: Message) -> ClientStep {
    let text = match message {
        Message::Text(text) => text,
        Message::Binary(_) => return ClientStep::Close(1003, "frames are JSON text messages"),
        Message::Ping(_) | Message::Pong(_) => return ClientStep::Continue,
        Message::Close(_) => return ClientStep::Detach,
    };
    let frame = match serde_json::from_str::<TerminalClientFrame>(text.as_str()) {
        Ok(frame) => frame,
        Err(_) => return ClientStep::Close(1007, "malformed awman.terminal.v1 frame"),
    };
    let detaching = matches!(frame, TerminalClientFrame::Detached { .. });
    match session.handle_client_frame(frame) {
        Ok(()) if detaching => ClientStep::Detach,
        Ok(()) => ClientStep::Continue,
        Err(refusal) => ClientStep::Reply(error_message(&refusal)),
    }
}

/// Drive one admitted session over its socket until either side closes.
/// Server frames are sent as text (`serde_json`); unknown or malformed client
/// messages close the socket with a protocol error. A socket that closes
/// without a `detached` frame counts as a hang-up.
pub(super) async fn serve_terminal_socket(
    mut socket: WebSocket,
    mut session: TerminalSession,
    shutdown: tokio_util::sync::CancellationToken,
    grant: Option<GrantWatch>,
) {
    loop {
        // Biased: when a grant ending or shutdown is ready together with
        // client input, the ending wins and no further input reaches the PTY.
        tokio::select! {
            biased;
            reason = grant_ended(grant.as_ref()) => {
                close(&mut socket, 1008, reason).await;
                break;
            }
            _ = shutdown.cancelled() => {
                close(&mut socket, 1001, "listener stopping").await;
                break;
            }
            frame = session.next_frame() => {
                let Some(frame) = frame else {
                    if session.access_revoked() {
                        // A live policy change stopped serving this viewer.
                        close(&mut socket, 1008, "terminal access revoked").await;
                        break;
                    }
                    // Exit was delivered; the terminal has nothing more.
                    close(&mut socket, 1000, "terminal exited").await;
                    break;
                };
                if socket.send(frame_message(&frame)).with_subscriber(tracing::subscriber::NoSubscriber::default()).await.is_err() {
                    break;
                }
            }
            message = socket.recv().with_subscriber(tracing::subscriber::NoSubscriber::default()) => {
                let Some(Ok(message)) = message else { break };
                match client_step(&mut session, message) {
                    ClientStep::Continue => {}
                    ClientStep::Reply(reply) => {
                        if socket.send(reply).with_subscriber(tracing::subscriber::NoSubscriber::default()).await.is_err() {
                            break;
                        }
                    }
                    ClientStep::Detach => {
                        close(&mut socket, 1000, "detached").await;
                        break;
                    }
                    ClientStep::Close(code, reason) => {
                        close(&mut socket, code, reason).await;
                        break;
                    }
                }
            }
        }
    }
    session.detach(DetachReason::Hangup);
}

fn invalid_annotation(error: AnnotationError) -> ApiError {
    let mut mapped = ApiError::invalid_request(error.to_string());
    mapped.code = AnnotationError::CODE;
    mapped
}
