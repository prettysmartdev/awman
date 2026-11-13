//! Keyed durable repo context and a push command's context changes (Layer 3).
//!
//! - `GET /v1/contexts/{key}` — `{items:[{path,size_bytes,sha256,modified_at}],
//!   next_cursor}`; a key with no directory yet is an empty list;
//! - `GET /v1/contexts/{key}/files/{*path}` — the bytes, `ETag: "<sha256>"`;
//! - `PUT …/files/{*path}` — `201` created / `200` replaced, the file's
//!   metadata and `ETag`; `If-Match: <sha256>` → `412 precondition_failed` on
//!   mismatch; the body is bounded while it is read (`413 context_too_large`);
//! - `DELETE …/files/{*path}` — `204`;
//! - `GET /v1/worker/commands/{id}/context_changes` — `{items:[{path,change,
//!   size_bytes?,sha256?}], next_cursor}`.
//!
//! Handlers only extract, call one `daemon_api::contexts` function off the
//! async runtime and map the typed result. Key and path validation, caps,
//! `If-Match` and locking are Layer 2's. No response carries a host path:
//! storage errors answer with a fixed message.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use axum::Json;
use http_body_util::BodyExt;

use super::error::ApiError;
use crate::command::context::ContextApiError;
use crate::command::daemon_api::{contexts, DaemonApi, Operation};
use crate::data::context::{ContextFileMeta, ContextLimits, WriteOutcome};
use crate::data::ids::CommandId;
use crate::data::wire::ListQuery;

pub(super) fn route(op: Operation, filter: MethodFilter) -> Option<MethodRouter<Arc<DaemonApi>>> {
    match op {
        Operation::CoreContextsList => Some(axum::routing::on(filter, handle_list)),
        Operation::CoreContextFilesGet => Some(axum::routing::on(filter, handle_get)),
        Operation::CoreContextFilesPut => Some(axum::routing::on(filter, handle_put)),
        Operation::CoreContextFilesDelete => Some(axum::routing::on(filter, handle_delete)),
        Operation::WorkerCommandsContextChanges => Some(axum::routing::on(filter, handle_changes)),
        _ => None,
    }
}

/// `ContextApiError` → the API error table (`ContextApiError::code`).
fn error_response(error: ContextApiError) -> Response {
    let message = error.to_string();
    match error.code() {
        "invalid_request" => ApiError::invalid_request(message),
        "not_found" => ApiError::request_not_found(message),
        "command_not_found" => ApiError::command_not_found(message),
        "precondition_failed" => ApiError::precondition_failed(message),
        "context_too_large" => ApiError::context_too_large(message),
        "conflict" => ApiError::conflict(message),
        "unavailable" => ApiError::output_listing_unavailable(message),
        // `Data` may name host paths: log it, answer with a fixed message.
        _ => {
            tracing::error!(error = %message, "Context request failed");
            ApiError::internal("context storage failed")
        }
    }
    .into_response()
}

fn join_failed(error: tokio::task::JoinError) -> Response {
    tracing::error!(%error, "Context task failed");
    ApiError::internal("context request failed").into_response()
}

/// Sets the flag when the request future is dropped, so a walk stops early.
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn etag(meta: &ContextFileMeta) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{}\"", meta.sha256))
        .unwrap_or_else(|_| HeaderValue::from_static("\"\""))
}

fn if_match(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match headers.get(header::IF_MATCH) {
        None => Ok(None),
        Some(value) => value.to_str().map(|v| Some(v.to_owned())).map_err(|_| {
            ApiError::invalid_request("If-Match must be a strong ETag (the file's sha256)")
        }),
    }
}

fn page<T: serde::Serialize>(items: Vec<T>, query: &ListQuery) -> Response {
    match crate::command::daemon_api::core::page(items, query) {
        Ok(page) => Json(page).into_response(),
        Err(e) => ApiError::invalid_request(e.to_string()).into_response(),
    }
}

/// `GET /v1/contexts/{key}`.
async fn handle_list(
    State(api): State<Arc<DaemonApi>>,
    AxumPath(key): AxumPath<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&cancelled));
    let listed = tokio::task::spawn_blocking(move || contexts::list(&api, &key, &cancelled)).await;
    match listed {
        Ok(Ok(files)) => page(files, &query),
        Ok(Err(error)) => error_response(error),
        Err(error) => join_failed(error),
    }
}

/// `GET /v1/contexts/{key}/files/{*path}`.
async fn handle_get(
    State(api): State<Arc<DaemonApi>>,
    AxumPath((key, path)): AxumPath<(String, String)>,
) -> Response {
    let content_type = crate::data::fs::command_outputs::content_type_for(&path);
    let fetched = tokio::task::spawn_blocking(move || contexts::get(&api, &key, &path)).await;
    match fetched {
        Ok(Ok((bytes, meta))) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
                (header::ETAG, etag(&meta)),
                // Agent-written content: never let a browser sniff it into
                // something more active than its declared type.
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
                (
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment"),
                ),
                (
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static("sandbox; default-src 'none'"),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => join_failed(error),
    }
}

/// `PUT /v1/contexts/{key}/files/{*path}`.
async fn handle_put(
    State(api): State<Arc<DaemonApi>>,
    AxumPath((key, path)): AxumPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let if_match = match if_match(&headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    // Bound the body while it is read; one byte over the cap is enough to refuse.
    let limit = ContextLimits::REPO.max_file_bytes;
    let bytes = match http_body_util::Limited::new(body, limit as usize + 1)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.is::<http_body_util::LengthLimitError>() => {
            return ApiError::context_too_large(format!(
                "context file exceeds the size limit of {limit} bytes"
            ))
            .into_response()
        }
        Err(_) => {
            return ApiError::invalid_request("request body could not be read").into_response()
        }
    };
    if bytes.len() as u64 > limit {
        return ApiError::context_too_large(format!(
            "context file exceeds the size limit of {limit} bytes"
        ))
        .into_response();
    }
    let written = tokio::task::spawn_blocking(move || {
        contexts::put(&api, &key, &path, &bytes, if_match.as_deref())
    })
    .await;
    match written {
        Ok(Ok((meta, outcome))) => {
            let status = match outcome {
                WriteOutcome::Created => StatusCode::CREATED,
                WriteOutcome::Replaced => StatusCode::OK,
            };
            let tag = etag(&meta);
            (status, [(header::ETAG, tag)], Json(meta)).into_response()
        }
        Ok(Err(error)) => error_response(error),
        Err(error) => join_failed(error),
    }
}

/// `DELETE /v1/contexts/{key}/files/{*path}`.
async fn handle_delete(
    State(api): State<Arc<DaemonApi>>,
    AxumPath((key, path)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let if_match = match if_match(&headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let deleted = tokio::task::spawn_blocking(move || {
        contexts::delete(&api, &key, &path, if_match.as_deref())
    })
    .await;
    match deleted {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => join_failed(error),
    }
}

/// `GET /v1/worker/commands/{id}/context_changes`.
async fn handle_changes(
    State(api): State<Arc<DaemonApi>>,
    AxumPath(command_id): AxumPath<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    let Some(worker) = api.worker.clone() else {
        return ApiError::feature_disabled("worker feature is disabled").into_response();
    };
    let Ok(id) = CommandId::parse(&command_id) else {
        return ApiError::command_not_found(format!("Command '{command_id}' not found"))
            .into_response();
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&cancelled));
    let listed =
        tokio::task::spawn_blocking(move || contexts::command_changes(&worker, &id, &cancelled))
            .await;
    match listed {
        Ok(Ok(changes)) => page(changes, &query),
        Ok(Err(ContextApiError::CommandNotFound(_))) => {
            ApiError::command_not_found(format!("Command '{command_id}' not found")).into_response()
        }
        Ok(Err(error)) => error_response(error),
        Err(error) => join_failed(error),
    }
}
