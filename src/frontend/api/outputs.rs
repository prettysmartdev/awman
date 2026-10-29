//! `GET /v1/commands/{id}/outputs` and `GET /v1/commands/{id}/outputs/{path}`
//! (WI 0137 §3): list and serve the files an agent left in the command's
//! `/awman/output` directory.
//!
//! - the directory is `ApiPaths::command_output_dir(session, command)`;
//! - listing: `{"files": [{path, size, sha256, modified_at}], "total":
//!   {files, bytes}}`, regular files only, sorted relative paths with `/`
//!   separators; `total` matches `CommandResult::outputs`;
//! - fetch: `Content-Type` guessed from the extension and
//!   `application/octet-stream` otherwise; a file over
//!   `MAX_SERVED_OUTPUT_BYTES` is listed but returns 413;
//! - both routes work while the command is running; a missing directory is
//!   an empty listing, not an error.
//!
//! The agent controls the directory, so path validation and the symlink-safe
//! open live in `crate::data::fs::command_outputs` and every request goes
//! through them: a bad path is 400, and a symlink, device, FIFO or missing
//! file is 404 (`aspec/architecture/security.md`).

use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::data::fs::command_outputs::{
    content_type_for, list_output_dir, open_output_file, validate_output_path,
    MAX_SERVED_OUTPUT_BYTES,
};
use crate::frontend::api::routes::AppState;
use crate::frontend::api::serve::error_json;

/// `GET /v1/commands/{id}/outputs`.
pub async fn handle_list_outputs(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
) -> Response {
    // Bound concurrent CPU/I/O work and cancel it when the request is dropped.
    static LISTINGS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let Ok(permit) = LISTINGS.try_acquire() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            error_json("output listing busy; retry later"),
        )
            .into_response();
    };
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let _cancel = CancelOnDrop(Arc::clone(&cancelled));
    let state = Arc::clone(&state);
    let id = command_id.clone();
    let listed = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let command = lookup_command(&state, &id)?;
        let dir = state
            .paths
            .command_output_dir(&command.session_id, &command.id);
        Some(list_output_dir(&dir, &cancelled))
    })
    .await;
    match listed {
        Ok(Some(Ok(listing))) => Json(listing).into_response(),
        Ok(Some(Err(_))) => (
            StatusCode::SERVICE_UNAVAILABLE,
            error_json("output listing could not complete; retry later"),
        )
            .into_response(),
        Ok(None) => command_not_found(&command_id),
        Err(error) => internal_error(error),
    }
}

/// What a fetch found, decided off the async runtime.
enum Fetched {
    UnknownCommand,
    BadPath(String),
    Missing,
    TooLarge(u64),
    File(Vec<u8>),
}

/// `GET /v1/commands/{id}/outputs/{*path}`.
pub async fn handle_get_output(
    State(state): State<Arc<AppState>>,
    AxumPath((command_id, path)): AxumPath<(String, String)>,
) -> Response {
    // Reject a hostile path before looking anything up.
    if let Err(error) = validate_output_path(&path) {
        return (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response();
    }
    let state = Arc::clone(&state);
    let id = command_id.clone();
    let requested = path.clone();
    let fetched = tokio::task::spawn_blocking(move || {
        let Some(command) = lookup_command(&state, &id) else {
            return Fetched::UnknownCommand;
        };
        let dir = state
            .paths
            .command_output_dir(&command.session_id, &command.id);
        let mut opened = match open_output_file(&dir, &requested) {
            Ok(Some(opened)) => opened,
            Ok(None) => return Fetched::Missing,
            Err(error) => return Fetched::BadPath(error.to_string()),
        };
        if opened.size > MAX_SERVED_OUTPUT_BYTES {
            return Fetched::TooLarge(opened.size);
        }
        // Read through the descriptor that was opened safely, bounded by the
        // cap even if the agent grows the file meanwhile.
        use std::io::Read;
        let mut bytes = Vec::with_capacity(opened.size as usize);
        match (&mut opened.file)
            .take(MAX_SERVED_OUTPUT_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            Ok(_) if bytes.len() as u64 > MAX_SERVED_OUTPUT_BYTES => {
                Fetched::TooLarge(bytes.len() as u64)
            }
            Ok(_) => Fetched::File(bytes),
            Err(error) => {
                tracing::warn!(%error, command_id = %id, "Failed to read command output");
                Fetched::Missing
            }
        }
    })
    .await;
    match fetched {
        Ok(Fetched::UnknownCommand) => command_not_found(&command_id),
        Ok(Fetched::BadPath(message)) => {
            (StatusCode::BAD_REQUEST, error_json(message)).into_response()
        }
        Ok(Fetched::Missing) => (
            StatusCode::NOT_FOUND,
            error_json(format!(
                "Output '{path}' not found for command '{command_id}'"
            )),
        )
            .into_response(),
        Ok(Fetched::TooLarge(size)) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            error_json(format!(
                "Output '{path}' is {size} bytes; the largest output served is \
                 {MAX_SERVED_OUTPUT_BYTES} bytes"
            )),
        )
            .into_response(),
        Ok(Fetched::File(bytes)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type_for(&path)),
                // The content is agent-written: never let a browser sniff it
                // into something more active than its declared type.
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_DISPOSITION, "attachment"),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "sandbox; default-src 'none'",
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => internal_error(error),
    }
}

fn lookup_command(
    state: &AppState,
    command_id: &str,
) -> Option<crate::data::fs::api_db::CommandRecord> {
    match state.store.get_command(command_id) {
        Ok(found) => found,
        Err(error) => {
            tracing::error!(%error, command_id, "Failed to look up command for outputs");
            None
        }
    }
}

fn command_not_found(command_id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        error_json(format!("Command '{command_id}' not found")),
    )
        .into_response()
}

fn internal_error(error: tokio::task::JoinError) -> Response {
    tracing::error!(%error, "Command outputs task failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        error_json("failed to read command outputs"),
    )
        .into_response()
}
