//! HTTP transport for the squad daemon.
//!
//! This module intentionally contains no task validation or scheduling
//! policy.  The command route delegates both parsing and execution to
//! `CommandCatalogue` and `Dispatch`; the two read routes expose daemon state.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower_http::trace::TraceLayer;

use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::squad::commands::SquadOutcome;
use crate::command::commands::squad::daemon_runtime::SquadWorkflowLookup;
use crate::command::commands::squad::gateway::EnvPush;
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::{CommandOutcome, Dispatch};
use crate::frontend::api::command_frontend::ApiDispatchFrontend;
use crate::frontend::api::serve::{check_bearer_auth, error_json};

use super::state::SquadAppState;

#[derive(Deserialize)]
struct CreateCommandRequest {
    subcommand: String,
    args: Vec<String>,
}

/// Build squad's independent router.  It is never mounted below the API router.
pub fn build_router(state: Arc<SquadAppState>) -> Router {
    Router::new()
        .route("/v1/commands", post(handle_command))
        .route("/v1/status", get(handle_status))
        // WI 0116 §4. A **dedicated typed route**, deliberately not the
        // `{subcommand, args: Vec<String>}` envelope of `/v1/commands`:
        // CLI-arg-shaped strings drift into tracing spans and error text, a
        // typed body whose `Debug` prints names only does not. It sits behind
        // the same `auth_middleware` as everything else, so a daemon started
        // with `--dangerously-skip-auth` follows whatever that mode decides and
        // no new auth surface is introduced.
        .route(
            "/v1/daemon/env",
            get(handle_env_coverage)
                .post(handle_env_push)
                .delete(handle_env_clear),
        )
        .route("/v1/tasks/{name}/workflow", get(handle_workflow))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// State extraction around the one shared bearer-auth decision, so squad's
/// wire behaviour is API mode's by construction rather than by copy.
async fn auth_middleware(
    State(state): State<Arc<SquadAppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    if let Some(rejection) = check_bearer_auth(
        &state.handles.engines().auth_engine,
        &state.handles.auth_mode(),
        req.headers(),
    ) {
        return rejection;
    }
    next.run(req).await
}

async fn handle_command(
    State(state): State<Arc<SquadAppState>>,
    Json(body): Json<CreateCommandRequest>,
) -> Response {
    let path_parts: Vec<&str> = body.subcommand.split_whitespace().collect();

    // Which commands this daemon serves is the catalogue's answer, under its
    // own frontend kind: `SquadDaemon` admits the squad subtree and nothing
    // else. This route used to compare `path_parts.first()` against the
    // literal `"squad"` itself (WI 0114 F-50).
    let catalogue = CommandCatalogue::get();
    if let Err(error) = catalogue.validate_for_frontend(FrontendKind::SquadDaemon, &path_parts) {
        return (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response();
    }
    if let Err(error) =
        catalogue.parse_raw_args_with_profile(&path_parts, &body.args, FrontendKind::SquadDaemon)
    {
        return (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response();
    }

    // ApiDispatchFrontend is the existing non-interactive Dispatch frontend.
    // Its event bus has no subscriber here: squad deliberately exposes neither
    // an SSE route nor a logs route.
    let frontend =
        ApiDispatchFrontend::new(&body.subcommand, &body.args, EventBus::new(1).sender());
    let outcome = Dispatch::new(
        frontend,
        state.handles.session(),
        state.handles.engines().clone(),
    )
    .with_squad_gateway(state.handles.gateway())
    .run_command(&path_parts)
    .await;
    match outcome {
        // The remote gateway deserializes the command result into the same
        // concrete type used by local callers.  Keep that synchronous payload
        // direct; the `SquadOutcome` enum is an internal Dispatch wrapper.
        Ok(CommandOutcome::Squad(outcome)) => squad_outcome_response(outcome),
        // The catalogue/front-door restriction above make this unreachable;
        // retain a safe error if a future Dispatch change violates the seam.
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("squad command dispatched to an unexpected command family"),
        )
            .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response(),
    }
}

fn squad_outcome_response(outcome: SquadOutcome) -> Response {
    match outcome {
        SquadOutcome::Task(task) | SquadOutcome::Updated(task) => Json(task).into_response(),
        SquadOutcome::Detail(detail) => Json(detail).into_response(),
        SquadOutcome::Tasks(tasks) => Json(tasks).into_response(),
        SquadOutcome::Removed { name, .. } => {
            Json(serde_json::json!({ "name": name })).into_response()
        }
        SquadOutcome::Triggered { name } | SquadOutcome::Canceled { name } => {
            Json(serde_json::json!({ "name": name })).into_response()
        }
        SquadOutcome::Ok => Json(serde_json::json!({})).into_response(),
        SquadOutcome::Status(status) => Json(status).into_response(),
        SquadOutcome::Started {
            port,
            background,
            refreshed_key,
        } => Json(serde_json::json!({
            "port": port,
            "background": background,
            "refreshed_key": refreshed_key,
        }))
        .into_response(),
        SquadOutcome::Stopped { stopped_pid } => {
            Json(serde_json::json!({ "stopped_pid": stopped_pid })).into_response()
        }
        SquadOutcome::Logs { log_path } => {
            Json(serde_json::json!({ "log_path": log_path })).into_response()
        }
        // `squad env` is `api_allowed: false`, so the front door above never
        // admits it and this arm exists only to keep the match exhaustive.
        // Answering with the report anyway would quietly make the leaf
        // API-reachable, which is exactly what its catalogue flag refuses.
        SquadOutcome::Env(_) => (
            StatusCode::NOT_FOUND,
            error_json("squad env is not available over the API"),
        )
            .into_response(),
    }
}

async fn handle_status(State(state): State<Arc<SquadAppState>>) -> Response {
    match state.handles.status(state.bound_addr()).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "squad: failed to read daemon status");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read daemon status"),
            )
                .into_response()
        }
    }
}

/// `GET /v1/daemon/env` — the daemon's `required_env` with a salted digest per
/// name it holds. Names and digests only; no endpoint ever returns a value, so
/// a compromised bearer key cannot read secrets back out of the daemon.
async fn handle_env_coverage(State(state): State<Arc<SquadAppState>>) -> Response {
    match state.handles.gateway().env_coverage().await {
        Ok(coverage) => Json(coverage).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "squad: failed to read env coverage");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read daemon env coverage"),
            )
                .into_response()
        }
    }
}

/// `POST /v1/daemon/env` — a per-name merge, never a whole-map replace.
///
/// The extractor's rejection is caught rather than returned: axum's default
/// `JsonDataError` text quotes the offending input, which for this one route is
/// the payload. A fixed message is the only safe answer, and it is why the
/// handler takes a `Result` instead of a bare `Json<EnvPush>`.
async fn handle_env_push(
    State(state): State<Arc<SquadAppState>>,
    body: Result<Json<EnvPush>, JsonRejection>,
) -> Response {
    let Ok(Json(push)) = body else {
        // No body echo, no serde message, no value — not even in the log.
        tracing::warn!("squad: rejected a malformed daemon env push");
        return (
            StatusCode::BAD_REQUEST,
            error_json("malformed daemon env request"),
        )
            .into_response();
    };
    match state.handles.gateway().push_env(push).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "squad: failed to apply env push");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to apply daemon env push"),
            )
                .into_response()
        }
    }
}

/// `DELETE /v1/daemon/env` — remove the persisted keychain item (§5c).
///
/// The in-memory overlay is deliberately untouched: removing what is stored
/// must not disarm a daemon that is running fine.
async fn handle_env_clear(State(state): State<Arc<SquadAppState>>) -> Response {
    match state.handles.gateway().clear_env_store().await {
        Ok(cleared) => Json(serde_json::json!({ "cleared": cleared })).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "squad: failed to clear stored env");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to clear the stored daemon env"),
            )
                .into_response()
        }
    }
}

async fn handle_workflow(
    State(state): State<Arc<SquadAppState>>,
    Path(name): Path<String>,
) -> Response {
    match state.handles.workflow_state(&name).await {
        Ok(SquadWorkflowLookup::Found(workflow)) => Json(*workflow).into_response(),
        Ok(SquadWorkflowLookup::TaskNotFound) => {
            (StatusCode::NOT_FOUND, error_json("task not found")).into_response()
        }
        Ok(SquadWorkflowLookup::NoWorkflow) => (
            StatusCode::NOT_FOUND,
            error_json("no workflow for this task"),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "squad: failed to read workflow state");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read workflow state"),
            )
                .into_response()
        }
    }
}
