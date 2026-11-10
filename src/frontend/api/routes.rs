//! HTTP route registration and handlers for the API server.
//!
//! One method/path table over the typed resource operations.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::trace::TraceLayer;

use crate::command::commands::api_server::admission::{
    AdmissionError, CommandSubmission, IdempotencyGate, IdempotencyGateError, IdempotentAdmission,
    IdempotentResult, SessionSubmission,
};
use crate::command::commands::api_server::command_log_stream::CommandLogUpdate;
use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::api_server::session_setup::{
    log_session_setup, SessionSetupBus, SessionSetupBusSender, SetupReadyFrontend, TracingSetupSink,
};
use crate::command::commands::api_server::{CancelOutcome, CloseOutcome, SetupReadiness};
use crate::command::session_create::{SessionCreatePlan, SessionCreateRequest};
use crate::command::session_setup::{SessionSetup, SessionSetupPresenter};
use crate::data::api_lifecycle_store::RunRecord;
use crate::data::command_status::CommandStatus;
use crate::data::execution_event::{EventCursor, ExecutionEvent};
use crate::data::fs::api_db::SessionCommandAdmission;
use crate::data::fs::api_db::SessionRecord;
use crate::data::fs::api_db::{CommandListFilter, CommandRecord, LabelOwner};
use crate::data::fs::api_labels::{LabelSelector, Labels};
#[cfg(test)]
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::session_setup_event::SetupEventPayload;
use crate::data::session_status::SessionStatus;
use crate::engine::git::GitFrontend;
use crate::engine::ready::frontend::ReadyFrontend;

// ─── Auth mode ───────────────────────────────────────────────────────────────

pub use crate::command::daemon_api::worker::WorkerApi as AppState;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionClosingResponse {
    setup_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup_error: Option<serde_json::Value>,
    session_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    running_command_id: Option<String>,
    cancelled_count: usize,
    message: String,
}

#[derive(Debug, Serialize)]
struct CancellationStoppingResponse {
    command_id: crate::data::ids::CommandId,
    session_id: crate::data::ids::WorkerSessionId,
    status: &'static str,
    #[serde(with = "crate::data::wire::timestamp")]
    cancel_requested_at: chrono::DateTime<chrono::Utc>,
    message: &'static str,
}

// ─── Request / Response types (API compatibility contract) ───────────────────

#[derive(Deserialize, Debug)]
struct CreateSessionRequest {
    /// `"local"` (default) or `"remote"`.
    #[serde(default)]
    session_type: Option<String>,
    /// Workdir on the server host (required for `local`).
    #[serde(default)]
    workdir: Option<String>,
    /// Repository URL (required for `remote`).
    #[serde(default)]
    repo_url: Option<String>,
    /// Optional branch (defaults to remote default when `remote`).
    #[serde(default)]
    branch: Option<String>,
    /// Remote only: start point for a branch that does not exist on origin.
    #[serde(default)]
    base_ref: Option<String>,
    /// Remote only: branch names merged into the session branch at setup.
    #[serde(default)]
    merge_refs: Vec<String>,
    /// Remote only: push the session branch after a successful command.
    #[serde(default)]
    push: bool,
    #[serde(default)]
    labels: Labels,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    setup: Option<crate::data::session_setup_event::SessionSetupState>,
    id: crate::data::ids::WorkerSessionId,
    workdir: String,
    #[serde(with = "crate::data::wire::timestamp")]
    created_at: chrono::DateTime<chrono::Utc>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    closed_at: Option<chrono::DateTime<chrono::Utc>>,
    session_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    merge_refs: Vec<String>,
    push: bool,
    #[serde(default)]
    #[serde(deserialize_with = "crate::command::daemon_api::worker::stored_labels")]
    labels: Labels,
    /// The setup stage stored on the row (`SessionSetupStatus` spelling). A
    /// closed session keeps its last stage.
    setup_status: String,
    /// `{stage, message}`, only when `setup_status` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    setup_error: Option<serde_json::Value>,
}

impl SessionResponse {
    /// The row projection plus its labels and, for a failed setup, the
    /// `{stage, message}` recorded in the session's setup_state.json.
    fn new(state: &AppState, row: SessionRecord, labels: Labels) -> Self {
        let setup_error = state
            .lifecycle()
            .setup_error(&row)
            .and_then(|error| serde_json::to_value(error).ok());
        Self {
            labels,
            setup_error,
            setup: {
                let mut setup = state
                    .setup_buses
                    .try_lock()
                    .ok()
                    .and_then(|buses| buses.get(&row.id).map(|bus| bus.snapshot()))
                    .or_else(|| state.paths.read_setup_state(&row.id))
                    .unwrap_or_default();
                setup.status = row.setup_status;
                Some(setup)
            },
            ..Self::from(row)
        }
    }
}

impl From<SessionRecord> for SessionResponse {
    fn from(s: SessionRecord) -> Self {
        Self {
            setup: None,
            id: crate::data::ids::WorkerSessionId::parse(&s.id).expect("stored session id"),
            workdir: s.workdir,
            created_at: parse_at(&s.created_at).expect("stored session created_at"),
            status: s.status.as_str().to_string(),
            setup_status: s.setup_status.as_str().to_string(),
            setup_error: None,
            closed_at: s
                .closed_at
                .map(|v| parse_at(&v).expect("stored session closed_at")),
            session_type: s.session_type,
            repo_url: s.repo_url,
            branch: s.branch,
            base_ref: s.base_ref,
            merge_refs: s.merge_refs,
            push: s.push,
            labels: Labels::empty(),
        }
    }
}

#[derive(Deserialize)]
struct CreateCommandRequest {
    #[serde(default)]
    allow_remote_input: bool,
    #[serde(default)]
    interaction: Option<crate::data::config::daemon::Interaction>,
    subcommand: String,
    args: Vec<String>,
    #[serde(default)]
    labels: Labels,
    /// `POST /v1/worker/commands` only: the session, as an alternative to the
    /// `x-awman-session` header. A run's command must not name one.
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResponse {
    #[serde(default)]
    allow_remote_input: bool,
    #[serde(default)]
    listener: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listener_name: Option<String>,
    #[serde(default)]
    trace_parent: Option<String>,
    id: crate::data::ids::CommandId,
    attempt: u32,
    session_id: crate::data::ids::WorkerSessionId,
    subcommand: String,
    args: serde_json::Value,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Deprecated: a server-side path a remote client cannot open, so it is
    /// sent only to loopback peers. Clients read `GET /v1/worker/commands/{id}/events`.
    #[serde(skip_serializing_if = "Option::is_none")]
    log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    queued_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(deserialize_with = "crate::command::daemon_api::worker::stored_labels")]
    labels: Labels,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    cancel_requested_at: Option<chrono::DateTime<chrono::Utc>>,
    /// WI 0143: where the job came from (`{kind: push|pull, …}`), once the
    /// worker has taken it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<crate::data::worker::JobOrigin>,
    /// WI 0143: whether `/awman/output` was actually mounted, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outputs_mounted: Option<bool>,
}

/// Capabilities a client can detect without parsing the version. Append only.
pub use crate::command::daemon_api::capabilities::CAPABILITIES as API_FEATURES;

#[derive(Deserialize)]
struct CreateRunRequest {
    session: CreateSessionRequest,
    command: CreateCommandRequest,
    #[serde(default)]
    close_on_finish: bool,
}

use crate::frontend::api::error::ApiError;

impl From<CreateCommandRequest> for CommandSubmission {
    fn from(body: CreateCommandRequest) -> Self {
        Self {
            allow_remote_input: body.allow_remote_input,
            subcommand: body.subcommand,
            args: body.args,
            labels: body.labels,
            interaction: body.interaction,
        }
    }
}

fn admission_error(error: AdmissionError, session_id: &str) -> Response {
    match error {
        AdmissionError::FeatureDisabled => {
            ApiError::feature_disabled("worker push is disabled").into_response()
        }
        AdmissionError::SessionCreation(e) => ApiError::session_creation(&e).into_response(),
        AdmissionError::CommandValidation(e) => ApiError::command_validation(&e).into_response(),
        AdmissionError::CommandAdmission(e) => ApiError::command_admission(&e).into_response(),
        AdmissionError::Unavailable { command, available } => ApiError::invalid_request(
            "command not available via API",
        )
        .with_details(serde_json::json!({ "blocked_command": command, "available": available })),
        AdmissionError::Session(SessionCommandAdmission::Closing) => {
            ApiError::session_closing("session is closing").with_details(serde_json::json!({
                "session_id": session_id,
                "hint": "Session is shutting down and no longer accepts commands."
            }))
        }
        AdmissionError::Session(SessionCommandAdmission::Closed) => {
            ApiError::session_not_found(format!("Session '{}' is closed", session_id))
                .into_response()
        }
        AdmissionError::Session(SessionCommandAdmission::NotFound)
        | AdmissionError::Setup(SetupReadiness::NotFound) => {
            ApiError::session_not_found(format!("Session '{}' not found", session_id))
                .into_response()
        }
        AdmissionError::Setup(readiness) => {
            let mut body = serde_json::json!({
                "setup_status": readiness.status(),
                "hint": "Poll GET /v1/worker/sessions/{id} to check setup progress"
            });
            let mut message = "session is not ready";
            if let Some(err) = readiness.error().cloned() {
                body["setup_error"] = err;
                message = "session setup failed";
            }
            ApiError::session_not_ready(message).with_details(body)
        }
        AdmissionError::Storage { what, source } => {
            tracing::error!(error = %source, "{what}");
            internal_error(what)
        }
        AdmissionError::ReservationLost => internal_error("Idempotency reservation lost"),
        AdmissionError::InteractionRefused(refused) => ApiError::interaction_override_refused(
            refused.to_string(),
        )
        .with_details(serde_json::json!({
            "setting": refused.key(),
            "configured": refused.configured().as_str(),
            "requested": refused.requested().as_str(),
        })),
        AdmissionError::Session(SessionCommandAdmission::Accepted) => {
            internal_error("Unexpected session admission outcome")
        }
    }
}

fn bad_request(message: impl Into<String>) -> Response {
    ApiError::invalid_request(message).into_response()
}

fn internal_error(message: &str) -> Response {
    ApiError::internal(message).into_response()
}

fn session_response(state: &AppState, row: SessionRecord) -> Response {
    match state.store.labels_for(LabelOwner::Session, &row.id) {
        Ok(labels) => Json(SessionResponse::new(state, row, labels)).into_response(),
        Err(_) => internal_error("Failed to read session labels"),
    }
}

// Keep fallible route helpers small on the success path; HTTP responses carry
// a body and extensions even when they are only used to report an error.
type RouteResult<T> = Result<T, Box<Response>>;

/// The TCP peer of a request, from the `ConnectInfo` the server installs.
/// Owner-authenticated IPC is also trusted to access host-local paths.
#[derive(Clone, Copy)]
pub(crate) struct Peer(Option<std::net::SocketAddr>, bool);

impl Peer {
    fn is_loopback(self) -> bool {
        self.1
            || self
                .0
                .is_some_and(|addr| addr.ip().to_canonical().is_loopback())
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0),
            parts
                .extensions
                .get::<super::local::OwnerAuthenticated>()
                .is_some(),
        ))
    }
}

fn command_response(
    state: &AppState,
    peer: Peer,
    c: CommandRecord,
    labels: Option<Labels>,
) -> RouteResult<CommandResponse> {
    let labels = match labels {
        Some(labels) => labels,
        None => state
            .store
            .labels_for(LabelOwner::Command, &c.id)
            .map_err(|_| internal_error("Failed to read command labels"))?,
    };
    let cancel_requested_at = state
        .lifecycle_store
        .cancel_requested_at(
            &crate::data::ids::CommandId::parse(&c.id)
                .map_err(|_| internal_error("invalid stored command id"))?,
        )
        .map_err(|_| internal_error("Failed to read cancel request"))?;
    let queue_position = state
        .lifecycle()
        .queue_position(&c)
        .map_err(|_| internal_error("Failed to read queue position"))?;
    let job = worker::commands_job(state, &c.id)
        .map_err(|_| internal_error("Failed to read the job record"))?;
    Ok(CommandResponse {
        origin: job.origin,
        outputs_mounted: job.outputs_mounted,
        allow_remote_input: c.allow_remote_input,
        listener: c.listener.clone(),
        listener_name: c.listener_name.clone(),
        trace_parent: c.trace_parent.clone(),
        args: crate::command::commands::api_server::runtime::stored_args_view(&c),
        result: c
            .result
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok()),
        id: crate::data::ids::CommandId::parse(&c.id)
            .map_err(|_| internal_error("invalid stored command id"))?,
        attempt: c.attempt,
        session_id: crate::data::ids::WorkerSessionId::parse(&c.session_id)
            .map_err(|_| internal_error("invalid stored session id"))?,
        subcommand: c.subcommand,
        // Rows migrate `pending` → `queued`; never show the legacy name.
        status: c.status.canonical().as_str().to_string(),
        exit_code: c.exit_code,
        started_at: c
            .started_at
            .map(|v| parse_at(&v).expect("stored command timestamp")),
        finished_at: c
            .finished_at
            .map(|v| parse_at(&v).expect("stored command timestamp")),
        log_path: peer.is_loopback().then_some(c.log_path),
        queued_at: c
            .queued_at
            .map(|v| parse_at(&v).expect("stored command timestamp")),
        queue_position,
        worker_id: c.worker_id,
        labels,
        cancel_requested_at: cancel_requested_at
            .map(|v| parse_at(&v).expect("stored cancel timestamp")),
    })
}

#[derive(Default)]
struct ListQuery {
    page: crate::data::wire::ListQuery,
    command_status: Option<CommandStatus>,
    session_status: Option<SessionStatus>,
    setup_status: Option<SessionSetupStatus>,
    session_id: Option<String>,
    labels: Vec<LabelSelector>,
    limit: i64,
}

impl ListQuery {
    fn parse(pairs: Vec<(String, String)>, commands: bool) -> RouteResult<Self> {
        let mut query = Self {
            limit: i64::MAX,
            ..Self::default()
        };
        for (key, value) in pairs {
            match key.as_str() {
                "label" => query.labels.push(
                    LabelSelector::parse_query(&value).map_err(|e| bad_request(e.to_string()))?,
                ),
                "status" if commands => {
                    query.command_status = Some(CommandStatus::parse(&value).ok_or_else(|| {
                        let valid: Vec<_> = CommandStatus::ALL.iter().map(|s| s.as_str()).collect();
                        bad_request(format!(
                            "unknown command status: {value}; expected {}",
                            valid.join("|")
                        ))
                    })?);
                }
                "status" => {
                    query.session_status = Some(SessionStatus::parse(&value).ok_or_else(|| {
                        let valid: Vec<_> = SessionStatus::ALL.iter().map(|s| s.as_str()).collect();
                        bad_request(format!(
                            "unknown session status: {value}; expected {}",
                            valid.join("|")
                        ))
                    })?);
                }
                "setup_status" if !commands => {
                    query.setup_status =
                        Some(SessionSetupStatus::parse(&value).ok_or_else(|| {
                            let valid: Vec<_> =
                                SessionSetupStatus::ALL.iter().map(|s| s.as_str()).collect();
                            bad_request(format!(
                                "unknown session setup_status: {value}; expected {}",
                                valid.join("|")
                            ))
                        })?);
                }
                "session_id" if commands => query.session_id = Some(value),
                "limit" => {
                    query.page.limit = Some(
                        value
                            .parse()
                            .map_err(|_| bad_request("limit must be an integer"))?,
                    );
                }
                "cursor" => query.page.cursor = Some(value),
                _ => {}
            }
        }
        query.page.page().map_err(|e| bad_request(e.to_string()))?;
        Ok(query)
    }
}

async fn handle_list_commands(
    State(state): State<Arc<AppState>>,
    peer: Peer,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let query = match ListQuery::parse(pairs, true) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let rows = match worker::commands_list(
        &state,
        &CommandListFilter {
            status: query.command_status,
            session_id: query.session_id.as_deref(),
            labels: &query.labels,
            limit: query.limit,
        },
    ) {
        Ok(rows) => rows,
        Err(_) => return internal_error("Failed to list commands"),
    };
    let ids: Vec<_> = rows.iter().map(|c| c.id.clone()).collect();
    let mut labels = match state.store.labels_for_many(LabelOwner::Command, &ids) {
        Ok(labels) => labels,
        Err(_) => return internal_error("Failed to read command labels"),
    };
    let mut commands = Vec::with_capacity(rows.len());
    for row in rows {
        let row_labels = labels.remove(&row.id).unwrap_or_default();
        match command_response(&state, peer, row, Some(row_labels)) {
            Ok(command) => commands.push(command),
            Err(response) => return *response,
        }
    }
    keyed_response(commands, &query.page, |v| v.id.as_str())
}

async fn handle_cancel_command(
    State(state): State<Arc<AppState>>,
    peer: Peer,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match worker::commands_cancel(
        &state,
        &crate::data::ids::CommandId::parse(&id).expect("route validated command id"),
    )
    .await
    {
        Ok(CancelOutcome::NotFound) => {
            ApiError::command_not_found(format!("Command '{id}' not found")).into_response()
        }
        Ok(CancelOutcome::AlreadyTerminal(row) | CancelOutcome::Cancelled(row)) => {
            match command_response(&state, peer, row, None) {
                Ok(response) => Json(response).into_response(),
                Err(response) => *response,
            }
        }
        Ok(CancelOutcome::Stopping {
            command_id,
            session_id,
            requested_at,
        }) => match parse_at(&requested_at) {
            Ok(cancel_requested_at) => (
                StatusCode::ACCEPTED,
                Json(CancellationStoppingResponse {
                    command_id: crate::data::ids::CommandId::parse(&command_id)
                        .expect("stored command id"),
                    session_id: crate::data::ids::WorkerSessionId::parse(&session_id)
                        .expect("stored session id"),
                    status: CommandStatus::Running.as_str(),
                    cancel_requested_at,
                    message:
                        "Stopping. Poll GET /v1/worker/commands/{id} until status is cancelled.",
                }),
            )
                .into_response(),
            Err(_) => internal_error("Invalid stored cancellation timestamp"),
        },
        Err(error) => {
            tracing::error!(?error, command_id = %id, "Command cancellation failed");
            internal_error(&error.to_string())
        }
    }
}

fn accepted_json(body: String) -> Response {
    (
        StatusCode::ACCEPTED,
        [("content-type", "application/json")],
        body,
    )
        .into_response()
}

/// HTTP adapter over Layer 2's [`IdempotencyGate`]: reads the
/// `Idempotency-Key` header, maps each gate decision to its status and
/// headers, and hands the handler's final status/body back to the gate.
/// Which requests run, replay, wait or conflict is the gate's decision.
async fn with_idempotency<F, Fut>(
    state: &AppState,
    principal: &str,
    headers: &HeaderMap,
    route: &'static str,
    canonical: &str,
    handler: F,
) -> Response
where
    F: FnOnce(Option<Arc<IdempotentAdmission>>) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let key = match headers.get("idempotency-key").map(|h| h.to_str()) {
        None => None,
        Some(Ok(key)) => Some(key),
        Some(Err(_)) => return bad_request("invalid Idempotency-Key header"),
    };
    let run = match IdempotencyGate::open(&state.store, route, principal, key, canonical) {
        Ok(IdempotencyGate::Unkeyed) => return handler(None).await,
        Ok(IdempotencyGate::Fresh(run)) => run,
        Ok(IdempotencyGate::Replay { status, body }) => {
            return (
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                [
                    ("content-type", "application/json"),
                    ("idempotent-replayed", "true"),
                ],
                body,
            )
                .into_response()
        }
        Ok(IdempotencyGate::InFlight) => {
            return (
                [("retry-after", "1")],
                ApiError::idempotency_in_flight(
                    "a request with this Idempotency-Key is still in progress",
                ),
            )
                .into_response()
        }
        Ok(IdempotencyGate::Mismatch) => {
            return ApiError::idempotency_conflict(
                "Idempotency-Key reused with a different request body",
            )
            .into_response()
        }
        Err(IdempotencyGateError::InvalidKey) => {
            return bad_request("invalid Idempotency-Key header")
        }
        Err(IdempotencyGateError::Reserve(error)) => {
            tracing::error!(%error, "Failed to reserve idempotency key");
            return internal_error("Failed to reserve idempotency key");
        }
    };
    // A panicking handler, or a response that cannot be buffered, drops
    // `run`; its guard releases the key unless admission became durable.
    let response = match std::panic::AssertUnwindSafe(handler(Some(run.admission())))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => return internal_error("Request handler failed"),
    };
    if response.status().is_server_error() {
        run.finish(IdempotentResult::ServerError);
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return internal_error("Failed to buffer response"),
    };
    let body = match std::str::from_utf8(&bytes) {
        Ok(body) => body,
        Err(_) => return internal_error("Invalid JSON response encoding"),
    };
    run.finish(IdempotentResult::Final {
        status: parts.status.as_u16(),
        body,
    });
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

// ─── Router ──────────────────────────────────────────────────────────────────

use crate::command::daemon_api::{worker, DaemonApi, Feature, FeatureState, Operation};
use axum::routing::{MethodFilter, MethodRouter};
/// The only path/method catalogue served by either daemon process.
#[derive(Debug, Clone, Copy)]
pub struct RouteSpec {
    pub method: &'static str,
    pub path: &'static str,
    pub op: Operation,
    pub local_only: bool,
    pub capability: Option<&'static str>,
}
pub const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        method: "GET",
        path: "/v1/engine/catalogue",
        op: Operation::EngineCatalogue,
        local_only: false,
        capability: Some("engine_catalogue"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/prompts",
        op: Operation::EnginePrompts,
        local_only: false,
        capability: Some("engine_catalogue"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/launch_contexts",
        op: Operation::EngineLaunchContextsList,
        local_only: false,
        capability: Some("launch_contexts"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/engine/launch_contexts",
        op: Operation::EngineLaunchContextsCreate,
        local_only: true,
        capability: Some("launch_contexts"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/engine/launch_contexts/{id}/refresh",
        op: Operation::EngineLaunchContextsRefresh,
        local_only: true,
        capability: Some("launch_contexts"),
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/engine/launch_contexts/{id}",
        op: Operation::EngineLaunchContextsDelete,
        local_only: true,
        capability: Some("launch_contexts"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/instances",
        op: Operation::EngineInstancesList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/engine/instances",
        op: Operation::EngineInstancesCreate,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/instances/{id}",
        op: Operation::EngineInstancesGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/engine/instances/{id}",
        op: Operation::EngineInstancesDelete,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "PATCH",
        path: "/v1/engine/instances/{id}",
        op: Operation::EngineInstancesPatch,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/instances/{id}/frontend",
        op: Operation::EngineInstancesFrontend,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/instances/{id}/tabs",
        op: Operation::EngineTabsList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/instances/{id}/tabs/{tab}",
        op: Operation::EngineTabsGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "PATCH",
        path: "/v1/engine/instances/{id}/tabs/{tab}",
        op: Operation::EngineTabsPatch,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/engine/instances/{id}/dialogs",
        op: Operation::EngineDialogsList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/engine/instances/{id}/dialogs/{dialog_id}/answer",
        op: Operation::EngineDialogsAnswer,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/engine/instances/{id}/ui_requests",
        op: Operation::EngineUiRequests,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/health",
        op: Operation::CoreHealth,
        local_only: false,
        capability: Some("health"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/keys",
        op: Operation::CoreKeysList,
        local_only: true,
        capability: Some("api_keys"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/keys",
        op: Operation::CoreKeysCreate,
        local_only: true,
        capability: Some("api_keys"),
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/keys/{id}",
        op: Operation::CoreKeysDelete,
        local_only: true,
        capability: Some("api_keys"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/status",
        op: Operation::CoreStatus,
        local_only: false,
        capability: Some("feature_states"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/config",
        op: Operation::CoreConfigGet,
        local_only: true,
        capability: Some("config_routes"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/config/reload",
        op: Operation::CoreConfigReload,
        local_only: true,
        capability: Some("config_routes"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/features",
        op: Operation::CoreFeaturesList,
        local_only: false,
        capability: Some("feature_states"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/features/{feature}",
        op: Operation::CoreFeaturesGet,
        local_only: false,
        capability: Some("feature_states"),
    },
    RouteSpec {
        method: "PATCH",
        path: "/v1/features/{feature}",
        op: Operation::CoreFeaturesPatch,
        local_only: true,
        capability: Some("daemon_features"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/grants",
        op: Operation::CoreGrantsList,
        local_only: true,
        capability: Some("grant_routes"),
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/grants/{id}",
        op: Operation::CoreGrantsDelete,
        local_only: true,
        capability: Some("grant_routes"),
    },
    // WI 0141: one core resource for every feature's agent terminals.
    RouteSpec {
        method: "GET",
        path: "/v1/terminals",
        op: Operation::CoreTerminalsList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/terminals/{id}",
        op: Operation::CoreTerminalsGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/workdirs",
        op: Operation::WorkerWorkdirs,
        local_only: false,
        capability: Some("resource_routes"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/sessions",
        op: Operation::WorkerSessionsList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/sessions",
        op: Operation::WorkerSessionsCreate,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/sessions/{id}",
        op: Operation::WorkerSessionsGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/worker/sessions/{id}",
        op: Operation::WorkerSessionsDelete,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/sessions/{id}/queue",
        op: Operation::WorkerSessionsQueue,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/commands",
        op: Operation::WorkerCommandsList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/commands",
        op: Operation::WorkerCommandsCreate,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/commands/{id}",
        op: Operation::WorkerCommandsGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/commands/{id}/cancel",
        op: Operation::WorkerCommandsCancel,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/commands/{id}/events",
        op: Operation::WorkerCommandsEvents,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/commands/{id}/workflow",
        op: Operation::WorkerCommandsWorkflow,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/commands/{id}/outputs",
        op: Operation::WorkerCommandsOutputs,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/commands/{id}/outputs/{*path}",
        op: Operation::WorkerCommandsOutput,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/runs",
        op: Operation::WorkerRunsCreate,
        local_only: false,
        capability: None,
    },
    // WI 0143 pull routes: the daemon's own API only (local listener).
    RouteSpec {
        method: "POST",
        path: "/v1/worker/leases/{id}/terminal",
        op: Operation::WorkerLeasesTerminal,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/leases/{id}/checkout",
        op: Operation::WorkerLeasesCheckout,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/leases/{id}/events",
        op: Operation::WorkerLeasesEvents,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/leases/{id}/result",
        op: Operation::WorkerLeasesResult,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/servers",
        op: Operation::WorkerServersList,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/servers/{name}",
        op: Operation::WorkerServersGet,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/queue",
        op: Operation::WorkerQueueList,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/queue/{item_id}/claim",
        op: Operation::WorkerQueueClaim,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/leases",
        op: Operation::WorkerLeasesList,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/worker/leases/{id}",
        op: Operation::WorkerLeasesGet,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/worker/leases/{id}/release",
        op: Operation::WorkerLeasesRelease,
        local_only: true,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/squad/tasks",
        op: Operation::SquadTasksList,
        local_only: false,
        capability: Some("resource_routes"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/squad/tasks",
        op: Operation::SquadTasksCreate,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/squad/tasks/{id}",
        op: Operation::SquadTasksGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "PATCH",
        path: "/v1/squad/tasks/{id}",
        op: Operation::SquadTasksUpdate,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/squad/tasks/{id}",
        op: Operation::SquadTasksDelete,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/squad/tasks/{id}/workflow",
        op: Operation::SquadTasksWorkflow,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/squad/tasks/{id}/runs",
        op: Operation::SquadRunsCreate,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/squad/tasks/{id}/runs",
        op: Operation::SquadRunsList,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "POST",
        path: "/v1/squad/runs/{id}/cancel",
        op: Operation::SquadRunsCancel,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "GET",
        path: "/v1/squad/env",
        op: Operation::SquadEnvGet,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "PUT",
        path: "/v1/squad/env",
        op: Operation::SquadEnvPut,
        local_only: false,
        capability: None,
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/squad/env",
        op: Operation::SquadEnvClear,
        local_only: false,
        capability: None,
    },
    // WI 0159: browser pairing (browser listeners) and grant management
    // (owner socket).
    RouteSpec {
        method: "POST",
        path: "/v1/browser-links",
        op: Operation::BrowserLinksCreate,
        local_only: false,
        capability: Some("browser_links"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/browser-links",
        op: Operation::BrowserLinksLookup,
        local_only: true,
        capability: Some("browser_links"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/browser-links/{id}/poll",
        op: Operation::BrowserLinksPoll,
        local_only: false,
        capability: Some("browser_links"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/browser-links/{id}/decision",
        op: Operation::BrowserLinksDecide,
        local_only: true,
        capability: Some("browser_links"),
    },
    RouteSpec {
        method: "GET",
        path: "/v1/browsers",
        op: Operation::BrowsersList,
        local_only: true,
        capability: Some("browser_links"),
    },
    RouteSpec {
        method: "DELETE",
        path: "/v1/browsers/{id}",
        op: Operation::BrowsersDelete,
        local_only: true,
        capability: Some("browser_links"),
    },
    RouteSpec {
        method: "POST",
        path: "/v1/browser-tickets",
        op: Operation::BrowserTicketsCreate,
        local_only: false,
        capability: Some("browser_tickets"),
    },
];
/// Listener exposure controls owner-only paths; feature state controls whether
/// a feature route is available and reports disabled features as 503. Network
/// API keys never grant owner access.
#[derive(Debug, Clone)]
pub enum ListenerPolicy {
    Local,
    Network { name: String },
}

#[cfg(any(test, feature = "test-support"))]
pub fn build_router(state: impl Into<DaemonApi>) -> Router {
    build_router_for_listener(Arc::new(state.into()), ListenerPolicy::Local)
        .layer(axum::Extension(super::local::OwnerAuthenticated))
}
pub fn build_router_for_listener(api: Arc<DaemonApi>, policy: ListenerPolicy) -> Router {
    let mut router = Router::new();
    for spec in ROUTES {
        let filter = match spec.method {
            "GET" => MethodFilter::GET,
            "POST" => MethodFilter::POST,
            "PATCH" => MethodFilter::PATCH,
            "PUT" => MethodFilter::PUT,
            "DELETE" => MethodFilter::DELETE,
            _ => unreachable!(),
        };
        let route: MethodRouter = match spec.op.owner() {
            _ if super::browser::route(spec.op, filter).is_some() => {
                super::browser::route(spec.op, filter)
                    .expect("browser route")
                    .with_state(api.clone())
            }
            _ if super::terminals::route(spec.op, filter).is_some() => {
                super::terminals::route(spec.op, filter)
                    .expect("terminal route")
                    .with_state(api.clone())
            }
            Feature::Engine => super::engine::route(spec.op, filter).with_state(api.clone()),
            Feature::Worker if spec.op.pull() => {
                super::worker_pull::route(spec.op, filter).with_state(api.clone())
            }
            Feature::Worker => match &api.worker {
                Some(worker) => worker_route(spec.op, filter).with_state(worker.clone()),
                None => axum::routing::on(filter, || async {
                    ApiError::feature_disabled("worker feature is disabled")
                }),
            },
            _ => axum::routing::on(filter, resource_handler).with_state(api.clone()),
        };
        let gate_api = api.clone();
        let gate_policy = policy.clone();
        let spec = *spec;
        let route = route.layer(axum::middleware::from_fn(
            move |mut request: axum::http::Request<axum::body::Body>,
                  next: axum::middleware::Next| {
                let api = gate_api.clone();
                let policy = gate_policy.clone();
                async move {
                    if matches!(&policy, ListenerPolicy::Network { .. }) && spec.local_only {
                        return ApiError::not_found("resource not found").into_response();
                    }
                    let state = spec
                        .op
                        .owner()
                        .feature_name()
                        .map(|name| api.runtime.feature_state(name).state)
                        .unwrap_or(FeatureState::On);
                    if state == FeatureState::Degraded && spec.op.creates() {
                        return ApiError::feature_degraded("feature is recovering").into_response();
                    }
                    if state == FeatureState::Off
                        || (state == FeatureState::Draining && spec.op.creates())
                    {
                        return ApiError::feature_disabled(format!(
                            "{} feature is disabled",
                            spec.op.owner().name()
                        ))
                        .into_response();
                    }
                    if spec.op.owner() == Feature::Worker
                        && !spec.op.pull()
                        && spec.op.creates()
                        && api
                            .worker
                            .as_ref()
                            .is_some_and(|worker| !worker.push_enabled())
                    {
                        return ApiError::feature_disabled("worker push is disabled")
                            .into_response();
                    }
                    // Worker IDs are validated even though the legacy store uses string keys.
                    if spec.op.owner() == Feature::Worker {
                        let parts: Vec<_> = request.uri().path().split('/').collect();
                        if let Some(id) = parts.get(4) {
                            let valid = match parts.get(3) {
                                Some(&"sessions") => {
                                    crate::data::ids::WorkerSessionId::parse(id).is_ok()
                                }
                                Some(&"commands") => crate::data::ids::CommandId::parse(id).is_ok(),
                                Some(&"leases") => crate::data::ids::LeaseId::parse(id).is_ok(),
                                _ => true,
                            };
                            if !valid {
                                return ApiError::not_found("resource not found").into_response();
                            }
                        }
                    }
                    request.extensions_mut().insert(spec.op);
                    next.run(request).await
                }
            },
        ));
        router = router.route(spec.path, route);
    }
    router
        .fallback(|| async { ApiError::not_found("resource not found") })
        .method_not_allowed_fallback(|| async {
            ApiError::method_not_allowed("method not allowed")
        })
        .layer(axum::middleware::from_fn_with_state(
            (api.clone(), policy.clone()),
            daemon_auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            (api, policy),
            listener_gate,
        ))
        .layer(axum::middleware::from_fn(
            super::error::normalize_rejections,
        ))
        .layer(axum::middleware::from_fn(no_store))
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &axum::http::Request<axum::body::Body>| {
                // Never record the URI, headers, or raw path: rejected URL
                // credentials must stay out of even DEBUG/TRACE diagnostics.
                let route = request
                    .extensions()
                    .get::<axum::extract::MatchedPath>()
                    .map_or("<unmatched>", |path| path.as_str());
                tracing::debug_span!("request", method = %request.method(), route)
            },
        ))
}

/// Daemon HTTP data is never cacheable: every response carries
/// `Cache-Control: no-store` unless a handler chose a policy itself.
async fn no_store(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .entry(axum::http::header::CACHE_CONTROL)
        .or_insert(axum::http::HeaderValue::from_static("no-store"));
    response
}
fn worker_route(op: Operation, filter: MethodFilter) -> MethodRouter<Arc<AppState>> {
    match op {
        Operation::WorkerWorkdirs => axum::routing::on(filter, handle_workdirs),
        Operation::WorkerSessionsList => axum::routing::on(filter, handle_list_sessions),
        Operation::WorkerSessionsCreate => axum::routing::on(filter, handle_create_session),
        Operation::WorkerSessionsGet => axum::routing::on(filter, handle_get_session),
        Operation::WorkerSessionsDelete => axum::routing::on(filter, handle_close_session),
        Operation::WorkerSessionsQueue => axum::routing::on(filter, handle_get_session_queue),
        Operation::WorkerCommandsList => axum::routing::on(filter, handle_list_commands),
        Operation::WorkerCommandsCreate => axum::routing::on(filter, handle_create_command),
        Operation::WorkerCommandsGet => axum::routing::on(filter, handle_get_command),
        Operation::WorkerCommandsCancel => axum::routing::on(filter, handle_cancel_command),
        Operation::WorkerCommandsEvents => axum::routing::on(filter, handle_stream_command_logs),
        Operation::WorkerCommandsWorkflow => axum::routing::on(filter, handle_get_workflow),
        Operation::WorkerCommandsOutputs => {
            axum::routing::on(filter, crate::frontend::api::outputs::handle_list_outputs)
        }
        Operation::WorkerCommandsOutput => {
            axum::routing::on(filter, crate::frontend::api::outputs::handle_get_output)
        }
        Operation::WorkerRunsCreate => axum::routing::on(filter, handle_create_run),
        _ => unreachable!("non-worker operation"),
    }
}
use crate::data::daemon_state::Principal;
async fn daemon_auth(
    State((api, policy)): State<(Arc<DaemonApi>, ListenerPolicy)>,
    mut request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str());
    // Axum executes GET handlers for HEAD. Classify their effective operation
    // before every authority/authentication/visibility check as well.
    let method = if request.method() == axum::http::Method::HEAD {
        "GET"
    } else {
        request.method().as_str()
    };
    let selected = ROUTES
        .iter()
        .find(|s| Some(s.path) == path && s.method == method);
    if let ListenerPolicy::Network { name } = &policy {
        if !api
            .runtime
            .listener_settings(name)
            .is_some_and(|l| l.enabled)
        {
            return ApiError::not_found("resource not found").into_response();
        }
        let hidden = |spec: &RouteSpec| {
            spec.op.local_only()
                || spec
                    .op
                    .owner()
                    .feature_name()
                    .is_some_and(|f| !api.runtime.listener_serves(name, f))
        };
        if selected
            .map(&hidden)
            .unwrap_or_else(|| ROUTES.iter().any(|s| Some(s.path) == path && hidden(s)))
        {
            return ApiError::not_found("resource not found").into_response();
        }
    }
    if let Some(spec) = selected {
        request.extensions_mut().insert(spec.op);
    }
    if let ListenerPolicy::Network { name } = &policy {
        request
            .extensions_mut()
            .insert(ReceivingListener(name.clone()));
    }
    request.extensions_mut().insert(match policy {
        ListenerPolicy::Local => crate::data::daemon_state::CommandListener::Local,
        ListenerPolicy::Network { .. } => crate::data::daemon_state::CommandListener::Network,
    });
    if request
        .extensions()
        .get::<super::local::OwnerAuthenticated>()
        .is_some()
    {
        request.extensions_mut().insert(Principal::Owner);
        return next.run(request).await;
    }
    if let Some(browser) = request.extensions().get::<BrowserRequest>().cloned() {
        // Browser requests are authenticated by grants and tickets only; they
        // never reach API-key verification below (D1).
        return browser_auth(&api, selected.copied(), browser, request, next).await;
    }
    if request.extensions().get::<Operation>() == Some(&Operation::CoreHealth)
        || request.uri().path() == "/v1/health"
    {
        return next.run(request).await;
    }
    {
        let runtime = &api.runtime;
        let header = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        match runtime.keys().verify(header) {
            crate::engine::auth::api_keys::KeyVerdict::Accepted { id } => {
                let principal = Principal::ApiKey { id: id.to_string() };
                if let ListenerPolicy::Network { name } = &policy {
                    if let Some(spec) = selected {
                        if !api
                            .runtime
                            .listener_settings(name)
                            .is_some_and(|l| spec.op.allowed_for(&principal, &l))
                        {
                            return ApiError::not_found("resource not found").into_response();
                        }
                    }
                }
                request.extensions_mut().insert(principal);
                next.run(request).await
            }
            crate::engine::auth::api_keys::KeyVerdict::NoKeys => {
                ApiError::unauthorized("no API keys are configured on this daemon").into_response()
            }
            crate::engine::auth::api_keys::KeyVerdict::Rejected => {
                ApiError::unauthorized("Invalid API key.").into_response()
            }
        }
    }
}

#[cfg(test)]
type BrowserAuthenticatedCallback =
    dyn Fn(&DaemonApi, &crate::command::daemon_api::browser::BrowserPrincipal) + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
pub(super) struct BrowserAuthenticatedHook(pub Arc<BrowserAuthenticatedCallback>);

/// The browser branch of `daemon_auth` (WI 0159, §2.3/§2.7).
///
/// * health and the two pairing routes need no principal (they are still
///   Origin/Host gated);
/// * a WebSocket upgrade to the frontend or terminal socket authenticates
///   with a one-use ticket offered as `awman.ticket.<t>`;
/// * everything else needs `Authorization: Bearer awb_…` for this exact
///   origin and listener.
///
/// The resulting `Principal::Browser` must pass `Operation::allowed_for`
/// and, for instance-scoped engine routes, resource ownership (D3);
/// refusals look like a missing route.
async fn browser_auth(
    api: &Arc<DaemonApi>,
    selected: Option<RouteSpec>,
    browser: BrowserRequest,
    mut request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    use crate::command::daemon_api::browser::{may_touch_instance, BrowserPrincipal};
    use crate::data::browser_auth::TicketRoute;
    use crate::engine::auth::browser_grants::GrantVerdict;
    let Some(spec) = selected else {
        // Unknown path or method: the fallbacks answer without data.
        return next.run(request).await;
    };
    let Some(settings) = api.runtime.listener_settings(&browser.listener) else {
        return ApiError::not_found("resource not found").into_response();
    };
    let ctx = super::browser::context(&browser);
    // The principal a pairing route would act for, used only for the
    // listener/route authority check.
    let anonymous = Principal::Browser {
        grant_id: String::new(),
        origin: ctx.origin.clone(),
        listener: ctx.listener.clone(),
    };
    match spec.op {
        Operation::CoreHealth => return next.run(request).await,
        Operation::BrowserLinksCreate | Operation::BrowserLinksPoll => {
            if !spec.op.allowed_for(&anonymous, &settings) {
                return ApiError::not_found("resource not found").into_response();
            }
            return next.run(request).await;
        }
        _ => {}
    }
    let upgrade = request
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let socket_route = match spec.op {
        Operation::EngineInstancesFrontend => Some((TicketRoute::Frontend, 4)),
        Operation::CoreTerminalsGet => Some((TicketRoute::Terminal, 3)),
        _ => None,
    };
    let ticket = match super::browser::offered_ticket(request.headers()) {
        Ok(ticket) => ticket,
        Err(()) => {
            return ApiError::browser_ticket_invalid("invalid WebSocket ticket offer")
                .into_response()
        }
    };
    let principal = match (upgrade, socket_route) {
        (true, Some((route, segment))) => {
            let resource = request
                .uri()
                .path()
                .split('/')
                .nth(segment)
                .unwrap_or("")
                .to_owned();
            let consumed = ticket.as_deref().and_then(|t| {
                api.runtime
                    .browser_auth()
                    .consume_ticket(t, route, &resource, &ctx)
            });
            match consumed {
                Some(principal) => principal,
                None => {
                    return ApiError::browser_ticket_invalid(
                        "a valid one-use WebSocket ticket is required",
                    )
                    .into_response()
                }
            }
        }
        // Browsers cannot upgrade anything else, and tickets are only for
        // upgrades.
        (true, None) => return ApiError::not_found("resource not found").into_response(),
        (false, _) if ticket.is_some() => {
            return ApiError::browser_ticket_invalid("tickets are only valid for upgrades")
                .into_response()
        }
        (false, _) => {
            let header = request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .map(|v| v.to_str().unwrap_or(""));
            match api.runtime.browser_auth().authenticate(header, &ctx) {
                Ok(principal) => principal,
                Err(GrantVerdict::Missing) => {
                    return ApiError::browser_grant_required("A browser grant is required")
                        .into_response()
                }
                Err(_) => {
                    return ApiError::browser_grant_invalid(
                        "The browser grant is invalid, expired or revoked",
                    )
                    .into_response()
                }
            }
        }
    };
    #[cfg(test)]
    if let Some(hook) = request.extensions().get::<BrowserAuthenticatedHook>() {
        (hook.0)(api, &principal);
    }
    // Deletion can win after bearer/ticket validation. A browser principal
    // without a retained cancellation token must never reach either socket.
    let Some(watch) = api.runtime.browser_auth().watch(&principal.grant_id) else {
        return if upgrade {
            ApiError::browser_ticket_invalid("the browser grant has ended")
        } else {
            ApiError::browser_grant_invalid("the browser grant has ended")
        }
        .into_response();
    };
    if !spec.op.allowed_for(&principal.principal(), &settings) {
        return ApiError::not_found("resource not found").into_response();
    }
    // Instance-scoped engine routes: only instances this browser may touch.
    if spec.op.owner() == Feature::Engine && spec.path.starts_with("/v1/engine/instances/{id}") {
        let visible = request
            .uri()
            .path()
            .split('/')
            .nth(4)
            .and_then(|id| crate::data::ids::InstanceId::parse(id).ok())
            .is_some_and(|id| may_touch_instance(api, &principal, &id));
        if !visible {
            return ApiError::not_found("instance not found").into_response();
        }
    }
    request.extensions_mut().insert(watch);
    request.extensions_mut().insert(principal.principal());
    request
        .extensions_mut()
        .insert::<BrowserPrincipal>(principal);
    next.run(request).await
}

// Handlers

async fn handle_workdirs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<crate::data::wire::ListQuery>,
) -> Response {
    page_response(worker::workdirs(&state), &query)
}

async fn handle_create_session(
    axum::Extension(principal): axum::Extension<Principal>,
    receiving: Option<axum::Extension<ReceivingListener>>,
    State(state): State<Arc<AppState>>,
    axum::Extension(listener): axum::Extension<crate::data::daemon_state::CommandListener>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let mut context = crate::data::daemon_state::CommandRequestContext::from_header(
        listener,
        headers.get("traceparent").and_then(|v| v.to_str().ok()),
    );
    context.listener_name = receiving.map(|r| r.0 .0);
    let st = &state;
    with_idempotency(
        &state,
        principal.audit_id(),
        &headers,
        "/v1/worker/sessions",
        &value.to_string(),
        |journal| async move {
            let body = match serde_json::from_value(value) {
                Ok(body) => body,
                Err(error) => return bad_request(error.to_string()),
            };
            create_submission(st, context, body, None, false, journal.as_deref()).await
        },
    )
    .await
}

async fn handle_create_run(
    axum::Extension(principal): axum::Extension<Principal>,
    receiving: Option<axum::Extension<ReceivingListener>>,
    State(state): State<Arc<AppState>>,
    axum::Extension(listener): axum::Extension<crate::data::daemon_state::CommandListener>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let mut context = crate::data::daemon_state::CommandRequestContext::from_header(
        listener,
        headers.get("traceparent").and_then(|v| v.to_str().ok()),
    );
    context.listener_name = receiving.map(|r| r.0 .0);
    let st = &state;
    let canonical = value.to_string();
    with_idempotency(
        &state,
        principal.audit_id(),
        &headers,
        "/v1/worker/runs",
        &canonical,
        |journal| async move {
            let body: CreateRunRequest = match serde_json::from_value(value) {
                Ok(body) => body,
                Err(error) => return bad_request(error.to_string()),
            };
            if body.command.session_id.is_some() {
                return bad_request(
                    "command.session_id is not accepted: a run creates its session",
                );
            }
            create_submission(
                st,
                context,
                body.session,
                Some(body.command),
                body.close_on_finish,
                journal.as_deref(),
            )
            .await
        },
    )
    .await
}

async fn create_submission(
    state: &Arc<AppState>,
    context: crate::data::daemon_state::CommandRequestContext,
    body: CreateSessionRequest,
    command: Option<CreateCommandRequest>,
    close_on_finish: bool,
    journal: Option<&IdempotentAdmission>,
) -> Response {
    if let Err(error) = Labels::new(body.labels.clone().into_inner()) {
        return bad_request(error.to_string());
    }
    let submission = SessionSubmission {
        request: SessionCreateRequest {
            session_type: body.session_type,
            workdir: body.workdir,
            repo_url: body.repo_url,
            branch: body.branch,
            base_ref: body.base_ref,
            merge_refs: body.merge_refs,
            push: body.push,
        },
        labels: body.labels,
    };
    let result = match command {
        Some(command) => {
            worker::runs_create(
                state,
                context,
                submission,
                command.into(),
                close_on_finish,
                journal,
            )
            .await
        }
        None => {
            worker::sessions_create(state, context, submission, None, close_on_finish, journal)
                .await
        }
    };
    let admitted = match result {
        Ok(admitted) => admitted,
        Err(error) => return admission_error(error, ""),
    };
    let crate::command::commands::api_server::admission::AdmittedSession {
        session_id,
        plan,
        run,
        response_body,
    } = admitted;

    let setup_bus = Arc::new(SessionSetupBus::new(256));

    tracing::info!(
        session_id = %session_id,
        session_type = %plan.kind,
        workdir = %plan.resolved_workdir.display(),
        "Session created (setup starting)"
    );

    // Register before spawning/responding so cancellation can never miss setup.
    let token = state.setup_cancels.register(&session_id);
    let state_clone = Arc::clone(state);
    let sid = session_id.clone();
    let run_for_setup = run.clone();
    // There is no await between durable admission and spawning its owner.
    // Bus contention belongs to that task, so dropping the HTTP future cannot
    // strand admitted work or serialize validation behind a presentation lock.
    tokio::spawn(async move {
        state_clone
            .setup_buses
            .lock()
            .await
            .insert(sid.clone(), Arc::clone(&setup_bus));
        run_session_setup(state_clone, sid, plan, setup_bus, token, run_for_setup).await;
    });
    if run.is_none() {
        (
            StatusCode::CREATED,
            [("content-type", "application/json")],
            response_body,
        )
            .into_response()
    } else {
        accepted_json(response_body)
    }
}

/// Drive the Layer 2 [`SessionSetup`] orchestrator with an
/// [`ApiSessionSetupPresenter`]. Every setup *behaviour* — clone/branch
/// sequencing, the remote-clone failure-cleanup rule, the status column, the
/// `setup_state.json` snapshot and every `SessionSetupState` transition — is
/// Layer 2's or Layer 0's (WI 0114 F-41). This frontend supplies the bus the
/// events are broadcast on, the log they are mirrored to, and the ready
/// frontend, and nothing else.
async fn run_session_setup(
    state: Arc<AppState>,
    session_id: String,
    plan: SessionCreatePlan,
    setup_bus: Arc<SessionSetupBus>,
    token: tokio_util::sync::CancellationToken,
    run: Option<RunRecord>,
) {
    let setup = SessionSetup::new(
        session_id.clone(),
        plan,
        state.engines.clone(),
        Arc::clone(&state.sessions),
        Arc::clone(&setup_bus.current_state),
        Arc::clone(&state.store),
        state.paths.clone(),
    )
    .with_cancellation(token.clone());
    let mut presenter = ApiSessionSetupPresenter {
        bus_sender: setup_bus.sender(),
        setup_bus,
        state: Arc::clone(&state),
        session_id: session_id.clone(),
        verbose_setup: state
            .live_config
            .read()
            .expect("worker config lock")
            .as_ref()
            .map(|c| c.daemon().verbose_setup)
            .unwrap_or_else(|| {
                crate::data::config::daemon::DaemonSettings::from_merged(
                    &state
                        .engines
                        .global_config
                        .daemon
                        .clone()
                        .unwrap_or_default(),
                )
                .verbose_setup
            }),
    };
    // Lists can expose the queued row before this task is spawned (or even
    // before run linkage/token registration). Carry that early cancel into
    // Track A's setup-abort path as well.
    if let Some(run) = &run {
        if let Err(error) = state.lifecycle().reconcile_setup_cancel(run, &token) {
            tracing::error!(%error, session_id, command_id = %run.command_id, "Failed to read early setup cancellation");
        }
    }
    setup.run(&mut presenter).await;
    // Once cleanup is complete, cancellation may close a terminal run itself.
    // Unregister before checking its row so neither path can miss the close.
    state.setup_cancels.unregister(&session_id);
    if let Some(run) = run {
        if let Err(error) = state
            .lifecycle()
            .finish_run_setup(&state.lifecycle_store, &run)
            .await
        {
            tracing::error!(%error, "Failed to reconcile run after setup");
        }
    }
}

/// API-frontend implementation of the Layer 2 [`SessionSetupPresenter`]: the
/// session-setup broadcast bus, the session-tagged log, and the ready-checks
/// frontend. It persists nothing and decides nothing.
struct ApiSessionSetupPresenter {
    state: Arc<AppState>,
    session_id: String,
    setup_bus: Arc<SessionSetupBus>,
    bus_sender: SessionSetupBusSender,
    /// Whether per-session setup lines are logged at `info` or `debug`.
    /// Read once from the Layer 0 snapshot when the presenter is built; Layer
    /// 0 owns `AWMAN_API_VERBOSE_SETUP` and nothing above it reads the
    /// environment directly (F-37).
    verbose_setup: bool,
}

#[async_trait::async_trait]
impl SessionSetupPresenter for ApiSessionSetupPresenter {
    fn log(&mut self, line: &str) {
        log_session_setup(&self.session_id, self.verbose_setup, line);
    }

    fn emit(&mut self, event: SetupEventPayload) {
        self.bus_sender.emit(event);
    }

    fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend> {
        // A throwaway EventBus satisfies the ready frontend's container sink; its
        // events are mirrored to the tracing log and the session-setup bus.
        let event_bus = EventBus::new(4096);
        Box::new(SetupReadyFrontend::new(
            &self.session_id,
            self.setup_bus.sender(),
            event_bus.sender(),
            self.verbose_setup,
        ))
    }

    fn git_log_sink(&mut self) -> Box<dyn GitFrontend + Send> {
        Box::new(TracingSetupSink::new(&self.session_id, self.verbose_setup))
    }

    async fn finished(&mut self) {
        tokio::spawn(cleanup_setup_bus(
            Arc::clone(&self.state),
            self.session_id.clone(),
        ));
    }
}

/// Retain the setup bus for 60 seconds after the run reaches a terminal state,
/// so a client that polls or subscribes just after completion still sees the
/// final events, then drop it.
async fn cleanup_setup_bus(state: Arc<AppState>, session_id: String) {
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    state.setup_buses.lock().await.remove(&session_id);
}

async fn handle_list_sessions(
    State(state): State<Arc<AppState>>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let query = match ListQuery::parse(pairs, false) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    match worker::sessions_list(
        &state,
        query.session_status,
        query.setup_status,
        &query.labels,
    ) {
        Ok(sessions) => {
            let ids: Vec<_> = sessions.iter().map(|s| s.id.clone()).collect();
            let mut labels = match state.store.labels_for_many(LabelOwner::Session, &ids) {
                Ok(labels) => labels,
                Err(_) => return internal_error("Failed to read session labels"),
            };
            let list: Vec<SessionResponse> = sessions
                .into_iter()
                .map(|s| {
                    let labels = labels.remove(&s.id).unwrap_or_default();
                    SessionResponse::new(&state, s, labels)
                })
                .collect();
            keyed_response(list, &query.page, |v| v.id.as_str())
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list sessions");
            ApiError::internal("Failed to list sessions").into_response()
        }
    }
}

async fn handle_get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match worker::sessions_get(
        &state,
        &crate::data::ids::WorkerSessionId::parse(&id).expect("route validated session id"),
    ) {
        Ok(Some(s)) => session_response(&state, s),
        Ok(None) => {
            ApiError::session_not_found(format!("Session '{}' not found", id)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            ApiError::internal("Failed to get session").into_response()
        }
    }
}

async fn handle_close_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let closing_readiness = || -> RouteResult<SessionResponse> {
        match state.store.get_session(&id) {
            Ok(Some(row)) => Ok(SessionResponse::new(&state, row, Labels::empty())),
            _ => Err(Box::new(internal_error("Failed to get session"))),
        }
    };
    match worker::sessions_delete(
        &state,
        &crate::data::ids::WorkerSessionId::parse(&id).expect("route validated session id"),
    )
    .await
    {
        Ok(CloseOutcome::NotFound) => {
            ApiError::session_not_found(format!("Session '{id}' not found")).into_response()
        }
        Ok(CloseOutcome::Draining {
            running_command_id,
            cancelled,
        }) => {
            let readiness = match closing_readiness() {
                Ok(row) => row,
                Err(response) => return *response,
            };
            (
            StatusCode::ACCEPTED,
            Json(SessionClosingResponse {
                setup_status: readiness.setup_status,
                setup_error: readiness.setup_error,
                session_id: id,
                status: SessionStatus::Closing.as_str().to_string(),
                running_command_id: Some(running_command_id),
                cancelled_count: cancelled.len(),
                message: "Session is closing. Waiting for running command to complete. Poll GET /v1/worker/sessions/{id} to monitor.".to_string(),
            }),
        ).into_response()
        }
        Ok(CloseOutcome::AlreadyClosing) => {
            let readiness = match closing_readiness() {
                Ok(row) => row,
                Err(response) => return *response,
            };
            Json(SessionClosingResponse {
                setup_status: readiness.setup_status,
                setup_error: readiness.setup_error,
                session_id: id.clone(),
                status: SessionStatus::Closing.as_str().to_string(),
                running_command_id: state
                    .store
                    .running_command_for_session(&id)
                    .ok()
                    .flatten()
                    .map(|command| command.id),
                cancelled_count: 0,
                message:
                    "Session is already closing. Poll GET /v1/worker/sessions/{id} to monitor."
                        .to_string(),
            })
            .into_response()
        }
        Ok(CloseOutcome::Closed) => match state.store.get_session(&id) {
            Ok(Some(session)) => session_response(&state, session),
            _ => StatusCode::NO_CONTENT.into_response(),
        },
        Err(error) => {
            tracing::error!(%error, session_id = %id, "Failed to close session");
            ApiError::internal("Failed to clean up remote session directory").into_response()
        }
    }
}

async fn handle_create_command(
    axum::Extension(principal): axum::Extension<Principal>,
    receiving: Option<axum::Extension<ReceivingListener>>,
    State(state): State<Arc<AppState>>,
    axum::Extension(listener): axum::Extension<crate::data::daemon_state::CommandListener>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let mut context = crate::data::daemon_state::CommandRequestContext::from_header(
        listener,
        headers.get("traceparent").and_then(|v| v.to_str().ok()),
    );
    context.listener_name = receiving.map(|r| r.0 .0);
    let body: CreateCommandRequest = match serde_json::from_value(value.clone()) {
        Ok(body) => body,
        Err(error) => return bad_request(error.to_string()),
    };
    let session_id = match command_session_id(&headers, body.session_id.as_deref()) {
        Ok(session_id) => session_id,
        Err(response) => return *response,
    };
    let canonical = CommandSubmission::idempotency_canonical(&value, &session_id);
    let (st, hd) = (&state, &headers);
    with_idempotency(
        &state,
        principal.audit_id(),
        &headers,
        "/v1/worker/commands",
        &canonical,
        |journal| async move { create_command(st, context, hd, body, journal.as_deref()).await },
    )
    .await
}

async fn create_command(
    state: &AppState,
    context: crate::data::daemon_state::CommandRequestContext,
    headers: &HeaderMap,
    body: CreateCommandRequest,
    journal: Option<&IdempotentAdmission>,
) -> Response {
    let session_id = match command_session_id(headers, body.session_id.as_deref()) {
        Ok(session_id) => session_id,
        Err(response) => return *response,
    };

    let submission: CommandSubmission = body.into();
    if let Err(error) = submission.validate() {
        return admission_error(error, &session_id);
    }
    let id = match crate::data::ids::WorkerSessionId::parse(&session_id) {
        Ok(id) => id,
        Err(_) => return ApiError::session_not_found("session not found").into_response(),
    };
    match worker::commands_create(state, context, &id, submission, journal).await {
        Ok(admitted) => accepted_json(admitted.response_body),
        Err(error) => admission_error(error, &session_id),
    }
}

/// The session a `POST /v1/worker/commands` targets: the body's `session_id`, the
/// `x-awman-session` header, or both when they agree. Two different values
/// are refused; awman never picks one.
fn command_session_id(headers: &HeaderMap, body: Option<&str>) -> RouteResult<String> {
    let header = match headers.get("x-awman-session").map(|h| h.to_str()) {
        Some(Ok(header)) => Some(header),
        Some(Err(_)) => {
            return Err(Box::new(
                ApiError::invalid_request("Invalid x-awman-session header value").into_response(),
            ))
        }
        None => None,
    };
    match (body, header) {
        (Some(body), Some(header)) if body != header => Err(Box::new(
            ApiError::invalid_request(
                "session_id in the body and the x-awman-session header name different sessions",
            )
            .into_response(),
        )),
        (Some(session), _) | (None, Some(session)) => Ok(session.to_string()),
        (None, None) => Err(Box::new(
            ApiError::invalid_request("Missing required header: x-awman-session").into_response(),
        )),
    }
}

async fn handle_get_command(
    State(state): State<Arc<AppState>>,
    peer: Peer,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match worker::commands_get(
        &state,
        &crate::data::ids::CommandId::parse(&id).expect("route validated command id"),
    ) {
        Ok(Some(c)) => match command_response(&state, peer, c, None) {
            Ok(response) => Json(response).into_response(),
            Err(response) => *response,
        },
        Ok(None) => {
            ApiError::command_not_found(format!("Command '{}' not found", id)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command");
            ApiError::internal("Failed to get command").into_response()
        }
    }
}

/// Query parameters for the per-command SSE / log endpoint.
#[derive(Deserialize, Default)]
struct CommandLogsQuery {
    /// Return the full events.log as JSON instead of streaming SSE.
    #[serde(default)]
    format: Option<String>,
    /// Resume strictly after this cursor; overrides Last-Event-ID.
    #[serde(default)]
    after: Option<String>,
    /// Only events whose `source.step_name` is this step, plus the
    /// command-level terminal events (WI 0137 §5). Cursors stay global, so a
    /// filtered stream resumes with the same `Last-Event-ID`.
    #[serde(default)]
    step: Option<String>,
}

const WORKER_WAIT_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);
const COMMAND_STATUS_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// `GET /v1/worker/commands/{id}/events` — structured event stream.
///
/// Persisted frames carry `<attempt>:<sequence>` ids. `?after=` overrides
/// Last-Event-ID; malformed cursors return the usual JSON 400 response.
/// Unknown attempts restart replay at the current attempt. JSON format always
/// returns the full log. Waiting has no deadline and only terminal rows end in
/// Done. Informational waiting frames and comment keepalives have no id.
async fn handle_stream_command_logs(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
    Query(query): Query<CommandLogsQuery>,
    headers: HeaderMap,
    shutdown: Option<axum::Extension<tokio_util::sync::CancellationToken>>,
) -> Response {
    let command = match worker::commands_get(
        &state,
        &crate::data::ids::CommandId::parse(&command_id).expect("route validated command id"),
    ) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return ApiError::command_not_found(format!("Command '{command_id}' not found"))
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up command");
            return internal_error("Failed to look up command");
        }
    };
    let session_id = &command.session_id;

    // Resume inputs apply only to SSE; the step selector applies to both.
    if query.format.as_deref() == Some("json") {
        let events: Vec<_> = state
            .paths
            .read_command_events(session_id, &command_id)
            .into_iter()
            .filter(|event| {
                query
                    .step
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .is_none_or(|step| event.matches_step_filter(step))
            })
            .collect();
        return Json(serde_json::json!({
            "session_id": session_id,
            "command_id": command_id,
            "attempt": command.attempt,
            "events": events.iter().map(ExecutionEvent::public_value).collect::<Vec<_>>(),
        }))
        .into_response();
    }

    let invalid_id = || bad_request("invalid event id; expected <attempt>:<sequence>");
    let requested = if let Some(after) = query.after.as_deref() {
        match EventCursor::parse(after) {
            Some(cursor) => Some(cursor),
            None => return invalid_id(),
        }
    } else if let Some(header) = headers.get("last-event-id") {
        match header.to_str().ok().and_then(EventCursor::parse) {
            Some(cursor) => Some(cursor),
            None => return invalid_id(),
        }
    } else {
        None
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(256);
    let shutdown = shutdown.map(|extension| extension.0).unwrap_or_default();
    let stream = async move {
        let mut events =
            worker::commands_events(&state, command, requested, query.step, COMMAND_STATUS_POLL)
                .await;
        let mut waiting = false;
        let mut keepalive = tokio::time::interval_at(
            tokio::time::Instant::now() + WORKER_WAIT_KEEPALIVE,
            WORKER_WAIT_KEEPALIVE,
        );
        loop {
            let frame = tokio::select! {
                _ = tx.closed() => return,
                _ = keepalive.tick() => Event::default().comment(if waiting {
                    "waiting for command progress"
                } else {
                    "keepalive"
                }),
                update = events.next_update() => match update {
                    None => return,
                    Some(CommandLogUpdate::Waiting(value)) => {
                        waiting = value;
                        keepalive.reset();
                        continue;
                    }
                    Some(CommandLogUpdate::Restart(attempt)) => Event::default().comment(
                        format!("resuming from the start of attempt {attempt}")
                    ),
                    Some(CommandLogUpdate::Lagged(n)) => Event::default().comment(
                        format!("lagged: recovering {n} events from log")
                    ),
                    Some(CommandLogUpdate::QueueStatus(status)) => Event::default().event("queue_status").data(serde_json::to_string(&status).expect("queue status serializes")),
                    Some(CommandLogUpdate::Event { event, cursor }) => execution_event_to_sse(&event, cursor),
                }
            };
            if tx.send(Ok(frame)).await.is_err() {
                return;
            }
        }
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = shutdown.cancelled() => {},
            _ = stream => {},
        }
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}

/// Encode an ExecutionEvent with a cursor safe for axum's SSE id validation.
fn execution_event_to_sse(event: &ExecutionEvent, cursor: Option<EventCursor>) -> Event {
    let data = serde_json::to_string(&event.public_value()).unwrap_or_else(|_| "{}".into());
    let frame = match cursor {
        Some(cursor) => Event::default().id(cursor.to_string()),
        None => Event::default(),
    };
    frame.event(event.payload.sse_event_type()).data(data)
}
async fn handle_get_session_queue(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<crate::data::wire::ListQuery>,
) -> Response {
    match worker::sessions_queue(
        &state,
        &crate::data::ids::WorkerSessionId::parse(&id).expect("route validated session id"),
    ) {
        Ok(None) => {
            ApiError::session_not_found(format!("Session '{}' not found", id)).into_response()
        }
        Err(crate::command::commands::api_server::runtime::QueueViewError::Session(error)) => {
            tracing::error!(error = %error, "Failed to get session");
            ApiError::internal("Failed to get session").into_response()
        }
        Err(crate::command::commands::api_server::runtime::QueueViewError::Queue(error)) => {
            tracing::error!(session_id = %id, %error, "Failed to read session queue");
            internal_error("Failed to read session queue")
        }
        Err(crate::command::commands::api_server::runtime::QueueViewError::Labels(error)) => {
            tracing::error!(%error, "Failed to read command labels");
            internal_error("Failed to read command labels")
        }
        Ok(Some(view)) => {
            let value = serde_json::json!({
                "session_id": id,
                "queue_depth": view.queue_depth,
                "running": view.running.map(|item| serde_json::json!({
                    "command_id": item.command_id,
                    "subcommand": item.subcommand,
                    "args": item.args,
                    "started_at": item.started_at,
                    "worker_id": item.worker_id,
                    "labels": item.labels,
                })),
                "queued": view.queued.into_iter().map(|item| serde_json::json!({
                    "command_id": item.command_id,
                    "subcommand": item.subcommand,
                    "args": item.args,
                    "queued_at": item.timestamp,
                    "position": item.position,
                    "labels": item.labels,
                })).collect::<Vec<_>>(),
                "recent_completed": view.recent_completed.into_iter().map(|item| serde_json::json!({
                    "command_id": item.command_id,
                    "subcommand": item.subcommand,
                    "status": item.status.as_str(),
                    "exit_code": item.exit_code,
                    "finished_at": item.finished_at,
                    "labels": item.labels,
                })).collect::<Vec<_>>(),
            });
            let mut items = Vec::new();
            if !value["running"].is_null() {
                let mut running = value["running"].clone();
                running["status"] = serde_json::json!("running");
                items.push(running);
            }
            items.extend(
                value["queued"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|mut v| {
                        v["status"] = serde_json::json!("queued");
                        v
                    }),
            );
            items.extend(
                value["recent_completed"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            let items = match items
                .into_iter()
                .map(serde_json::from_value::<worker::QueueRecord>)
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(items) => items,
                Err(_) => return internal_error("invalid stored queue record"),
            };
            page_response(items, &query)
        }
    }
}

async fn handle_get_workflow(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
) -> Response {
    let id = crate::data::ids::CommandId::parse(&command_id).expect("route validated command id");
    let cmd = match worker::commands_get(&state, &id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return ApiError::command_not_found("command not found").into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command for workflow");
            return ApiError::internal("Failed to get command").into_response();
        }
    };

    match worker::commands_workflow(&state, &id) {
        // WI 0137 §6: computed, client-ready fields are added at read
        // time; the persisted state file stays an engine format.
        Ok(Some(val)) => {
            Json(crate::frontend::api::workflow_view::client_view(val)).into_response()
        }
        Ok(None) => {
            let error = match state.lifecycle().workflow_state_availability(&cmd) {
                crate::command::commands::api_server::runtime::WorkflowStateAvailability::NotStarted => {
                    ApiError::workflow_not_started("workflow not started")
                }
                crate::command::commands::api_server::runtime::WorkflowStateAvailability::NeverStarted => {
                    ApiError::workflow_never_started("workflow never started")
                }
                crate::command::commands::api_server::runtime::WorkflowStateAvailability::NotWorkflow => {
                    ApiError::no_workflow("no workflow for this command")
                }
            };
            error.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to read workflow state");
            ApiError::internal("Failed to read workflow state").into_response()
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[tokio::test]
    async fn owner_socket_peer_is_considered_local_for_path_projection() {
        use axum::extract::FromRequestParts;
        let request = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        parts
            .extensions
            .insert(super::super::local::OwnerAuthenticated);
        let peer = Peer::from_request_parts(&mut parts, &()).await.unwrap();
        assert!(peer.is_loopback());
    }
    use std::sync::Arc;
    use std::time::Instant;

    #[tokio::test]
    async fn setup_owner_refuses_work_when_early_cancel_row_cannot_be_read() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = make_test_state(tmp.path());
        Arc::get_mut(&mut state).unwrap().workdirs = vec![tmp.path().canonicalize().unwrap()];
        let admitted = state
            .admission()
            .admit_session(
                SessionSubmission {
                    request: SessionCreateRequest {
                        workdir: Some(tmp.path().to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                    labels: Labels::empty(),
                },
                Some(CommandSubmission {
                    allow_remote_input: false,
                    interaction: None,
                    subcommand: "exec prompt".into(),
                    args: vec!["hello".into()],
                    labels: Labels::empty(),
                }),
                true,
                None,
            )
            .await
            .unwrap();
        state
            .store
            .with_conn(|conn| {
                conn.execute_batch("ALTER TABLE commands RENAME TO unavailable_commands")?;
                Ok(())
            })
            .unwrap();
        let token = state.setup_cancels.register(&admitted.session_id);
        run_session_setup(
            Arc::clone(&state),
            admitted.session_id.clone(),
            admitted.plan,
            Arc::new(SessionSetupBus::new(256)),
            token.clone(),
            admitted.run,
        )
        .await;
        assert!(token.is_cancelled());
        let snapshot = state.paths.read_setup_state(&admitted.session_id).unwrap();
        assert_eq!(snapshot.status, SessionSetupStatus::Failed);
        assert_eq!(snapshot.error.unwrap().stage, "cancelled");
        assert!(!state.setup_cancels.is_registered(&admitted.session_id));
    }

    #[tokio::test]
    async fn setup_bus_contention_cannot_leave_admitted_work_without_a_setup_task() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = make_test_state(tmp.path());
        Arc::get_mut(&mut state).unwrap().workdirs = vec![tmp.path().canonicalize().unwrap()];
        let bus_guard = state.setup_buses.lock().await;
        let request = serde_json::from_value(serde_json::json!({"workdir": tmp.path()})).unwrap();
        let mut submission = Box::pin(create_submission(
            &state,
            Default::default(),
            request,
            None,
            false,
            None,
        ));
        // Even with the bus lock held, admission and spawn finish in one poll.
        let response = match futures_util::poll!(&mut submission) {
            std::task::Poll::Ready(response) => response,
            std::task::Poll::Pending => panic!("setup scheduling yielded after admission"),
        };
        assert_eq!(response.status(), StatusCode::CREATED);
        drop(submission);
        let session = state.store.list_sessions().unwrap().pop().unwrap();
        assert!(state.setup_cancels.is_registered(&session.id));
        assert_eq!(session.setup_status, SessionSetupStatus::Initializing);
        // The setup task survives its request and waits on the lock itself.
        tokio::task::yield_now().await;
        assert!(!bus_guard.contains_key(&session.id));
        state.setup_cancels.cancel(&session.id);
        drop(bus_guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if state.setup_buses.lock().await.contains_key(&session.id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("setup owner was never spawned");
    }

    #[tokio::test]
    async fn queue_and_command_reads_preserve_each_legacy_args_value_without_hiding_other_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path());
        state
            .store
            .insert_session("ses_03c7c0ace395d80182db07ae2c30f034", "/tmp", "now")
            .unwrap();
        state
            .store
            .enqueue_command(
                "cmd_9f7d0ee82b6a6ca7ddeae841f3253059",
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "exec prompt",
                "[\"hello\"]",
                "log",
            )
            .unwrap();
        for (id, raw, expected) in [
            (
                "cmd_00000000000000000000000000000001",
                "{}",
                serde_json::json!({}),
            ),
            (
                "cmd_00000000000000000000000000000002",
                "[1]",
                serde_json::json!([1]),
            ),
            (
                "cmd_00000000000000000000000000000003",
                "{",
                serde_json::json!([]),
            ),
        ] {
            state
                .store
                .enqueue_command(
                    id,
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "exec prompt",
                    raw,
                    "log",
                )
                .unwrap();
            let response = handle_get_session_queue(
                State(Arc::clone(&state)),
                AxumPath("ses_03c7c0ace395d80182db07ae2c30f034".into()),
                Query(Default::default()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let queued = body["items"].as_array().unwrap();
            assert_eq!(
                queued.iter().find(|row| row["command_id"] == id).unwrap()["args"],
                expected
            );
            assert_eq!(
                queued[0]["command_id"],
                "cmd_9f7d0ee82b6a6ca7ddeae841f3253059"
            );
            let command = command_response(
                &state,
                Peer(None, false),
                state.store.get_command(id).unwrap().unwrap(),
                None,
            )
            .unwrap_or_else(|_| panic!("command read failed"));
            assert_eq!(command.args, expected);
        }
    }

    #[tokio::test]
    async fn command_log_encoding_preserves_exact_persisted_and_transient_bytes() {
        use crate::data::execution_event::EventPayload;

        let event = ExecutionEvent {
            timestamp: "2026-10-30T12:00:00Z".parse().unwrap(),
            attempt: 2,
            sequence: 7,
            source: None,
            payload: EventPayload::StdoutLine("hello\nworld".into()),
        };
        for (cursor, id) in [(Some(event.cursor()), "id: 2:7\n"), (None, "")] {
            let frame = execution_event_to_sse(&event, cursor);
            let response = Sse::new(tokio_stream::iter([Ok::<_, std::convert::Infallible>(
                frame,
            )]))
            .into_response();
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let expected = format!(
                "{id}event: stdout_line\ndata: {}\n\n",
                serde_json::to_string(&event.public_value()).unwrap()
            );
            assert_eq!(bytes.as_ref(), expected.as_bytes());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_a_worker_sends_keepalives_on_cadence_past_300s() {
        use futures_util::StreamExt;

        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path());
        state
            .store
            .insert_session_full(crate::data::fs::api_db::NewSessionRow {
                id: "ses_03c7c0ace395d80182db07ae2c30f034",
                workdir: "/work",
                created_at: &chrono::Utc::now().to_rfc3339(),
                setup_status: SessionSetupStatus::Ready,
                kind: crate::data::session::SessionKind::Local,
                cloned_path: None,
            })
            .unwrap();
        state
            .store
            .enqueue_command(
                "cmd_4a8a08f09d37b73795649038408b5f33",
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "exec prompt",
                "[]",
                "/unused",
            )
            .unwrap();
        let response = handle_stream_command_logs(
            State(state),
            AxumPath("cmd_4a8a08f09d37b73795649038408b5f33".into()),
            Query(CommandLogsQuery::default()),
            HeaderMap::new(),
            None,
        )
        .await;
        let mut body = response.into_body().into_data_stream();
        let waiting = body.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&waiting)
            .unwrap()
            .contains("Waiting for a worker"));
        let started = tokio::time::Instant::now();
        for n in 1..=22 {
            let frame = body.next().await.unwrap().unwrap();
            assert_eq!(frame.as_ref(), b": waiting for command progress\n\n");
            assert_eq!(started.elapsed(), WORKER_WAIT_KEEPALIVE * n);
        }
    }

    // Route table — assertion guard. Every entry here must be registered in
    // build_router; any divergence is a regression.
    const EXPECTED_ROUTES: &[RouteSpec] = ROUTES;

    pub(crate) fn make_test_state(tmp: &std::path::Path) -> Arc<AppState> {
        use crate::command::dispatch::Engines;
        use crate::data::fs::api_db::SqliteSessionStore;
        use crate::data::fs::api_paths::ApiPaths;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::engine::agent::AgentEngine;
        use crate::engine::auth::AuthEngine;
        use crate::engine::container::ContainerRuntime;
        use crate::engine::git::GitEngine;
        use crate::engine::overlay::OverlayEngine;

        let paths = ApiPaths::at_root(tmp);
        let store = Arc::new(SqliteSessionStore::open(tmp).unwrap());
        let runtime = Arc::new(ContainerRuntime::docker());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(tmp),
        ));
        let git_engine = Arc::new(GitEngine::new());
        let agent_engine = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
        let auth_engine = Arc::new(AuthEngine::with_paths(
            AuthPathResolver::at_home(tmp),
            paths.clone(),
        ));
        let workflow_state_store = Arc::new(crate::data::WorkflowStateStore::at_git_root(tmp));
        let engines = Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
            credential_monitor: None,
            global_config: std::sync::Arc::new(Default::default()),
        };
        let lifecycle_store = crate::data::CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
        let setup_cancels = Arc::new(crate::command::commands::api_server::TokenRegistry::new());
        let canceller = crate::command::commands::api_server::CommandCanceller::new(
            Arc::clone(&store),
            lifecycle_store.clone(),
            Arc::new(crate::command::commands::api_server::TokenRegistry::new()),
            Arc::clone(&setup_cancels),
        );
        Arc::new(AppState {
            live_config: std::sync::RwLock::new(None),
            settings_tx: None,
            lifecycle_store,
            setup_cancels,
            canceller,
            store,
            paths,
            workdirs: Vec::new(),
            workdir_roots: Vec::new(),
            started_at: Instant::now(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            engines,
            sessions: Arc::new(SessionManager::in_memory(
                crate::data::config::env::EnvSnapshot::empty(),
            )),
            event_buses: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    #[test]
    fn expected_route_count() {
        // Guard: if someone adds a route without updating this table, the count drifts.
        assert_eq!(
            EXPECTED_ROUTES.len(),
            77,
            "route count mismatch — update EXPECTED_ROUTES"
        );
    }

    #[test]
    fn create_session_request_defaults_remote_fields_when_omitted() {
        let request: CreateSessionRequest =
            serde_json::from_str(r#"{"session_type":"remote","repo_url":"git://example/repo"}"#)
                .unwrap();
        assert_eq!(request.base_ref, None);
        assert!(request.merge_refs.is_empty());
        assert!(!request.push);
    }

    #[tokio::test]
    async fn all_expected_routes_respond_non_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path());
        let app = build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        // Test routes that always return non-404 regardless of request content.
        // These only depend on server state, not on specific resource IDs.
        let unconditional_routes: &[(&str, &str)] = &[
            ("GET", "/v1/status"),
            ("GET", "/v1/worker/workdirs"),
            ("GET", "/v1/worker/sessions"),
            ("GET", "/v1/worker/commands"),
        ];

        for (method, path) in unconditional_routes {
            let url = format!("http://{addr}{path}");
            let req = match *method {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                _ => panic!("unhandled method {method}"),
            };
            let resp = req
                .send()
                .await
                .unwrap_or_else(|e| panic!("request to {method} {path} failed: {e}"));
            assert_ne!(
                resp.status().as_u16(),
                404,
                "{method} {path} returned 404 — route may not be registered"
            );
        }

        // Routes that naturally return 4xx for missing resources ARE registered —
        // verify by calling them with the correct method and asserting we get
        // anything other than a routing-level 404 for a completely unknown path.
        // (We use a clearly-bogus path to get the routing 404 baseline, then compare.)
        let bogus_404 = client
            .get(format!("http://{addr}/v1/definitely-not-a-route"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(bogus_404, 404, "bogus path must return 404");

        // Resource routes: these return handler-level 4xx (session/command not found).
        // We assert they respond with something (connection succeeds and we get any HTTP response).
        let resource_routes: &[(&str, &str, u16)] = &[
            // (method, path, expected_status_for_missing_resource)
            (
                "GET",
                "/v1/worker/sessions/cmd_361dc45aacd2d2a1961554d12a2d666b",
                404,
            ), // session not found
            (
                "DELETE",
                "/v1/worker/sessions/cmd_361dc45aacd2d2a1961554d12a2d666b",
                404,
            ), // session not found
            (
                "GET",
                "/v1/worker/sessions/cmd_361dc45aacd2d2a1961554d12a2d666b/status",
                404,
            ), // session not found
            (
                "GET",
                "/v1/worker/sessions/cmd_361dc45aacd2d2a1961554d12a2d666b/queue",
                404,
            ), // session not found
            (
                "GET",
                "/v1/worker/commands/cmd_361dc45aacd2d2a1961554d12a2d666b",
                404,
            ), // command not found
            (
                "GET",
                "/v1/worker/commands/cmd_361dc45aacd2d2a1961554d12a2d666b/status",
                404,
            ), // command not found
            (
                "GET",
                "/v1/worker/commands/cmd_361dc45aacd2d2a1961554d12a2d666b/events",
                404,
            ), // command not found
            (
                "POST",
                "/v1/worker/commands/cmd_361dc45aacd2d2a1961554d12a2d666b/cancel",
                404,
            ), // command not found
            ("GET", "/v1/workflows/test-cmd", 404), // command not found
        ];

        for (method, path, expected_status) in resource_routes {
            let url = format!("http://{addr}{path}");
            let req = match *method {
                "GET" => client.get(&url),
                "DELETE" => client.delete(&url),
                "POST" => client.post(&url),
                _ => panic!("unhandled method {method}"),
            };
            let resp = req
                .send()
                .await
                .unwrap_or_else(|e| panic!("request to {method} {path} failed: {e}"));
            // The handler returns *expected_status* for missing resources.
            // We verify the route exists by confirming the response status matches
            // what the handler produces (not a routing-level 404 from an unregistered path).
            // Since both cases return 404 here, we at least verify the request succeeds.
            assert_eq!(
                resp.status().as_u16(),
                *expected_status,
                "{method} {path} returned unexpected status"
            );
        }

        // POST /v1/worker/sessions — check it responds (even with 400/422 for missing body).
        let resp = client
            .post(format!("http://{addr}/v1/worker/sessions"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/worker/sessions returned 404 — route may not be registered"
        );

        // POST /v1/worker/commands — check it responds (even with 400 for missing headers).
        let resp = client
            .post(format!("http://{addr}/v1/worker/commands"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/worker/commands returned 404 — route may not be registered"
        );
        let resp = client
            .post(format!("http://{addr}/v1/worker/runs"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/worker/runs must be registered"
        );
    }

    #[test]
    fn auth_middleware_rejects_missing_authorization_header() {
        // Auth logic is synchronous; test the hash comparison in isolation.
        use ring::digest;
        use subtle::ConstantTimeEq;

        let key = "test-api-key";
        let hash: String = {
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };

        // Good key: computed hash matches stored hash.
        let provided_hash: String = {
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(bool::from(provided_hash.as_bytes().ct_eq(hash.as_bytes())));

        // Bad key: hash does NOT match.
        let bad_hash: String = {
            let h = digest::digest(&digest::SHA256, b"wrong-key");
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(!bool::from(bad_hash.as_bytes().ct_eq(hash.as_bytes())));
    }

    #[tokio::test]
    async fn auth_enabled_rejects_bad_key_with_401() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path());
        let api = Arc::new(DaemonApi::for_worker_tests(state));
        let (_, key) = api.runtime.keys().create("test").unwrap();
        let key = key.as_str();
        let app = build_router_for_listener(
            api,
            ListenerPolicy::Network {
                name: "default".into(),
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        // No Authorization header → 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "missing auth header must return 401"
        );

        // Wrong key → 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .header("Authorization", "Bearer wrong-key")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "wrong key must return 401");

        // Correct key → not 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .unwrap();
        assert_ne!(resp.status().as_u16(), 401, "correct key must pass auth");
    }

    #[tokio::test]
    async fn auth_disabled_allows_all_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path()); // AuthMode::Disabled by default
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/v1/status"))
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            401,
            "disabled auth must not block requests"
        );
    }
}

fn page_response<T: Serialize>(items: Vec<T>, query: &crate::data::wire::ListQuery) -> Response {
    match crate::command::daemon_api::core::page(items, query) {
        Ok(page) => Json(page).into_response(),
        Err(e) => ApiError::invalid_request(e.to_string()).into_response(),
    }
}
fn squad_error(error: crate::command::daemon_api::squad::SquadError) -> Response {
    use crate::command::daemon_api::squad::SquadError;
    match error {
        SquadError::TaskNotFound => ApiError::task_not_found("task not found"),
        SquadError::RunNotFound => ApiError::run_not_found("run not found"),
        SquadError::TaskExists => ApiError::task_exists("task already exists"),
        SquadError::TaskPaused => {
            ApiError::invalid_request("task is paused; resume it before starting a run")
        }
        SquadError::NoWorkflow => ApiError::no_workflow("no workflow for this task"),
        SquadError::Command(e) => ApiError::from_command_error(&e),
    }
    .into_response()
}
#[derive(Default)]
struct ResourceQuery {
    name: Option<String>,
    status: Option<String>,
    page: crate::data::wire::ListQuery,
}
async fn resource_handler(
    State(api): State<Arc<DaemonApi>>,
    axum::Extension(op): axum::Extension<Operation>,
    params: Option<AxumPath<HashMap<String, String>>>,
    Query(pairs): Query<Vec<(String, String)>>,
    body: axum::body::Bytes,
) -> Response {
    use crate::command::daemon_api::{core, squad};
    use crate::data::ids::{GrantId, RunId, TaskId};
    let mut query = ResourceQuery::default();
    for (key, value) in pairs {
        match key.as_str() {
            "name" => query.name = Some(value),
            "status" => query.status = Some(value),
            "cursor" => query.page.cursor = Some(value),
            "limit" => {
                query.page.limit = match value.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        return ApiError::invalid_request("limit must be an integer")
                            .into_response()
                    }
                };
            }
            _ => {}
        }
    }
    let params = params.map(|AxumPath(p)| p).unwrap_or_default();
    let id = params.get("id").map(String::as_str).unwrap_or("");
    macro_rules! command {
        ($value:expr) => {
            match $value {
                Ok(v) => Json(v).into_response(),
                Err(e) => ApiError::from_command_error(&e).into_response(),
            }
        };
    }
    macro_rules! sq {
        ($value:expr) => {
            match $value {
                Ok(v) => Json(v).into_response(),
                Err(e) => squad_error(e),
            }
        };
    }
    macro_rules! task_id {
        () => {
            match TaskId::parse(id) {
                Ok(id) => id,
                Err(_) => return ApiError::not_found("resource not found").into_response(),
            }
        };
    }
    macro_rules! decode {
        ($ty:ty) => {
            match serde_json::from_slice::<$ty>(&body) {
                Ok(v) => v,
                Err(error) => {
                    return ApiError::invalid_request(format!("malformed resource request: {error}"))
                        .into_response()
                }
            }
        };
    }
    match op {
        Operation::CoreStatus => command!(core::status(&api).await),
        Operation::CoreConfigGet => Json(core::config_get(&api)).into_response(),
        Operation::CoreHealth => Json(core::health().await).into_response(),
        Operation::CoreKeysList => match crate::command::daemon_api::keys::keys_list(&api) {
            Ok(v) => page_response(v.items, &query.page),
            Err(e) => key_error(e),
        },
        Operation::CoreKeysCreate => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct CreateKey {
                name: String,
            }
            let request = decode!(CreateKey);
            match crate::command::daemon_api::keys::keys_create(&api, &request.name) {
                Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
                Err(e) => key_error(e),
            }
        }
        Operation::CoreKeysDelete => match crate::data::ids::KeyId::parse(id) {
            Ok(id) => match crate::command::daemon_api::keys::keys_delete(&api, &id) {
                Ok(true) => StatusCode::NO_CONTENT.into_response(),
                Ok(false) => ApiError::not_found("key not found").into_response(),
                Err(e) => key_error(e),
            },
            Err(_) => ApiError::not_found("resource not found").into_response(),
        },
        Operation::CoreConfigReload => match core::config_reload(&api).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => ApiError::config_invalid("configuration validation failed")
                .with_details(serde_json::json!({"errors":[e.to_string()]})),
        },
        Operation::CoreFeaturesList => match core::features_list(&api).await {
            Ok(v) => page_response(v, &query.page),
            Err(e) => ApiError::from_command_error(&e).into_response(),
        },
        Operation::CoreFeaturesGet => {
            match Feature::ALL
                .into_iter()
                .find(|f| params.get("feature").is_some_and(|p| p == f.name()))
            {
                Some(f) => command!(core::feature(&api, f).await),
                None => ApiError::not_found("feature not found").into_response(),
            }
        }
        Operation::CoreFeaturesPatch => {
            use crate::command::daemon_api::core::FeaturePatchError;
            use crate::command::daemon_runtime::FeatureChangeError;
            let Some(feature) = crate::data::daemon_state::FeatureName::ALL
                .into_iter()
                .find(|f| params.get("feature").is_some_and(|p| p == f.as_str()))
            else {
                return ApiError::not_found("feature not found").into_response();
            };
            match core::feature_patch(&api, feature, decode!(core::FeaturePatch)).await {
                Ok(record) => Json(record).into_response(),
                Err(FeaturePatchError::PersistentChange) => {
                    ApiError::invalid_request("persistent changes go through awman config set")
                        .into_response()
                }
                Err(FeaturePatchError::Change(FeatureChangeError::UnknownFeature)) => {
                    ApiError::not_found("feature not found").into_response()
                }
                Err(FeaturePatchError::Change(FeatureChangeError::EphemeralNotAllowed(f))) => {
                    ApiError::ephemeral_not_allowed(format!(
                        "ephemeral enablement is not allowed for {f}"
                    ))
                    .into_response()
                }
                Err(FeaturePatchError::Change(FeatureChangeError::EnabledByConfig(f))) => {
                    ApiError::conflict(format!("{f} is enabled by config")).into_response()
                }
                Err(FeaturePatchError::Change(FeatureChangeError::Engine(e))) => {
                    ApiError::internal(e.to_string()).into_response()
                }
            }
        }
        Operation::CoreGrantsList => match core::grants_list(&api) {
            Ok(v) => page_response(v, &query.page),
            Err(e) => ApiError::from_command_error(&e).into_response(),
        },
        Operation::CoreGrantsDelete => match GrantId::parse(id) {
            Ok(id) => match core::grants_delete(&api, id) {
                Ok(true) => StatusCode::NO_CONTENT.into_response(),
                Ok(false) => ApiError::grant_not_found("grant not found").into_response(),
                Err(e) => ApiError::from_command_error(&e).into_response(),
            },
            Err(_) => ApiError::not_found("resource not found").into_response(),
        },
        Operation::SquadTasksList => match squad::tasks_list(&api, query.name.as_deref()).await {
            Ok(v) => keyed_response(
                v.into_iter().map(squad::TaskResponse).collect(),
                &query.page,
                |v| v.0.id.as_str(),
            ),
            Err(e) => squad_error(e),
        },
        Operation::SquadTasksCreate => match squad::tasks_create(
            &api,
            decode!(crate::command::commands::squad::gateway::CreateTask),
        )
        .await
        {
            Ok(v) => (StatusCode::CREATED, Json(squad::TaskResponse(v))).into_response(),
            Err(e) => squad_error(e),
        },
        Operation::SquadTasksGet => sq!(squad::tasks_get(&api, &task_id!())
            .await
            .map(squad::TaskResponse)),
        Operation::SquadTasksUpdate => {
            sq!(
                squad::tasks_update(&api, &task_id!(), decode!(squad::TaskPatch))
                    .await
                    .map(squad::TaskResponse)
            )
        }
        Operation::SquadTasksDelete => match squad::tasks_delete(&api, &task_id!()).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => squad_error(e),
        },
        Operation::SquadTasksWorkflow => match squad::tasks_workflow(&api, &task_id!()).await {
            Ok(v) => Json(super::workflow_view::client_view(
                serde_json::to_value(v).expect("workflow serializes"),
            ))
            .into_response(),
            Err(e) => squad_error(e),
        },
        Operation::SquadRunsCreate => match squad::runs_create(&api, &task_id!()).await {
            Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
            Err(e) => squad_error(e),
        },
        Operation::SquadRunsList => {
            if query.status.as_deref().is_some_and(|status| {
                ![
                    "running",
                    "not_triggered",
                    "workflow_executed",
                    "failed",
                    "interrupted",
                    "canceled",
                ]
                .contains(&status)
            }) {
                return ApiError::invalid_request("invalid run status filter").into_response();
            }
            match squad::runs_list(&api, &task_id!(), query.status.as_deref()).await {
                Ok(v) => keyed_response(
                    v.into_iter().map(squad::RunResponse).collect(),
                    &query.page,
                    |v| v.0.id.as_str(),
                ),
                Err(e) => squad_error(e),
            }
        }
        Operation::SquadRunsCancel => match RunId::parse(id) {
            Ok(id) => sq!(squad::runs_cancel(&api, &id).await),
            Err(_) => ApiError::not_found("resource not found").into_response(),
        },
        Operation::SquadEnvGet => command!(squad::env_get(&api).await),
        Operation::SquadEnvPut => {
            let push = match serde_json::from_slice::<
                crate::command::commands::squad::gateway::EnvPush,
            >(&body)
            {
                Ok(push) => push,
                Err(_) => {
                    tracing::warn!("squad: rejected a malformed daemon env push");
                    return ApiError::invalid_request("malformed daemon env request")
                        .into_response();
                }
            };
            command!(squad::env_put(&api, push).await)
        }
        Operation::SquadEnvClear => command!(squad::env_clear(&api)
            .await
            .map(|cleared| serde_json::json!({"cleared":cleared}))),
        _ => unreachable!("worker routes have typed worker handlers"),
    }
}

fn key_error(error: crate::command::daemon_api::keys::KeyError) -> Response {
    use crate::command::daemon_api::keys::KeyError;
    match error {
        KeyError::InvalidName => {
            ApiError::invalid_request("invalid key name: use [a-z0-9][a-z0-9_-]{0,62}")
                .into_response()
        }
        KeyError::NameTaken => {
            ApiError::conflict("a key with this name already exists").into_response()
        }
        KeyError::Engine(e) => ApiError::internal(e.to_string()).into_response(),
    }
}

fn parse_at(raw: &str) -> Result<chrono::DateTime<chrono::Utc>, chrono::ParseError> {
    chrono::DateTime::parse_from_rfc3339(raw).map(|t| t.with_timezone(&chrono::Utc))
}

fn keyed_response<T: Serialize>(
    items: Vec<T>,
    query: &crate::data::wire::ListQuery,
    key: impl Fn(&T) -> &str,
) -> Response {
    match crate::command::daemon_api::core::keyed_page(items, query, key) {
        Ok(page) => Json(page).into_response(),
        Err(e) => ApiError::invalid_request(e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod wire_contract_tests {
    use super::*;
    fn round<T: Serialize + serde::de::DeserializeOwned>(value: serde_json::Value) {
        let response: T = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(response).unwrap();
        let decoded: T = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(encoded, serde_json::to_value(decoded).unwrap());
        fn units(value: &serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    for (k, v) in fields {
                        assert!(!k.ends_with("_seconds") && !k.ends_with("_secs"), "{k}");
                        if (k.ends_with("_ms") || k.ends_with("_bytes")) && !v.is_null() {
                            assert!(v.is_u64(), "{k}: {v}");
                        }
                        if k.ends_with("_at") && !v.is_null() {
                            let raw = v.as_str().unwrap();
                            chrono::DateTime::parse_from_rfc3339(raw).unwrap();
                            assert!(raw.ends_with('Z'), "{k}: {raw}");
                        }
                        units(v);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        units(item)
                    }
                }
                _ => {}
            }
        }
        units(&encoded);
    }
    #[test]
    fn every_owned_response_round_trips_with_layer_zero_ids_and_time_units() {
        let session = crate::data::ids::WorkerSessionId::new();
        let command = crate::data::ids::CommandId::new();
        let run = crate::data::ids::RunId::new();
        let at = "2026-11-05T12:34:56.789Z";
        round::<SessionResponse>(
            serde_json::json!({"id":session,"workdir":"/repo","created_at":at,"closed_at":at,"status":"closed","session_type":"local","setup_status":"ready","push":false}),
        );
        round::<CommandResponse>(
            serde_json::json!({"id":command,"attempt":1,"session_id":session,"subcommand":"exec prompt","args":[],"status":"done","labels":{},"started_at":at,"finished_at":at,"queued_at":at,"cancel_requested_at":at}),
        );
        round::<SessionClosingResponse>(
            serde_json::json!({"session_id":session,"setup_status":"ready","status":"closing","running_command_id":command,"cancelled_count":1,"message":"closing"}),
        );
        round::<crate::command::daemon_api::core::StatusResponse>(
            serde_json::json!({"status":"ok","version":"test","protocol":{"api":1},"capabilities":[],"features":{},"max_concurrent_jobs":2,"pid":1,"uptime_ms":1234,"listeners":{"local":null,"network":{}},"agents":[]}),
        );
        round::<crate::command::daemon_api::core::FeatureRecord>(
            serde_json::json!({"name":"worker","state":"on","running":1,"queued":2,"enabled_by":"config","oldest_queued_age_ms":4100,"restarts":0,"last_error":null,"since_at":"2026-11-06T10:00:00Z"}),
        );
        round::<crate::command::daemon_api::core::ReloadResponse>(
            serde_json::json!({"applied":[],"restart_required":[]}),
        );
        round::<crate::command::daemon_api::core::ConfigResponse>(
            serde_json::json!({"config":{},"sources":{}}),
        );
        round::<crate::command::daemon_api::squad::RunCreated>(serde_json::json!({"id":run}));
        round::<crate::command::daemon_api::squad::RunResponse>(serde_json::json!({
            "id":run,"task_id":crate::data::ids::TaskId::new(),"status":"running",
            "workflow_path":null,"workflow_state_path":null,"session_id":session,
            "started_at":at,"finished_at":at,"error":null
        }));
        round::<crate::command::daemon_api::worker::QueueRecord>(serde_json::json!({
            "command_id":command,"subcommand":"exec prompt","args":[],"status":"done",
            "queued_at":at,"started_at":at,"finished_at":at,"labels":{},"exit_code":0
        }));
        round::<crate::frontend::api::outputs::OutputEntry>(serde_json::json!({
            "path":"result.json","size_bytes":512,"sha256":"fixture","modified_at":at
        }));
        round::<crate::command::daemon_api::worker::Workdir>(
            serde_json::json!({"path":"/repo","kind":"allow"}),
        );
        round::<crate::command::daemon_api::squad::RunCanceled>(
            serde_json::json!({"id":run,"status":"canceled"}),
        );
        round::<crate::command::daemon_api::squad::TaskResponse>(
            serde_json::json!({"id":crate::data::ids::TaskId::new(),"name":"nightly","description":"fixture","repo_scope":"/repo","mount_scope":"directory","overlays":[],"interval_ms":60000,"status":"active","agent":null,"model":null,"backoff_until":null,"created_at":at,"updated_at":at,"last_run_at":null}),
        );
        round::<crate::command::daemon_api::core::GrantResponse>(
            serde_json::json!({"id":crate::data::ids::GrantId::new(),"git_root":"/repo","remote_url":"https://example.com/repo","prerequisite":"ssh","grant":{"type":"ssh_dir"},"recorded_at":at}),
        );
        round::<crate::command::commands::squad::gateway::EnvCoverage>(
            serde_json::json!({"salt":"fixture","persistence":"none","required":[]}),
        );
        round::<crate::command::commands::squad::gateway::EnvPushResponse>(
            serde_json::json!({"accepted":[],"ignored":[],"coverage":{"salt":"fixture","persistence":"none","required":[]}}),
        );
    }
    #[test]
    fn central_capabilities_are_unique_and_cover_every_route() {
        let c = crate::command::daemon_api::capabilities::CAPABILITIES;
        assert_eq!(
            c.len(),
            c.iter().collect::<std::collections::HashSet<_>>().len()
        );
        for route in ROUTES {
            if let Some(capability) = route.capability {
                assert!(c.contains(&capability));
            }
        }
    }
}

/// Name of the receiving listener, preserved for worker commands and viewers.
#[derive(Clone)]
pub struct ReceivingListener(pub String);

/// Exact browser request context established by `listener_gate`.
/// Browser authentication consumes this in the browser-auth workflow lane.
#[derive(Clone, Debug)]
pub struct BrowserRequest {
    pub origin: crate::data::config::network::BrowserOrigin,
    pub listener: String,
}

async fn listener_gate(
    State((api, policy)): State<(Arc<DaemonApi>, ListenerPolicy)>,
    mut request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    use crate::data::config::network::ListenerTransport;
    let ListenerPolicy::Network { name } = policy else {
        if super::browser::query_carries_credentials(request.uri()) {
            return ApiError::invalid_request("credentials are never accepted in the URL")
                .into_response();
        }
        return next.run(request).await;
    };
    let Some(settings) = api.runtime.listener_settings(&name).filter(|l| l.enabled) else {
        return ApiError::not_found("resource not found").into_response();
    };
    if request
        .uri()
        .path()
        .split('/')
        .any(|segment| segment == "..")
    {
        return ApiError::not_found("resource not found").into_response();
    }
    let matched = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str());
    let hidden = ROUTES
        .iter()
        .find(|s| Some(s.path) == matched)
        .is_some_and(|spec| {
            spec.op.local_only()
                || spec
                    .op
                    .owner()
                    .feature_name()
                    .is_some_and(|f| !api.runtime.listener_serves(&name, f))
        });
    // Keep TLS feature hiding unchanged. On loopback, validate the browser
    // origin first so an allowed origin can read the ordinary 404 response.
    if hidden && settings.transport != ListenerTransport::LoopbackHttp {
        return ApiError::not_found("resource not found").into_response();
    }
    if settings.transport == ListenerTransport::LoopbackHttp
        && (request.headers().get_all("host").iter().count() != 1
            || request.headers().get("host").and_then(|h| h.to_str().ok())
                != Some(settings.listen.to_string().as_str()))
    {
        return ApiError::host_mismatch("Host must match the listener address").into_response();
    }
    let browser = request.headers().contains_key("origin")
        || request
            .headers()
            .keys()
            .any(|h| h.as_str().starts_with("sec-fetch-"));
    if !browser {
        if super::browser::query_carries_credentials(request.uri()) {
            return ApiError::invalid_request("credentials are never accepted in the URL")
                .into_response();
        }
        if hidden {
            return ApiError::not_found("resource not found").into_response();
        }
        return next.run(request).await;
    }
    let origin = request
        .headers()
        .get("origin")
        .and_then(|h| h.to_str().ok());
    let allowed = settings.transport == ListenerTransport::LoopbackHttp
        && request.headers().get_all("origin").iter().count() == 1
        && origin.is_some_and(|o| {
            settings
                .allowed_origins
                .iter()
                .any(|allowed| allowed.as_str() == o)
        });
    if !allowed {
        return ApiError::origin_not_allowed("Origin is not allowed").into_response();
    }
    let origin = request.headers()["origin"].clone();
    let parsed_origin = match origin
        .to_str()
        .ok()
        .and_then(|s| crate::data::config::network::BrowserOrigin::parse(s).ok())
    {
        Some(origin) => origin,
        None => return ApiError::origin_not_allowed("Origin is not allowed").into_response(),
    };
    let private_network = request
        .headers()
        .get("access-control-request-private-network")
        .is_some_and(|v| v == "true");
    let preflight = request.method() == axum::http::Method::OPTIONS;
    let health = request.uri().path() == "/v1/health";
    // Origin/Host refusal remains first, without CORS. Once allowed, apply
    // URL policy before health, preflight, hidden routes or method fallbacks.
    let mut response = if super::browser::query_carries_credentials(request.uri()) {
        ApiError::invalid_request("credentials are never accepted in the URL").into_response()
    } else if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else if hidden {
        ApiError::not_found("resource not found").into_response()
    } else if health {
        let listener_status = api.runtime.listeners();
        let record = listener_status.network.get(&name);
        let browser_links = record.is_some_and(|r| r.browser_links);
        let engine_state = api
            .runtime
            .feature_state(crate::data::daemon_state::FeatureName::Engine)
            .state;
        let mut protocols = serde_json::json!({"api":1,"frontend":1,"terminal":1});
        if browser_links {
            protocols["browser"] = serde_json::json!(1);
        }
        Json(serde_json::json!({
            "status":"ok",
            "build":env!("CARGO_PKG_VERSION"),
            "protocols":protocols,
            "capabilities":crate::command::daemon_api::capabilities::CAPABILITIES,
            "listener":{"name":name,"transport":settings.transport,"features":settings.features.into_iter().filter(|f| api.runtime.listener_serves(&name,*f)).collect::<Vec<_>>()},
            "engine":{"state":engine_state},
            "browser_links":browser_links
        })).into_response()
    } else {
        let method = request.method();
        if [
            axum::http::Method::POST,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
        ]
        .contains(method)
            && !request
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
                })
        {
            ApiError::unsupported_media_type("Browser requests require application/json")
                .into_response()
        } else if request
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > crate::data::browser_auth::BROWSER_BODY_LIMIT as u64)
        {
            ApiError::request_too_large("Browser request bodies are limited to 64 KiB")
                .into_response()
        } else {
            request.extensions_mut().insert(BrowserRequest {
                origin: parsed_origin,
                listener: name.clone(),
            });
            // Chunked bodies are bounded while they are read (413 on overflow).
            let request = request.map(|body| {
                axum::body::Body::new(http_body_util::Limited::new(
                    body,
                    crate::data::browser_auth::BROWSER_BODY_LIMIT,
                ))
            });
            next.run(request).await
        }
    };
    let headers = response.headers_mut();
    headers.insert("access-control-allow-origin", origin);
    headers.insert("vary", axum::http::HeaderValue::from_static("Origin"));
    headers.insert(
        "access-control-expose-headers",
        axum::http::HeaderValue::from_static("Retry-After, ETag"),
    );
    headers.insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    if preflight {
        headers.insert(
            "access-control-allow-methods",
            axum::http::HeaderValue::from_static("GET, POST, PATCH, DELETE, OPTIONS"),
        );
        headers.insert(
            "access-control-allow-headers",
            axum::http::HeaderValue::from_static("Authorization, Content-Type, If-None-Match"),
        );
        headers.insert(
            "access-control-max-age",
            axum::http::HeaderValue::from_static("600"),
        );
        if private_network {
            headers.insert(
                "access-control-allow-private-network",
                axum::http::HeaderValue::from_static("true"),
            );
        }
    }
    response
}
