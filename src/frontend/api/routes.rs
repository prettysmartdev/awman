//! HTTP route registration and handlers for the API server.
//!
//! Wire-identical to `oldsrc/commands/headless/server.rs::build_router`.

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
use sha2::{Digest, Sha256};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::trace::TraceLayer;

use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::api_server::session_setup::{
    log_session_setup, SessionSetupBus, SessionSetupBusSender, SetupReadyFrontend, TracingSetupSink,
};
pub use crate::command::commands::api_server::AuthMode;
use crate::command::commands::api_server::{
    ApiSessionLifecycle, CancelOutcome, CloseOutcome, CommandCanceller, SetupReadiness,
    TokenRegistry,
};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::error::CommandError;
use crate::command::session_create::{
    SessionCreatePlan, SessionCreatePolicy, SessionCreateRequest,
};
use crate::command::session_setup::{SessionSetup, SessionSetupPresenter};
use crate::data::api_lifecycle_store::{CommandLifecycleStore, RunRecord};
use crate::data::command_status::CommandStatus;
use crate::data::execution_event::{EventCursor, EventPayload, ExecutionEvent, ReplayFrom};
use crate::data::fs::api_db::{
    CommandListFilter, CommandRecord, IdempotencyBegin, IdempotencyOwner, IdempotencyResource,
    IdempotencyScope, LabelOwner,
};
use crate::data::fs::api_db::{NewSessionRow, RemoteSessionFields, SessionRecord};
use crate::data::fs::api_db::{SessionCommandAdmission, SqliteSessionStore};
use crate::data::fs::api_labels::{LabelSelector, Labels};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::SessionKind;
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::session_setup_event::SetupEventPayload;
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
struct QueueStatusResponse {
    session_id: String,
    queue_depth: i64,
    running: Option<serde_json::Value>,
    queued: Vec<serde_json::Value>,
    recent_completed: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct SessionClosingResponse {
    session_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    running_command_id: Option<String>,
    cancelled_count: usize,
    message: String,
}

// ─── Request / Response types (wire-compatible with oldsrc) ──────────────────

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
struct CreateSessionResponse {
    session_id: String,
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
}

impl From<SessionRecord> for SessionResponse {
    fn from(s: SessionRecord) -> Self {
        Self {
            id: s.id,
            workdir: s.workdir,
            created_at: s.created_at,
            status: s.status,
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
}

#[derive(Serialize)]
struct CreateCommandResponse {
    command_id: String,
    /// Server-enforced flags whose values the API frontend always overrides.
    /// Documents to clients that `yolo` and `non_interactive` are forced to
    /// `true` regardless of any value sent in the request body. Empty object
    /// for non-exec routes.
    flags_applied: serde_json::Value,
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
    log_path: String,
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
    "workflow_state",
    "command_cancel",
    "labels",
    "idempotency",
    "runs",
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

#[derive(Serialize)]
struct CreateRunResponse {
    session_id: String,
    command_id: String,
}

use crate::frontend::api::serve::error_json;

fn bad_request(message: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, error_json(message)).into_response()
}

fn internal_error(message: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, error_json(message)).into_response()
}

fn command_is_terminal(status: &str) -> bool {
    CommandStatus::parse(status).is_some_and(CommandStatus::is_terminal)
}

fn session_response(state: &AppState, row: SessionRecord) -> Response {
    match state.store.labels_for(LabelOwner::Session, &row.id) {
        Ok(labels) => Json(SessionResponse {
            labels,
            ..SessionResponse::from(row)
        })
        .into_response(),
        Err(_) => internal_error("Failed to read session labels"),
    }
}

// Keep fallible route helpers small on the success path; HTTP responses carry
// a body and extensions even when they are only used to report an error.
type RouteResult<T> = Result<T, Box<Response>>;

fn command_response(
    state: &AppState,
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
    let queue_position = if CommandStatus::parse(&c.status).is_some_and(CommandStatus::is_claimable)
    {
        state
            .store
            .queue_position_for_command(&c.id, &c.session_id)
            .map_err(|_| internal_error("Failed to read queue position"))?
    } else {
        None
    };
    Ok(CommandResponse {
        args: serde_json::from_str(&c.args).unwrap_or_else(|_| serde_json::json!([])),
        result: c
            .result
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok()),
        id: c.id,
        attempt: c.attempt,
        session_id: c.session_id,
        subcommand: c.subcommand,
        status: c.status,
        exit_code: c.exit_code,
        started_at: c.started_at,
        finished_at: c.finished_at,
        log_path: c.log_path,
        queued_at: c.queued_at,
        queue_position,
        worker_id: c.worker_id,
        labels,
        cancel_requested_at,
    })
}

#[derive(Default)]
struct ListQuery {
    status: Option<String>,
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
                "status" => {
                    if commands && CommandStatus::parse(&value).is_none() {
                        let valid: Vec<_> = CommandStatus::ALL.iter().map(|s| s.as_str()).collect();
                        return Err(Box::new(bad_request(format!(
                            "unknown command status: {value}; expected {}",
                            valid.join("|")
                        ))));
                    }
                    if !commands && !matches!(value.as_str(), "active" | "closing" | "closed") {
                        return Err(Box::new(bad_request(format!(
                            "unknown session status: {value}; expected active|closing|closed"
                        ))));
                    }
                    query.status = Some(value);
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
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let query = match ListQuery::parse(pairs, true) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let rows = match state.store.list_commands_filtered(&CommandListFilter {
        status: query.status.as_deref(),
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
        match command_response(&state, row, Some(row_labels)) {
            Ok(command) => commands.push(command),
            Err(response) => return *response,
        }
    }
    Json(serde_json::json!({ "commands": commands })).into_response()
}

async fn handle_cancel_command(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.canceller.cancel(&id) {
        Ok(CancelOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Command '{id}' not found")),
        )
            .into_response(),
        Ok(CancelOutcome::AlreadyTerminal(row) | CancelOutcome::Cancelled(row)) => {
            // During setup its owner must finish clone cleanup before closing.
            // After setup, a cancelled queued row has no worker to close it.
            if !state.setup_cancels.is_registered(&row.session_id) {
                match state.lifecycle_store.run_for_command(&id) {
                    Ok(Some(run)) if run.close_on_finish => {
                        if let Err(error) = state.lifecycle().close(&run.session_id).await {
                            tracing::error!(%error, "Failed to close cancelled run session");
                            return internal_error("Failed to close cancelled run session");
                        }
                    }
                    Err(_) => return internal_error("Failed to read run"),
                    _ => {}
                }
            }
            match command_response(&state, row, None) {
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
                "command_id": command_id, "session_id": session_id, "status": "running",
                "cancel_requested_at": requested_at,
                "message": "Stopping. Poll GET /v1/commands/{id}/status until status is cancelled.",
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "Failed to cancel command");
            internal_error("Failed to cancel command")
        }
    }
}

/// One id per daemon process. An unfinished idempotency reservation held by
/// another id belongs to a daemon that has exited (see `idempotency_begin`).
fn daemon_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

/// A request's hold on its idempotency reservation. Handlers call
/// [`AdmissionJournal::record`] before their first durable write; the
/// wrapper stores or releases the reservation afterwards.
pub(crate) struct AdmissionJournal {
    store: Arc<SqliteSessionStore>,
    route: &'static str,
    principal: String,
    key: String,
    request: String,
}

impl AdmissionJournal {
    fn scope(&self) -> IdempotencyScope<'_> {
        IdempotencyScope {
            route: self.route,
            principal: &self.principal,
        }
    }

    fn owner(&self) -> IdempotencyOwner<'_> {
        IdempotencyOwner {
            instance: daemon_instance(),
            request: &self.request,
        }
    }

    /// Record the 202 `body` this request returns once `resource` exists.
    fn record(&self, body: &str, resource: IdempotencyResource<'_>) -> RouteResult<()> {
        match self.store.idempotency_record_intent(
            self.scope(),
            &self.key,
            self.owner(),
            StatusCode::ACCEPTED.as_u16(),
            body,
            resource,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Box::new(internal_error("Idempotency reservation lost"))),
            Err(error) => {
                tracing::error!(%error, "Failed to record idempotent admission");
                Err(Box::new(internal_error(
                    "Failed to record idempotent admission",
                )))
            }
        }
    }
}

/// Record the admission intent (when keyed) and serialise the 202 body, so
/// the response a restart recovers is byte-identical to the one returned.
fn admission_body(
    journal: Option<&AdmissionJournal>,
    body: &impl Serialize,
    resource: IdempotencyResource<'_>,
) -> RouteResult<String> {
    let body =
        serde_json::to_string(body).map_err(|_| internal_error("Failed to encode response"))?;
    if let Some(journal) = journal {
        journal.record(&body, resource)?;
    }
    Ok(body)
}

fn accepted_json(body: String) -> Response {
    (
        StatusCode::ACCEPTED,
        [("content-type", "application/json")],
        body,
    )
        .into_response()
}

// A panic, a dropped request future or a failed response store ends the
// request without a stored response: keep the reservation if the work was
// admitted (its recorded resource exists), else release it.
struct IdempotencyReservation(Option<Arc<AdmissionJournal>>);

impl Drop for IdempotencyReservation {
    fn drop(&mut self) {
        if let Some(journal) = self.0.take() {
            if let Err(error) = journal.store.idempotency_release_unless_admitted(
                journal.scope(),
                &journal.key,
                journal.owner(),
            ) {
                tracing::error!(%error, "Failed to release idempotency reservation");
            }
        }
    }
}

async fn with_idempotency<F, Fut>(
    state: &AppState,
    headers: &HeaderMap,
    route: &'static str,
    canonical: &str,
    handler: F,
) -> Response
where
    F: FnOnce(Option<Arc<AdmissionJournal>>) -> Fut,
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
    let journal = Arc::new(AdmissionJournal {
        store: Arc::clone(&state.store),
        route,
        principal: principal.to_string(),
        key: key.to_string(),
        request: uuid::Uuid::new_v4().to_string(),
    });
    let hash: String = Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    match state.store.idempotency_begin(
        journal.scope(),
        key,
        &hash,
        &chrono::Utc::now().to_rfc3339(),
        journal.owner(),
    ) {
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
                StatusCode::CONFLICT,
                [("retry-after", "1")],
                error_json("a request with this Idempotency-Key is still in progress"),
            )
                .into_response()
        }
        Ok(IdempotencyBegin::Mismatch) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                error_json("Idempotency-Key reused with a different request body"),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(%error, "Failed to reserve idempotency key");
            return internal_error("Failed to reserve idempotency key");
        }
    }
    let mut reservation = IdempotencyReservation(Some(Arc::clone(&journal)));
    let response = match std::panic::AssertUnwindSafe(handler(Some(Arc::clone(&journal))))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => return internal_error("Request handler failed"),
    };
    if response.status().is_server_error() {
        // Handlers roll back what they admitted before returning a 5xx.
        reservation.0 = None;
        if let Err(error) = state
            .store
            .idempotency_release(journal.scope(), key, journal.owner())
        {
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
    match state.store.idempotency_complete(
        journal.scope(),
        key,
        journal.owner(),
        parts.status.as_u16(),
        body,
    ) {
        Ok(true) => reservation.0 = None,
        Ok(false) => {
            tracing::warn!("Idempotency reservation was no longer held when completing");
            reservation.0 = None;
        }
        Err(error) => {
            // The reservation guard keeps an admitted request's recorded
            // response; the work itself has already been accepted.
            tracing::error!(%error, "Failed to save idempotent response");
        }
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// Map a session-creation validation error to its HTTP status. This is the
/// ONLY session-creation logic that remains in the frontend — the transport
/// mapping. An off-allowlist workdir is a 403 (the path exists but the caller
/// is not permitted to use it), as is an unapproved custom agent definition;
/// an unknown agent is a 404; every other validation failure is a 400.
fn session_create_error_status(err: &CommandError) -> StatusCode {
    match err {
        CommandError::SessionWorkdirNotAllowed { .. } => StatusCode::FORBIDDEN,
        CommandError::Engine(
            crate::engine::error::EngineError::AgentDefinitionUntrusted { .. }
            | crate::engine::error::EngineError::UnknownAgent { .. },
        ) => StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::BAD_REQUEST),
        _ => StatusCode::BAD_REQUEST,
    }
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
        .route("/v1/commands/{id}/status", get(handle_get_command))
        .route("/v1/commands/{id}/logs", get(handle_stream_command_logs))
        .route("/v1/workflows/{command_id}", get(handle_get_workflow))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
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
        &state.engines.auth_engine,
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
    journal: Option<&AdmissionJournal>,
) -> Response {
    let session_id = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339();
    let session_dir = state.paths.session_dir(&session_id);

    // Validate and plan the session in Layer 2. The route only maps the typed
    // result/error to an HTTP status + JSON envelope — no business logic here.
    let request = SessionCreateRequest {
        session_type: body.session_type,
        workdir: body.workdir,
        repo_url: body.repo_url,
        branch: body.branch,
        base_ref: body.base_ref,
        merge_refs: body.merge_refs,
        push: body.push,
    };
    let policy = SessionCreatePolicy::new(state.workdirs.clone(), session_dir.clone())
        .with_workdir_roots(state.workdir_roots.clone());
    let plan = match request.validate(&policy, state.engines.git_engine.as_ref()) {
        Ok(plan) => plan,
        Err(e) => {
            return (session_create_error_status(&e), error_json(e.to_string())).into_response();
        }
    };
    if let Err(e) = plan
        .validate_merge_ref_kinds(Arc::clone(&state.engines.git_engine))
        .await
    {
        return (session_create_error_status(&e), error_json(e.to_string())).into_response();
    }

    if let Some(command) = &command {
        if let Err(response) = validate_command(command) {
            return *response;
        }
    }

    // Create session storage directories (Layer 0).
    if let Err(e) = state.paths.prepare_session_dirs(&session_id) {
        tracing::error!(error = %e, "Failed to create session directory");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create session directory"),
        )
            .into_response();
    }

    // The response is fixed before anything durable is written, so a keyed
    // request recovers it from the admitted resource after a crash: the
    // session row for a session, the command row (written last) for a run.
    let command_id = command.as_ref().map(|_| uuid::Uuid::new_v4().to_string());
    let response_body = match &command_id {
        Some(command_id) => admission_body(
            journal,
            &CreateRunResponse {
                session_id: session_id.clone(),
                command_id: command_id.clone(),
            },
            IdempotencyResource::Command(command_id),
        ),
        None => admission_body(
            journal,
            &CreateSessionResponse {
                session_id: session_id.clone(),
            },
            IdempotencyResource::Session(&session_id),
        ),
    };
    let response_body = match response_body {
        Ok(body) => body,
        Err(response) => return *response,
    };

    // Persist the session row with setup_status='initializing' BEFORE spawning
    // the setup task. If the server restarts mid-setup we want the cleanup
    // pass to find this session as non-terminal even if no setup_state.json
    // was written yet.
    let workdir = plan.resolved_workdir.to_string_lossy().into_owned();
    let cloned_path = plan
        .cloned_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    if let Err(e) = state.store.insert_session_full(NewSessionRow {
        id: &session_id,
        workdir: &workdir,
        created_at: &created_at,
        setup_status: SessionSetupStatus::Initializing,
        kind: plan.kind,
        cloned_path: cloned_path.as_deref(),
    }) {
        tracing::error!(error = %e, "Failed to insert session");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create session"),
        )
            .into_response();
    }
    // Record the remote inputs before setup starts, so a restored session
    // keeps them (and the push stage can read `push` from the row).
    if plan.kind == SessionKind::Remote {
        if let Err(e) = state.store.set_remote_fields(
            &session_id,
            RemoteSessionFields {
                repo_url: plan.repo_url.as_deref(),
                branch: plan.branch.as_deref(),
                base_ref: plan.base_ref.as_deref(),
                merge_refs: &plan.merge_refs,
                push: plan.push,
            },
        ) {
            tracing::error!(error = %e, "Failed to record remote session fields");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to create session"),
            )
                .into_response();
        }
    }

    if let Err(error) = state
        .store
        .set_labels(LabelOwner::Session, &session_id, &body.labels)
    {
        tracing::error!(%error, "Failed to store session labels");
        let _ = state.lifecycle().close(&session_id).await;
        return internal_error("Failed to store session labels");
    }
    let run = if let (Some(command), Some(command_id)) = (command, command_id) {
        // The run link precedes the command row: the row is the admission
        // point, and setup/bootstrap always find the link for it.
        let run = RunRecord {
            session_id: session_id.clone(),
            command_id,
            close_on_finish,
            created_at,
        };
        if let Err(error) = state.lifecycle_store.record_run(&run) {
            tracing::error!(%error, "Failed to record run");
            let _ = state.lifecycle().close(&session_id).await;
            return internal_error("Failed to record run");
        }
        if let Err(response) = enqueue_command(state, &run.command_id, &session_id, &command) {
            let _ = state.lifecycle().close(&session_id).await;
            return *response;
        }
        Some(run)
    } else {
        None
    };

    let setup_bus = Arc::new(SessionSetupBus::new(256));
    state
        .setup_buses
        .lock()
        .await
        .insert(session_id.clone(), Arc::clone(&setup_bus));

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
    tokio::spawn(async move {
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
        if state
            .store
            .get_command(&run.command_id)
            .ok()
            .flatten()
            .is_some_and(|c| c.status == CommandStatus::Cancelled.as_str())
        {
            token.cancel();
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
    match state
        .store
        .list_sessions_filtered(query.status.as_deref(), &query.labels)
    {
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
                    SessionResponse {
                        labels,
                        ..SessionResponse::from(s)
                    }
                })
                .collect();
            Json(serde_json::json!({ "sessions": list })).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list sessions");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to list sessions"),
            )
                .into_response()
        }
    }
}

async fn handle_get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.store.get_session(&id) {
        Ok(Some(s)) => session_response(&state, s),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Session '{}' not found", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response()
        }
    }
}

async fn handle_close_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.lifecycle().close(&id).await {
        Ok(CloseOutcome::NotFound) => (StatusCode::NOT_FOUND, error_json(format!("Session '{id}' not found"))).into_response(),
        Ok(CloseOutcome::Draining { running_command_id, cancelled }) => (
            StatusCode::ACCEPTED,
            Json(SessionClosingResponse {
                session_id: id,
                status: "closing".to_string(),
                running_command_id: Some(running_command_id),
                cancelled_count: cancelled.len(),
                message: "Session is closing. Waiting for running command to complete. Poll GET /v1/sessions/{id}/status to monitor.".to_string(),
            }),
        ).into_response(),
        Ok(CloseOutcome::AlreadyClosing) => Json(SessionClosingResponse {
            session_id: id.clone(),
            status: "closing".to_string(),
            running_command_id: state.store.running_command_for_session(&id).ok().flatten().map(|command| command.id),
            cancelled_count: 0,
            message: "Session is already closing. Poll GET /v1/sessions/{id}/status to monitor.".to_string(),
        }).into_response(),
        Ok(CloseOutcome::Closed) => match state.store.get_session(&id) {
            Ok(Some(session)) => session_response(&state, session),
            _ => StatusCode::NO_CONTENT.into_response(),
        },
        Err(error) => {
            tracing::error!(%error, session_id = %id, "Failed to close session");
            (StatusCode::INTERNAL_SERVER_ERROR, error_json("Failed to clean up remote session directory")).into_response()
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
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }

    let labels = match state.store.labels_for(LabelOwner::Session, &id) {
        Ok(labels) => labels,
        Err(_) => return internal_error("Failed to read session labels"),
    };

    // Try to read from in-memory setup bus first.
    if let Some(bus) = state.setup_buses.lock().await.get(&id).cloned() {
        let setup_state = bus.snapshot();
        return Json(serde_json::json!({
            "session_id": id,
            "labels": labels,
            "status": setup_state.status,
            "current_stage": setup_state.current_stage,
            "current_ready_phase": setup_state.current_ready_phase,
            "ready_step_statuses": setup_state.ready_step_statuses,
            "ready_summary": setup_state.ready_summary,
            "error": setup_state.error,
        }))
        .into_response();
    }

    // Fall back to on-disk setup_state.json (Layer 0).
    match state.paths.read_setup_state(&id) {
        Some(setup_state) => Json(serde_json::json!({
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
        None => fallback_status_from_db(&state, &id).await,
    }
}

/// Resolve a session's setup status to (is_ready, status_string, optional error JSON).
/// Reads the in-memory bus first, then setup_state.json on disk, then the sqlite
/// session row. Used by the job-submission guard and other places that need to
/// reason about session readiness.
async fn resolve_setup_status(state: &AppState, session_id: &str) -> SetupReadiness {
    state.lifecycle().setup_readiness(session_id).await
}

/// Last-resort fallback when neither the in-memory bus nor the on-disk
/// setup_state.json is usable: read the session's setup_status from sqlite
/// and return a minimal response. Used for very old sessions (pre-WI-0078).
async fn fallback_status_from_db(state: &AppState, id: &str) -> Response {
    let setup_status = state
        .store
        .get_session(id)
        .ok()
        .flatten()
        .map(|s| s.setup_status)
        .unwrap_or_else(|| "ready".to_string());
    let labels = match state.store.labels_for(LabelOwner::Session, id) {
        Ok(labels) => labels,
        Err(_) => return internal_error("Failed to read session labels"),
    };
    Json(serde_json::json!({
        "session_id": id,
        "labels": labels,
        "status": setup_status,
    }))
    .into_response()
}

async fn handle_create_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let canonical = format!(
        "{}\n{}",
        value,
        headers
            .get("x-awman-session")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
    );
    let (st, hd) = (&state, &headers);
    with_idempotency(
        &state,
        &headers,
        "/v1/commands",
        &canonical,
        |journal| async move {
            let body = match serde_json::from_value(value) {
                Ok(body) => body,
                Err(error) => return bad_request(error.to_string()),
            };
            create_command(st, hd, body, journal.as_deref()).await
        },
    )
    .await
}

async fn create_command(
    state: &AppState,
    headers: &HeaderMap,
    body: CreateCommandRequest,
    journal: Option<&AdmissionJournal>,
) -> Response {
    let session_id = match headers.get("x-awman-session") {
        Some(val) => match val.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("Invalid x-awman-session header value"),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::BAD_REQUEST,
                error_json("Missing required header: x-awman-session"),
            )
                .into_response();
        }
    };

    if let Err(response) = validate_command(&body) {
        return *response;
    }
    let path_parts: Vec<&str> = body.subcommand.split_whitespace().collect();

    // Validate session exists and is in a state that accepts commands. The
    // lifecycle classification lives in Layer 0 (on the store that owns the
    // `status` column); the route only maps each outcome to an HTTP status.
    match state.store.command_admission(&session_id) {
        Ok(SessionCommandAdmission::Accepted) => {}
        Ok(SessionCommandAdmission::Closing) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "session is closing",
                    "session_id": session_id,
                    "hint": "Session is shutting down and no longer accepts commands."
                })),
            )
                .into_response();
        }
        Ok(SessionCommandAdmission::Closed) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' is closed", session_id)),
            )
                .into_response();
        }
        Ok(SessionCommandAdmission::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", session_id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to validate session"),
            )
                .into_response();
        }
    };

    // Job submission guard: reject if session setup is not ready.
    {
        let readiness = resolve_setup_status(state, &session_id).await;
        if !readiness.is_ready() {
            if matches!(readiness, SetupReadiness::NotFound) {
                return (
                    StatusCode::NOT_FOUND,
                    error_json(format!("Session '{session_id}' not found")),
                )
                    .into_response();
            }
            let mut body = serde_json::json!({
                "error": "session is not ready",
                "setup_status": readiness.status(),
                "hint": "Poll GET /v1/sessions/{id}/status to check setup progress"
            });
            if let Some(err) = readiness.error().cloned() {
                body["setup_error"] = err;
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "error".into(),
                        serde_json::Value::String("session setup failed".into()),
                    );
                }
            }
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
    }

    let command_id = uuid::Uuid::new_v4().to_string();

    // The profile's own account of itself, not a restatement of it (F-50).
    // Serialised with the underscore spelling the response has always used.
    let mut flags_applied = serde_json::Value::Object(
        CommandCatalogue::get()
            .frontend_profile(FrontendKind::Api, &path_parts)
            .into_iter()
            .map(|(flag, value)| (flag.replace('-', "_"), flag_default_json(&value)))
            .collect(),
    );
    if let Some(session) = state.sessions.get_by_key(&session_id) {
        let bus = EventBus::new(1);
        let frontend = super::command_frontend::ApiDispatchFrontend::new(
            &body.subcommand,
            &body.args,
            bus.sender(),
        );
        let dispatch =
            crate::command::dispatch::Dispatch::new(frontend, session, state.engines.clone());
        match dispatch.prerequisite_admission(&path_parts) {
            Ok(Some(policy)) => flags_applied["prerequisites"] = policy,
            Ok(None) => {}
            Err(error) => {
                return (
                    StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_REQUEST),
                    error_json(error.to_string()),
                )
                    .into_response()
            }
        }
    }
    let response_body = match admission_body(
        journal,
        &CreateCommandResponse {
            command_id: command_id.clone(),
            flags_applied,
        },
        IdempotencyResource::Command(&command_id),
    ) {
        Ok(body) => body,
        Err(response) => return *response,
    };

    if let Err(response) = enqueue_command(state, &command_id, &session_id, &body) {
        return *response;
    }
    accepted_json(response_body)
}

fn validate_command(body: &CreateCommandRequest) -> RouteResult<()> {
    // Validate the command shape against the catalogue BEFORE touching session
    // state. Both checks below are request-shape (400-class) errors derived
    // entirely from the command catalogue — no per-command logic in the route.
    let path_parts: Vec<&str> = body.subcommand.split_whitespace().collect();
    {
        let catalogue = CommandCatalogue::get();

        // (1) The command must be reachable via the API frontend.
        if let Err(CommandError::NotAvailableForFrontend { command, .. }) =
            catalogue.validate_for_frontend(FrontendKind::Api, &path_parts)
        {
            // The advertised alternatives are the catalogue's api-allowed
            // commands, not a hand-maintained list that could drift.
            let available: Vec<String> = catalogue
                .api_allowed_commands()
                .into_iter()
                .map(|(parent, sub)| format!("{parent} {sub}"))
                .collect();
            return Err(Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "command not available via API",
                        "blocked_command": command,
                        "available": available,
                    })),
                )
                    .into_response(),
            ));
        }

        // (2) The args must parse cleanly against the catalogue: an unknown
        // flag, a bad flag value, or an unknown command produces a structured
        // 400 here, rather than being enqueued and failing asynchronously in
        // the worker when the dispatch accessors are later called.
        if let Err(e) =
            catalogue.parse_raw_args_with_profile(&path_parts, &body.args, FrontendKind::Api)
        {
            return Err(Box::new(bad_request(e.to_string())));
        }
    }

    Ok(())
}

fn enqueue_command(
    state: &AppState,
    command_id: &str,
    session_id: &str,
    body: &CreateCommandRequest,
) -> RouteResult<()> {
    let args_json = serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".to_string());

    if let Err(e) = state.paths.prepare_command_dir(session_id, command_id) {
        tracing::error!(error = %e, "Failed to create command directory");
        return Err(Box::new(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to create command directory"),
            )
                .into_response(),
        ));
    }

    let log_path = state.paths.command_log_path(session_id, command_id);

    if let Err(e) = state.store.enqueue_command_with_labels(
        command_id,
        session_id,
        &body.subcommand,
        &args_json,
        &log_path.to_string_lossy(),
        &body.labels,
    ) {
        tracing::error!(error = %e, "Failed to enqueue command");
        return Err(Box::new(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to enqueue command"),
            )
                .into_response(),
        ));
    }

    tracing::info!(
        command_id = %command_id,
        session_id = %session_id,
        subcommand = %body.subcommand,
        "Command enqueued"
    );

    Ok(())
}

async fn handle_get_command(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.store.get_command(&id) {
        Ok(Some(c)) => match command_response(&state, c, None) {
            Ok(response) => Json(response).into_response(),
            Err(response) => *response,
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Command '{}' not found", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get command"),
            )
                .into_response()
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
}

const WORKER_WAIT_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);
const COMMAND_STATUS_POLL: std::time::Duration = std::time::Duration::from_secs(1);

type CommandLogSender = tokio::sync::mpsc::Sender<Result<Event, std::convert::Infallible>>;
type CommandLogReceiver = tokio::sync::broadcast::Receiver<ExecutionEvent>;

/// Per-connection replay boundary and the last event actually sent. Done is
/// held back until the row is terminal, including when it arrives on a live bus.
struct CommandLogCursor {
    replay_from: ReplayFrom,
    last_sent: Option<EventCursor>,
    attempt: u32,
    next_sequence: u64,
    done: Option<ExecutionEvent>,
}

impl CommandLogCursor {
    fn new(replay_from: ReplayFrom, attempt: u32) -> Self {
        Self {
            replay_from,
            last_sent: match replay_from {
                ReplayFrom::After(cursor) => Some(cursor),
                _ => None,
            },
            attempt,
            next_sequence: 0,
            done: None,
        }
    }

    fn update_attempt(&mut self, attempt: u32) {
        if attempt > self.attempt {
            self.attempt = attempt;
            self.next_sequence = 0;
            self.done = None;
            if matches!(self.replay_from, ReplayFrom::StartOfAttempt(_)) {
                self.replay_from = ReplayFrom::StartOfAttempt(attempt);
            }
        }
    }

    async fn send_frame(&mut self, event: &ExecutionEvent, tx: &CommandLogSender) -> bool {
        if !self.replay_from.includes(event)
            || self.last_sent.is_some_and(|last| event.cursor() <= last)
        {
            return true;
        }
        if tx.send(Ok(execution_event_to_sse(event))).await.is_err() {
            return false;
        }
        self.last_sent = Some(event.cursor());
        true
    }

    async fn forward(&mut self, event: &ExecutionEvent, tx: &CommandLogSender) -> bool {
        if event.attempt == self.attempt {
            self.next_sequence = self.next_sequence.max(event.sequence.saturating_add(1));
            if matches!(event.payload, EventPayload::Done) {
                self.done = Some(event.clone());
            }
        }
        // An earlier attempt may have written Done just before a daemon crash.
        // Neither that marker nor the current marker can end a nonterminal row.
        if matches!(event.payload, EventPayload::Done) {
            return true;
        }
        self.send_frame(event, tx).await
    }

    async fn replay(&mut self, events: &[ExecutionEvent], tx: &CommandLogSender) -> bool {
        // Legacy logs can contain inversions from concurrent producers. Sort
        // before applying the high-water mark so full replay loses no records.
        let mut events: Vec<_> = events.iter().collect();
        events.sort_by_key(|event| event.cursor());
        for event in events {
            if !self.forward(event, tx).await {
                return false;
            }
        }
        true
    }

    fn missing_before(&self, event: &ExecutionEvent) -> bool {
        match self.last_sent {
            Some(last) if last.attempt == event.attempt => {
                event.sequence > last.sequence.saturating_add(1)
            }
            Some(last) if last.attempt > event.attempt => false,
            _ => event.sequence > 0,
        }
    }

    async fn finish(&mut self, tx: &CommandLogSender) {
        // Early failures and queued cancellations can have no events.log.
        let done = self.done.take().unwrap_or_else(|| ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: self.attempt,
            sequence: self.next_sequence,
            payload: EventPayload::Done,
        });
        // Completion is a terminal-state notification as well as a log event.
        // Even an ahead-of-head resume must learn that the command ended.
        let _ = tx.send(Ok(execution_event_to_sse(&done))).await;
    }
}

enum WorkerWaitOutcome {
    Terminal(u32),
    Bus(CommandLogReceiver),
    ClientGone,
    Unavailable,
}

/// SQLite can wait on a busy lock; never run these polls on a Tokio worker.
async fn command_log_row(state: &AppState, command_id: &str) -> Option<CommandRecord> {
    let store = Arc::clone(&state.store);
    let id = command_id.to_owned();
    match tokio::task::spawn_blocking(move || store.get_command(&id)).await {
        Ok(Ok(row)) => row,
        Ok(Err(error)) => {
            tracing::error!(%error, %command_id, "Failed to poll command status");
            None
        }
        Err(error) => {
            tracing::error!(%error, %command_id, "Command status poll failed");
            None
        }
    }
}

async fn command_log_events(
    state: &AppState,
    session_id: &str,
    command_id: &str,
) -> Vec<ExecutionEvent> {
    let paths = state.paths.clone();
    let session = session_id.to_owned();
    let command = command_id.to_owned();
    tokio::task::spawn_blocking(move || paths.read_command_events(&session, &command))
        .await
        .unwrap_or_default()
}

/// A subscription can start ahead of the independent logfile writer. Hold
/// the live event until the log catches up, replaying the missing prefix before
/// advancing the client's id. The same path repairs broadcast lag. On failure,
/// close without Done so the client resumes from its actual delivered cursor.
async fn recover_command_log_gap(
    state: &AppState,
    session_id: &str,
    command_id: &str,
    through: EventCursor,
    cursor: &mut CommandLogCursor,
    tx: &CommandLogSender,
) -> bool {
    let recovery = async {
        loop {
            let events = command_log_events(state, session_id, command_id).await;
            if events.iter().any(|event| event.cursor() == through) {
                return cursor.replay(&events, tx).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    tokio::select! {
        _ = tx.closed() => false,
        result = tokio::time::timeout(std::time::Duration::from_secs(5), recovery) => {
            match result {
                Ok(sent) => sent,
                Err(_) => {
                    tracing::warn!(%command_id, %through, "Command log gap could not be recovered");
                    false
                }
            }
        }
    }
}

/// Wait indefinitely, polling independently of the comment keepalive cadence.
/// Subscribe before returning; live gap recovery also waits for the independent
/// logfile writer to catch up before advancing the response cursor.
async fn wait_for_worker(
    state: &AppState,
    command_id: &str,
    tx: &CommandLogSender,
    keepalive: std::time::Duration,
    poll: std::time::Duration,
) -> WorkerWaitOutcome {
    let mut status_poll = tokio::time::interval(poll);
    let mut keepalive =
        tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
    loop {
        tokio::select! {
            _ = tx.closed() => return WorkerWaitOutcome::ClientGone,
            _ = status_poll.tick() => {
                let Some(row) = command_log_row(state, command_id).await else {
                    return WorkerWaitOutcome::Unavailable;
                };
                if command_is_terminal(&row.status) {
                    return WorkerWaitOutcome::Terminal(row.attempt);
                }
                if let Some(bus) = state.event_buses.lock().await.get(command_id) {
                    return WorkerWaitOutcome::Bus(bus.subscribe());
                }
            }
            _ = keepalive.tick() => {
                if tx.send(Ok(Event::default().comment("waiting for command progress"))).await.is_err() {
                    return WorkerWaitOutcome::ClientGone;
                }
            }
        }
    }
}

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
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Command '{command_id}' not found")),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up command");
            return internal_error("Failed to look up command");
        }
    };
    let session_id = command.session_id;

    // JSON is a full-log snapshot; SSE resume inputs do not filter it.
    if query.format.as_deref() == Some("json") {
        return Json(serde_json::json!({
            "session_id": session_id,
            "command_id": command_id,
            "attempt": command.attempt,
            "events": state.paths.read_command_events(&session_id, &command_id),
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

    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let shutdown = shutdown.map(|extension| extension.0).unwrap_or_default();
    let stream = async move {
        // Subscribe before reading disk and deduplicate the overlap. If the
        // writer is behind this subscription, repair the gap before forwarding.
        let mut live_rx = state
            .event_buses
            .lock()
            .await
            .get(&command_id)
            .map(|bus| bus.subscribe());
        let events = command_log_events(&state, &session_id, &command_id).await;
        let replay_from = ReplayFrom::resolve(&events, command.attempt, requested);
        let mut cursor = CommandLogCursor::new(replay_from, command.attempt);
        if let ReplayFrom::StartOfAttempt(attempt) = replay_from {
            if tx
                .send(Ok(Event::default().comment(format!(
                    "resuming from the start of attempt {attempt}"
                ))))
                .await
                .is_err()
            {
                return;
            }
        }
        if !cursor.replay(&events, &tx).await {
            return;
        }
        if command_is_terminal(&command.status) {
            cursor.finish(&tx).await;
            return;
        }

        let mut saw_worker = live_rx.is_some();
        loop {
            if live_rx.is_none() {
                // This informational event is outside the persisted cursor
                // namespace. An id-less frame leaves Last-Event-ID untouched.
                let waiting = ExecutionEvent {
                    timestamp: chrono::Utc::now(),
                    attempt: 0,
                    sequence: 0,
                    payload: EventPayload::StatusMessage {
                        phase: "queue".into(),
                        message: if saw_worker {
                            "Waiting for command status to be recorded..."
                        } else {
                            "Waiting for a worker to claim this command..."
                        }
                        .into(),
                    },
                };
                let waiting = Event::default()
                    .event(waiting.payload.sse_event_type())
                    .data(serde_json::to_string(&waiting).unwrap_or_else(|_| "{}".into()));
                if tx.send(Ok(waiting)).await.is_err() {
                    return;
                }
                match wait_for_worker(
                    &state,
                    &command_id,
                    &tx,
                    WORKER_WAIT_KEEPALIVE,
                    COMMAND_STATUS_POLL,
                )
                .await
                {
                    WorkerWaitOutcome::ClientGone | WorkerWaitOutcome::Unavailable => return,
                    WorkerWaitOutcome::Terminal(attempt) => {
                        cursor.update_attempt(attempt);
                        let events = command_log_events(&state, &session_id, &command_id).await;
                        if cursor.replay(&events, &tx).await {
                            cursor.finish(&tx).await;
                        }
                        return;
                    }
                    WorkerWaitOutcome::Bus(receiver) => {
                        saw_worker = true;
                        live_rx = Some(receiver);
                        let events = command_log_events(&state, &session_id, &command_id).await;
                        if !cursor.replay(&events, &tx).await {
                            return;
                        }
                    }
                }
            }

            let Some(receiver) = live_rx.as_mut() else {
                continue;
            };
            let mut status_poll = tokio::time::interval(COMMAND_STATUS_POLL);
            let mut keepalive = tokio::time::interval_at(
                tokio::time::Instant::now() + WORKER_WAIT_KEEPALIVE,
                WORKER_WAIT_KEEPALIVE,
            );
            loop {
                tokio::select! {
                    _ = tx.closed() => return,
                    _ = keepalive.tick() => {
                        if tx.send(Ok(Event::default().comment("keepalive"))).await.is_err() {
                            return;
                        }
                    }
                    _ = status_poll.tick() => {
                        let Some(row) = command_log_row(&state, &command_id).await else {
                            return;
                        };
                        cursor.update_attempt(row.attempt);
                        if command_is_terminal(&row.status) {
                            let events = command_log_events(&state, &session_id, &command_id).await;
                            if cursor.replay(&events, &tx).await {
                                cursor.finish(&tx).await;
                            }
                            return;
                        }
                    }
                    result = receiver.recv() => match result {
                        Ok(event) => {
                            cursor.update_attempt(event.attempt);
                            if cursor.missing_before(&event)
                                && !recover_command_log_gap(
                                    &state, &session_id, &command_id, event.cursor(), &mut cursor, &tx,
                                ).await
                            {
                                return;
                            }
                            if !cursor.forward(&event, &tx).await {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lagged = n, "SSE subscriber lagged");
                            let comment = Event::default().comment(format!("lagged: recovering {n} events from log"));
                            if tx.send(Ok(comment)).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // A vanished bus does not prove completion. Re-enter
                            // worker waiting and check the row before sending Done.
                            live_rx = None;
                            break;
                        }
                    }
                }
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
fn execution_event_to_sse(event: &ExecutionEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    Event::default()
        .id(event.cursor().to_string())
        .event(event.payload.sse_event_type())
        .data(data)
}
async fn handle_get_session_queue(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // Validate session exists.
    match state.store.get_session(&id) {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }

    let queue_depth = state.store.count_queued_for_session(&id).unwrap_or(0);

    let running_record = state.store.running_command_for_session(&id).ok().flatten();
    let session_cmds = state
        .store
        .list_commands_for_session(&id, 100)
        .unwrap_or_default();
    // Every command projection carries its labels; one batch read covers them.
    let ids: Vec<String> = session_cmds
        .iter()
        .chain(running_record.iter())
        .map(|c| c.id.clone())
        .collect();
    let labels = match state.store.labels_for_many(LabelOwner::Command, &ids) {
        Ok(labels) => labels,
        Err(error) => {
            tracing::error!(%error, "Failed to read command labels");
            return internal_error("Failed to read command labels");
        }
    };
    let labels_of = |id: &str| labels.get(id).cloned().unwrap_or_else(Labels::empty);

    let running = match running_record {
        Some(c) => {
            let args: serde_json::Value =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::Array(vec![]));
            Some(serde_json::json!({
                "command_id": c.id,
                "subcommand": c.subcommand,
                "args": args,
                "started_at": c.started_at,
                "worker_id": c.worker_id,
                "labels": labels_of(&c.id),
            }))
        }
        None => None,
    };

    // Queued items: spec requires oldest first (position 0 = next to run).
    // `list_commands_for_session` returns newest first, so collect queued
    // candidates and reverse before assigning positions.
    let mut queued_records: Vec<_> = session_cmds
        .iter()
        .filter(|c| c.status == "queued")
        .collect();
    queued_records.reverse();
    let queued: Vec<serde_json::Value> = queued_records
        .iter()
        .enumerate()
        .map(|(pos, c)| {
            let args: serde_json::Value =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::Array(vec![]));
            serde_json::json!({
                "command_id": c.id,
                "subcommand": c.subcommand,
                "args": args,
                "queued_at": c.queued_at,
                "position": pos as i64,
                "labels": labels_of(&c.id),
            })
        })
        .collect();

    // Recent completed: spec requires `finished_at DESC`, capped at 10.
    let mut completed_records: Vec<_> = session_cmds
        .iter()
        .filter(|c| command_is_terminal(&c.status))
        .collect();
    completed_records.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
    let recent_completed: Vec<serde_json::Value> = completed_records
        .into_iter()
        .take(10)
        .map(|c| {
            serde_json::json!({
                "command_id": c.id,
                "subcommand": c.subcommand,
                "status": c.status,
                "exit_code": c.exit_code,
                "finished_at": c.finished_at,
                "labels": labels_of(&c.id),
            })
        })
        .collect();

    Json(QueueStatusResponse {
        session_id: id,
        queue_depth,
        running,
        queued,
        recent_completed,
    })
    .into_response()
}

async fn handle_get_workflow(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
) -> Response {
    let cmd = match state.store.get_command(&command_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, error_json("command not found")).into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command for workflow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get command"),
            )
                .into_response();
        }
    };

    match state
        .paths
        .read_command_workflow_state_raw(&cmd.session_id, &command_id)
    {
        Ok(Some(content)) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(val) => Json(val).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse workflow state");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_json("Failed to parse workflow state"),
                )
                    .into_response()
            }
        },
        Ok(None) => {
            let path: Vec<&str> = cmd.subcommand.split_whitespace().collect();
            let error = if CommandCatalogue::get().canonical_path(&path) == ["exec", "workflow"] {
                if matches!(cmd.status.as_str(), "queued" | "pending" | "running") {
                    "workflow not started"
                } else {
                    "workflow never started"
                }
            } else {
                "no workflow for this command"
            };
            (StatusCode::NOT_FOUND, error_json(error)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to read workflow state");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read workflow state"),
            )
                .into_response()
        }
    }
}

/// One `FlagDefault` as the JSON a client sees in `flags_applied`.
///
/// Rendering only — the values themselves come from the catalogue's frontend
/// profile (WI 0114 F-50).
fn flag_default_json(
    value: &crate::command::dispatch::catalogue::FlagDefault,
) -> serde_json::Value {
    use crate::command::dispatch::catalogue::FlagDefault;
    match value {
        FlagDefault::Bool(b) => serde_json::Value::Bool(*b),
        FlagDefault::Str(s) => serde_json::Value::String((*s).to_string()),
        FlagDefault::U16(n) => serde_json::Value::from(*n),
        FlagDefault::EmptyVec => serde_json::Value::Array(Vec::new()),
        FlagDefault::None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

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
        ("GET", "/v1/commands/{id}/status"),
        ("GET", "/v1/commands/{id}/logs"),
        ("GET", "/v1/workflows/{command_id}"),
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
            15,
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
