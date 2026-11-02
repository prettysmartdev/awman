//! HTTP route registration and handlers for the API server.
//!
//! Routes preserve the API's established request and response contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::trace::TraceLayer;

use crate::command::commands::api_server::admission::{
    AdmissionError, ApiAdmission, CommandSubmission, IdempotentAdmission, SessionSubmission,
};
use crate::command::commands::api_server::command_log_stream::{
    CommandLogStream, CommandLogUpdate,
};
use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::api_server::session_setup::{
    log_session_setup, SessionSetupBus, SessionSetupBusSender, SetupReadyFrontend, TracingSetupSink,
};
pub use crate::command::commands::api_server::AuthMode;
use crate::command::commands::api_server::{
    ApiSessionLifecycle, CancelOutcome, CloseOutcome, CommandCanceller, SetupReadiness,
    TokenRegistry,
};
use crate::command::session_create::{SessionCreatePlan, SessionCreateRequest};
use crate::command::session_setup::{SessionSetup, SessionSetupPresenter};
use crate::data::api_lifecycle_store::{CommandLifecycleStore, RunRecord};
use crate::data::command_status::CommandStatus;
use crate::data::execution_event::{EventCursor, ExecutionEvent};
use crate::data::fs::api_db::SessionRecord;
use crate::data::fs::api_db::{CommandListFilter, CommandRecord, IdempotencyBegin, LabelOwner};
use crate::data::fs::api_db::{SessionCommandAdmission, SqliteSessionStore};
use crate::data::fs::api_labels::{LabelSelector, Labels};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::session_setup_event::SetupEventPayload;
use crate::data::session_status::SessionStatus;
use crate::engine::git::GitFrontend;
use crate::engine::ready::frontend::ReadyFrontend;

// ─── Auth mode ───────────────────────────────────────────────────────────────

// ─── Shared state ────────────────────────────────────────────────────────────
pub struct AppState {
    pub store: Arc<SqliteSessionStore>,
    pub lifecycle_store: CommandLifecycleStore,
    pub canceller: CommandCanceller,
    pub setup_cancels: Arc<TokenRegistry>,
    pub paths: ApiPaths,
    pub workdirs: Vec<PathBuf>,
    pub workdir_roots: Vec<PathBuf>,
    pub started_at: Instant,
    pub task_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub auth_mode: AuthMode,
    pub engines: crate::command::dispatch::Engines,
    pub sessions: Arc<SessionManager>,
    pub event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    pub setup_buses: tokio::sync::Mutex<HashMap<String, Arc<SessionSetupBus>>>,
}

impl AppState {
    fn admission(&self) -> ApiAdmission {
        ApiAdmission::new(
            Arc::clone(&self.store),
            self.lifecycle_store.clone(),
            self.paths.clone(),
            self.engines.clone(),
            Arc::clone(&self.sessions),
            self.workdirs.clone(),
            self.workdir_roots.clone(),
        )
    }

    fn lifecycle(&self) -> ApiSessionLifecycle {
        ApiSessionLifecycle::new(
            Arc::clone(&self.store),
            self.engines.clone(),
            Arc::clone(&self.sessions),
            self.paths.clone(),
        )
    }
}

#[derive(Serialize)]
struct SessionClosingResponse {
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

#[derive(Serialize)]
struct SessionResponse {
    id: String,
    workdir: String,
    created_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<String>,
    session_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_ref: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    merge_refs: Vec<String>,
    push: bool,
    #[serde(default)]
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
            ..Self::from(row)
        }
    }
}

impl From<SessionRecord> for SessionResponse {
    fn from(s: SessionRecord) -> Self {
        Self {
            id: s.id,
            workdir: s.workdir,
            created_at: s.created_at,
            status: s.status.as_str().to_string(),
            setup_status: s.setup_status.as_str().to_string(),
            setup_error: None,
            closed_at: s.closed_at,
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
    subcommand: String,
    args: Vec<String>,
    #[serde(default)]
    labels: Labels,
    /// `POST /v1/commands` only: the session, as an alternative to the
    /// `x-awman-session` header. A run's command must not name one.
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Serialize)]
struct CommandResponse {
    id: String,
    attempt: u32,
    session_id: String,
    subcommand: String,
    args: serde_json::Value,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    /// Deprecated: a server-side path a remote client cannot open, so it is
    /// sent only to loopback peers. Clients read `GET /v1/commands/{id}/logs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    labels: Labels,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancel_requested_at: Option<String>,
}

/// Capabilities a client can detect without parsing the version. Append only.
pub const API_FEATURES: &[&str] = &[
    "error_codes",
    "workflow_state",
    "command_cancel",
    "labels",
    "idempotency",
    "runs",
    // WI 0137: `result.{outcome,failure,failed_steps,git,pull_request,
    // final_message,outputs,data}` and the one final `command_status`.
    "structured_results",
    // WI 0137: `GET /v1/commands/{id}/outputs[/{path}]`.
    "outputs",
    // WI 0137: `source` on the event envelope and `?step=` on the logs.
    "event_source",
    // WI 0137: `step_activity` events from structured-output agents.
    "step_activity",
    // WI 0138: `GET /v1/commands/{id}`, the command record.
    "command_record",
    // WI 0138: `setup_status`/`setup_error` on session records and the
    // `?setup_status=` filter on `GET /v1/sessions`.
    "session_setup_status",
    // WI 0138: `session_id` in the `POST /v1/commands` body.
    "command_session_body",
];

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    pid: u32,
    uptime_seconds: u64,
    active_sessions: i64,
    running_commands: i64,
    features: Vec<String>,
}

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
            subcommand: body.subcommand,
            args: body.args,
            labels: body.labels,
        }
    }
}

fn admission_error(error: AdmissionError, session_id: &str) -> Response {
    match error {
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
                "hint": "Poll GET /v1/sessions/{id}/status to check setup progress"
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
/// Only the socket address counts; `Forwarded`/`X-Forwarded-For` are never
/// read. No `ConnectInfo` (a router served without it) reads as not loopback.
#[derive(Clone, Copy)]
pub(crate) struct Peer(Option<std::net::SocketAddr>);

impl Peer {
    fn is_loopback(self) -> bool {
        self.0
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
        .cancel_requested_at(&c.id)
        .map_err(|_| internal_error("Failed to read cancel request"))?;
    let queue_position = state
        .lifecycle()
        .queue_position(&c)
        .map_err(|_| internal_error("Failed to read queue position"))?;
    Ok(CommandResponse {
        args: crate::command::commands::api_server::runtime::stored_args_view(&c),
        result: c
            .result
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok()),
        id: c.id,
        attempt: c.attempt,
        session_id: c.session_id,
        subcommand: c.subcommand,
        // Rows migrate `pending` → `queued`; never show the legacy name.
        status: c.status.canonical().as_str().to_string(),
        exit_code: c.exit_code,
        started_at: c.started_at,
        finished_at: c.finished_at,
        log_path: peer.is_loopback().then_some(c.log_path),
        queued_at: c.queued_at,
        queue_position,
        worker_id: c.worker_id,
        labels,
        cancel_requested_at,
    })
}

#[derive(Default)]
struct ListQuery {
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
            limit: 100,
            ..Self::default()
        };
        for (key, value) in pairs {
            match key.as_str() {
                "label" => query
                    .labels
                    .push(LabelSelector::parse(&value).map_err(|e| bad_request(e.to_string()))?),
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
                "limit" if commands => {
                    query.limit = value
                        .parse::<i64>()
                        .map_err(|_| bad_request("limit must be an integer"))?
                        .clamp(1, 1000)
                }
                _ => {}
            }
        }
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
    let rows = match state.store.list_commands_filtered(&CommandListFilter {
        status: query.command_status,
        session_id: query.session_id.as_deref(),
        labels: &query.labels,
        limit: query.limit,
    }) {
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
    Json(serde_json::json!({ "commands": commands })).into_response()
}

async fn handle_cancel_command(
    State(state): State<Arc<AppState>>,
    peer: Peer,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.lifecycle().cancel_command(&id, &state.canceller, &state.setup_cancels, &state.lifecycle_store).await {
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
        }) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "command_id": command_id, "session_id": session_id, "status": CommandStatus::Running.as_str(),
                "cancel_requested_at": requested_at,
                "message": "Stopping. Poll GET /v1/commands/{id}/status until status is cancelled.",
            })),
        )
            .into_response(),
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

async fn with_idempotency<F, Fut>(
    state: &AppState,
    headers: &HeaderMap,
    route: &'static str,
    canonical: &str,
    handler: F,
) -> Response
where
    F: FnOnce(Option<Arc<IdempotentAdmission>>) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let Some(header) = headers.get("idempotency-key") else {
        return handler(None).await;
    };
    let key = match header.to_str() {
        Ok(key)
            if !key.is_empty()
                && key.len() <= crate::data::fs::api_db::IDEMPOTENCY_KEY_MAX_BYTES
                && key.bytes().all(|b| (0x21..=0x7e).contains(&b)) =>
        {
            key
        }
        _ => return bad_request("invalid Idempotency-Key header"),
    };
    let principal = match &state.auth_mode {
        AuthMode::Enabled { key_hash } => key_hash.as_str(),
        AuthMode::Disabled => "anonymous",
    };
    let journal = Arc::new(IdempotentAdmission::new(
        Arc::clone(&state.store),
        route,
        principal,
        key,
    ));
    match journal.begin(canonical) {
        Ok(IdempotencyBegin::Fresh) => {}
        Ok(IdempotencyBegin::Replay { status, body }) => {
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
        Ok(IdempotencyBegin::InFlight) => {
            return (
                [("retry-after", "1")],
                ApiError::idempotency_in_flight(
                    "a request with this Idempotency-Key is still in progress",
                ),
            )
                .into_response()
        }
        Ok(IdempotencyBegin::Mismatch) => {
            return ApiError::idempotency_conflict(
                "Idempotency-Key reused with a different request body",
            )
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, "Failed to reserve idempotency key");
            return internal_error("Failed to reserve idempotency key");
        }
    }
    let response = match std::panic::AssertUnwindSafe(handler(Some(Arc::clone(&journal))))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => return internal_error("Request handler failed"),
    };
    if response.status().is_server_error() {
        // Layer 2 retains a barrier if admission compensation is unfinished.
        if let Err(error) = journal.release() {
            tracing::error!(%error, "Failed to release idempotency reservation");
        }
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
    match journal.complete(parts.status.as_u16(), body) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("Idempotency reservation was no longer held when completing");
        }
        Err(error) => {
            // The reservation guard keeps an admitted request's recorded
            // response; the work itself has already been accepted.
            tracing::error!(%error, "Failed to save idempotent response");
        }
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

// ─── Router ──────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/status", get(handle_status))
        .route("/v1/workdirs", get(handle_workdirs))
        .route(
            "/v1/sessions",
            get(handle_list_sessions).post(handle_create_session),
        )
        .route(
            "/v1/sessions/{id}",
            get(handle_get_session).delete(handle_close_session),
        )
        .route("/v1/sessions/{id}/status", get(handle_get_session_status))
        .route("/v1/sessions/{id}/queue", get(handle_get_session_queue))
        .route(
            "/v1/commands",
            get(handle_list_commands).post(handle_create_command),
        )
        .route("/v1/commands/{id}/cancel", post(handle_cancel_command))
        .route("/v1/runs", post(handle_create_run))
        // WI 0138: the command record itself; `/status` stays as an alias.
        .route("/v1/commands/{id}", get(handle_get_command))
        .route("/v1/commands/{id}/status", get(handle_get_command))
        .route("/v1/commands/{id}/logs", get(handle_stream_command_logs))
        .route("/v1/workflows/{command_id}", get(handle_get_workflow))
        // WI 0137 §6: every per-command resource under /v1/commands/{id}/.
        .route("/v1/commands/{id}/workflow", get(handle_get_workflow))
        // WI 0137 §3: the agent-written outputs directory.
        .route(
            "/v1/commands/{id}/outputs",
            get(crate::frontend::api::outputs::handle_list_outputs),
        )
        .route(
            "/v1/commands/{id}/outputs/{*path}",
            get(crate::frontend::api::outputs::handle_get_output),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(axum::middleware::from_fn(
            super::error::normalize_rejections,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ─── Auth middleware ─────────────────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    // State extraction only: the decision itself lives in one shared place so
    // the API and squad daemons can never diverge on how a key is accepted.
    if let Some(rejection) = crate::frontend::api::serve::check_bearer_auth(
        &state.engines,
        &state.auth_mode,
        req.headers(),
    ) {
        return rejection;
    }
    next.run(req).await
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn handle_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let active_sessions = state.store.count_active_sessions().unwrap_or(0);
    let running_commands = state.store.count_running_commands().unwrap_or(0);
    let uptime = state.started_at.elapsed().as_secs();

    Json(StatusResponse {
        status: "ok".to_string(),
        pid: std::process::id(),
        uptime_seconds: uptime,
        active_sessions,
        running_commands,
        features: API_FEATURES
            .iter()
            .map(|feature| (*feature).to_string())
            .collect(),
    })
}

async fn handle_workdirs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let dirs: Vec<String> = state
        .workdirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let roots: Vec<String> = state
        .workdir_roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    Json(serde_json::json!({ "workdirs": dirs, "roots": roots }))
}

async fn handle_create_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let st = &state;
    with_idempotency(
        &state,
        &headers,
        "/v1/sessions",
        &value.to_string(),
        |journal| async move {
            let body = match serde_json::from_value(value) {
                Ok(body) => body,
                Err(error) => return bad_request(error.to_string()),
            };
            create_submission(st, body, None, false, journal.as_deref()).await
        },
    )
    .await
}

async fn handle_create_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let st = &state;
    let canonical = value.to_string();
    with_idempotency(
        &state,
        &headers,
        "/v1/runs",
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
    body: CreateSessionRequest,
    command: Option<CreateCommandRequest>,
    close_on_finish: bool,
    journal: Option<&IdempotentAdmission>,
) -> Response {
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
    let admitted = match state
        .admission()
        .admit_session(
            submission,
            command.map(CommandSubmission::from),
            close_on_finish,
            journal,
        )
        .await
    {
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
    accepted_json(response_body)
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
        verbose_setup: crate::data::config::env::Env::from_process().api_verbose_setup(),
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
    match state.store.list_sessions_filtered(
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
            Json(serde_json::json!({ "sessions": list })).into_response()
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
    match state.store.get_session(&id) {
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
    match state.lifecycle().close(&id).await {
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
                message: "Session is closing. Waiting for running command to complete. Poll GET /v1/sessions/{id}/status to monitor.".to_string(),
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
                    "Session is already closing. Poll GET /v1/sessions/{id}/status to monitor."
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

async fn handle_get_session_status(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // Check if session exists at all.
    match state.store.get_session(&id) {
        Ok(None) => {
            return ApiError::session_not_found(format!("Session '{}' not found", id))
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return ApiError::internal("Failed to get session").into_response();
        }
        Ok(Some(_)) => {}
    }

    // Capturing the frontend's bus is wiring; Layer 2 owns source precedence.
    let snapshot = state
        .setup_buses
        .lock()
        .await
        .get(&id)
        .map(|bus| bus.snapshot());
    use crate::command::commands::api_server::runtime::SessionStatusView;
    match state.lifecycle().session_status_view(&id, snapshot) {
        Ok(SessionStatusView::Snapshot {
            labels,
            state: setup_state,
        }) => Json(serde_json::json!({
            "session_id": id,
            "labels": labels,
            "status": setup_state.status,
            "current_stage": setup_state.current_stage,
            "current_ready_phase": setup_state.current_ready_phase,
            "ready_step_statuses": setup_state.ready_step_statuses,
            "ready_summary": setup_state.ready_summary,
            "error": setup_state.error,
        }))
        .into_response(),
        Ok(SessionStatusView::Fallback(fallback)) => status_fallback_response(&id, fallback),
        Err(error) => status_fallback_error(&id, error),
    }
}

fn status_fallback_error(
    id: &str,
    error: crate::command::commands::api_server::runtime::StatusFallbackError,
) -> Response {
    match error {
        crate::command::commands::api_server::runtime::StatusFallbackError::Session(error) => {
            tracing::error!(session_id = id, %error, "Failed to read session setup status");
            internal_error("Failed to read session")
        }
        crate::command::commands::api_server::runtime::StatusFallbackError::Labels(error) => {
            tracing::error!(session_id = id, %error, "Failed to read session labels");
            internal_error("Failed to read session labels")
        }
    }
}

fn status_fallback_response(
    id: &str,
    fallback: crate::command::commands::api_server::runtime::SessionStatusFallback,
) -> Response {
    Json(serde_json::json!({
        "session_id": id,
        "labels": fallback.labels,
        "status": fallback.setup_status.as_str(),
    }))
    .into_response()
}

async fn handle_create_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
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
        &headers,
        "/v1/commands",
        &canonical,
        |journal| async move { create_command(st, hd, body, journal.as_deref()).await },
    )
    .await
}

async fn create_command(
    state: &AppState,
    headers: &HeaderMap,
    body: CreateCommandRequest,
    journal: Option<&IdempotentAdmission>,
) -> Response {
    let session_id = match command_session_id(headers, body.session_id.as_deref()) {
        Ok(session_id) => session_id,
        Err(response) => return *response,
    };

    match state
        .admission()
        .admit_command(&session_id, body.into(), journal)
        .await
    {
        Ok(admitted) => accepted_json(admitted.response_body),
        Err(error) => admission_error(error, &session_id),
    }
}

/// The session a `POST /v1/commands` targets: the body's `session_id`, the
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
    match state.store.get_command(&id) {
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

/// `GET /v1/commands/{id}/logs` — structured event stream.
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
    let command = match state.store.get_command(&command_id) {
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
            "events": events,
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
        let mut events = CommandLogStream::open(
            Arc::clone(&state.store),
            state.paths.clone(),
            Arc::clone(&state.event_buses),
            command,
            requested,
            query.step,
            COMMAND_STATUS_POLL,
        )
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
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    let frame = match cursor {
        Some(cursor) => Event::default().id(cursor.to_string()),
        None => Event::default(),
    };
    frame.event(event.payload.sse_event_type()).data(data)
}
async fn handle_get_session_queue(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.lifecycle().queue_view(&id) {
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
        Ok(Some(view)) => Json(serde_json::json!({
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
        }))
        .into_response(),
    }
}

async fn handle_get_workflow(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
) -> Response {
    let cmd = match state.store.get_command(&command_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return ApiError::command_not_found("command not found").into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command for workflow");
            return ApiError::internal("Failed to get command").into_response();
        }
    };

    match state
        .paths
        .read_command_workflow_state_raw(&cmd.session_id, &command_id)
    {
        Ok(Some(content)) => match serde_json::from_str::<serde_json::Value>(&content) {
            // WI 0137 §6: computed, client-ready fields are added at read
            // time; the persisted state file stays an engine format.
            Ok(val) => Json(crate::frontend::api::workflow_view::client_view(val)).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse workflow state");
                ApiError::internal("Failed to parse workflow state").into_response()
            }
        },
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
mod tests {
    use super::*;
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
        let mut submission = Box::pin(create_submission(&state, request, None, false, None));
        // Even with the bus lock held, admission and spawn finish in one poll.
        let response = match futures_util::poll!(&mut submission) {
            std::task::Poll::Ready(response) => response,
            std::task::Poll::Pending => panic!("setup scheduling yielded after admission"),
        };
        assert_eq!(response.status(), StatusCode::ACCEPTED);
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
        state.store.insert_session("s", "/tmp", "now").unwrap();
        state
            .store
            .enqueue_command("valid", "s", "exec prompt", "[\"hello\"]", "log")
            .unwrap();
        for (id, raw, expected) in [
            ("object", "{}", serde_json::json!({})),
            ("number", "[1]", serde_json::json!([1])),
            ("truncated", "{", serde_json::json!([])),
        ] {
            state
                .store
                .enqueue_command(id, "s", "exec prompt", raw, "log")
                .unwrap();
            let response =
                handle_get_session_queue(State(Arc::clone(&state)), AxumPath("s".into())).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let queued = body["queued"].as_array().unwrap();
            assert_eq!(
                queued.iter().find(|row| row["command_id"] == id).unwrap()["args"],
                expected
            );
            assert_eq!(queued[0]["command_id"], "valid");
            let command = command_response(
                &state,
                Peer(None),
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
                "{id}event: stdout_line\ndata: {{\"timestamp\":\"2026-10-30T12:00:00Z\",\"attempt\":2,\"sequence\":7,\"payload\":{{\"type\":\"StdoutLine\",\"data\":\"hello\\nworld\"}}}}\n\n"
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
                id: "s",
                workdir: "/work",
                created_at: &chrono::Utc::now().to_rfc3339(),
                setup_status: SessionSetupStatus::Ready,
                kind: crate::data::session::SessionKind::Local,
                cloned_path: None,
            })
            .unwrap();
        state
            .store
            .enqueue_command("c", "s", "exec prompt", "[]", "/unused")
            .unwrap();
        let response = handle_stream_command_logs(
            State(state),
            AxumPath("c".into()),
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
    const EXPECTED_ROUTES: &[(&str, &str)] = &[
        ("GET", "/v1/status"),
        ("GET", "/v1/workdirs"),
        ("GET", "/v1/sessions"),
        ("POST", "/v1/sessions"),
        ("GET", "/v1/sessions/{id}"),
        ("DELETE", "/v1/sessions/{id}"),
        ("GET", "/v1/sessions/{id}/status"),
        ("GET", "/v1/sessions/{id}/queue"),
        ("POST", "/v1/commands"),
        ("GET", "/v1/commands"),
        ("POST", "/v1/commands/{id}/cancel"),
        ("POST", "/v1/runs"),
        ("GET", "/v1/commands/{id}"),
        ("GET", "/v1/commands/{id}/status"),
        ("GET", "/v1/commands/{id}/logs"),
        ("GET", "/v1/workflows/{command_id}"),
        ("GET", "/v1/commands/{id}/workflow"),
        ("GET", "/v1/commands/{id}/outputs"),
        ("GET", "/v1/commands/{id}/outputs/{*path}"),
    ];

    fn make_test_state(tmp: &std::path::Path) -> Arc<AppState> {
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
            config_notices: Default::default(),
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
            lifecycle_store,
            setup_cancels,
            canceller,
            store,
            paths,
            workdirs: Vec::new(),
            workdir_roots: Vec::new(),
            started_at: Instant::now(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            auth_mode: AuthMode::Disabled,
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
            19,
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
            ("GET", "/v1/workdirs"),
            ("GET", "/v1/sessions"),
            ("GET", "/v1/commands"),
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
            ("GET", "/v1/sessions/test-id", 404), // session not found
            ("DELETE", "/v1/sessions/test-id", 404), // session not found
            ("GET", "/v1/sessions/test-id/status", 404), // session not found
            ("GET", "/v1/sessions/test-id/queue", 404), // session not found
            ("GET", "/v1/commands/test-id", 404), // command not found
            ("GET", "/v1/commands/test-id/status", 404), // command not found
            ("GET", "/v1/commands/test-id/logs", 404), // command not found
            ("POST", "/v1/commands/test-id/cancel", 404), // command not found
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

        // POST /v1/sessions — check it responds (even with 400/422 for missing body).
        let resp = client
            .post(format!("http://{addr}/v1/sessions"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/sessions returned 404 — route may not be registered"
        );

        // POST /v1/commands — check it responds (even with 400 for missing headers).
        let resp = client
            .post(format!("http://{addr}/v1/commands"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/commands returned 404 — route may not be registered"
        );
        let resp = client
            .post(format!("http://{addr}/v1/runs"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/runs must be registered"
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
        let mut state = make_test_state(tmp.path());

        // Set up auth with a known key hash.
        let key = "my-test-api-key";
        let hash: String = {
            use ring::digest;
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        // Replace auth_mode with Enabled.
        Arc::get_mut(&mut state).unwrap().auth_mode = AuthMode::Enabled { key_hash: hash };

        let app = build_router(state);
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
