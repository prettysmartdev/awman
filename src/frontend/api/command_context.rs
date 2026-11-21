//! Read-only worker scratch routes. Filesystem policy lives below this adapter.
use super::error::ApiError;
use crate::command::{
    context::ContextApiError,
    daemon_api::{command_context, core, DaemonApi, Operation},
};
use crate::data::{ids::CommandId, wire::ListQuery};
use axum::{
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter},
    Json,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

static LISTINGS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub(super) fn route(op: Operation, filter: MethodFilter) -> Option<MethodRouter<Arc<DaemonApi>>> {
    match op {
        Operation::WorkerCommandsContext => Some(axum::routing::on(filter, list)),
        Operation::WorkerCommandsContextFile => Some(axum::routing::on(filter, get)),
        _ => None,
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn list(
    State(api): State<Arc<DaemonApi>>,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    let Some(worker) = api.worker.clone() else {
        return ApiError::feature_disabled("worker is disabled").into_response();
    };
    let Ok(permit) = LISTINGS.try_acquire() else {
        return ApiError::output_listing_unavailable("workflow context busy; retry later")
            .into_response();
    };
    let id = CommandId::parse(&id).expect("route validated command id");
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(cancelled.clone());
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        command_context::list(&worker, &id, &cancelled)
    })
    .await
    {
        Ok(Ok(items)) => match core::page(items, &query) {
            Ok(page) => Json(page).into_response(),
            Err(error) => ApiError::invalid_request(error.to_string()).into_response(),
        },
        Ok(Err(error)) => context_error(error),
        Err(error) => task_error(error),
    }
}

async fn get(
    State(api): State<Arc<DaemonApi>>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    let Some(worker) = api.worker.clone() else {
        return ApiError::feature_disabled("worker is disabled").into_response();
    };
    let id = CommandId::parse(&id).expect("route validated command id");
    match tokio::task::spawn_blocking(move || command_context::get(&worker, &id, &path)).await {
        Ok(Ok((bytes, meta))) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::ETAG, format!("\"{}\"", meta.sha256)),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                (header::CONTENT_DISPOSITION, "attachment".to_owned()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "sandbox; default-src 'none'".to_owned(),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) => context_error(error),
        Err(error) => task_error(error),
    }
}

fn context_error(error: ContextApiError) -> Response {
    // Data errors can contain host paths. Only fixed presentation text leaves
    // this boundary; the complete error is available in daemon diagnostics.
    tracing::debug!(%error, "Workflow context request failed");
    match error.code() {
        "invalid_request" => ApiError::invalid_request("invalid context path"),
        "not_found" => ApiError::request_not_found("workflow context file not found"),
        "command_not_found" => ApiError::command_not_found("command not found"),
        "context_too_large" => {
            ApiError::context_too_large("workflow context file exceeds the size limit")
        }
        "unavailable" => {
            ApiError::output_listing_unavailable("workflow context listing could not complete")
        }
        _ => ApiError::internal("failed to read workflow context"),
    }
    .into_response()
}

fn task_error(error: tokio::task::JoinError) -> Response {
    tracing::error!(%error, "Workflow context task failed");
    ApiError::internal("failed to read workflow context").into_response()
}
