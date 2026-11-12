//! Stable HTTP error codes and the shared, additive JSON error envelope.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::command::error::{failure_codes, CommandError};

/// An HTTP failure. Messages are presentation text; clients should branch on `code`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

macro_rules! constructors {
    ($($name:ident => ($code:expr, $status:ident)),+ $(,)?) => {
        impl ApiError {
            /// Complete code/status mapping, including compatibility statuses.
            /// A code can have several statuses where existing routes require it.
            pub const ALL: &'static [(&'static str, StatusCode)] = &[
                $(($code, StatusCode::$status)),+
            ];

            $(pub fn $name(message: impl Into<String>) -> Self {
                Self { status: StatusCode::$status, code: $code, message: message.into() }
            })+
        }

        #[cfg(test)]
        mod constructor_tests {
            use super::*;

            #[tokio::test]
            async fn every_constructor_preserves_message_status_and_code() {
                $(let error = ApiError::$name("unchanged message\n\"quoted\"");
                assert_eq!(error.status, StatusCode::$status);
                assert_eq!(error.code, $code);
                let response = error.into_response();
                assert_eq!(response.status(), StatusCode::$status);
                assert_eq!(response.headers()["content-type"], "application/json");
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    serde_json::json!({"error": "unchanged message\n\"quoted\"", "code": $code}));)+
            }
        }
    };
}

constructors! {
    host_mismatch => ("host_mismatch", MISDIRECTED_REQUEST),
    origin_not_allowed => ("origin_not_allowed", FORBIDDEN),
    browser_grant_required => ("browser_grant_required", UNAUTHORIZED),
    browser_grant_invalid => ("browser_grant_invalid", UNAUTHORIZED),
    // WI 0159 browser pairing.
    browser_ticket_invalid => ("browser_ticket_invalid", UNAUTHORIZED),
    link_gone => ("link_gone", GONE),
    link_not_found => ("link_not_found", NOT_FOUND),
    rate_limited => ("rate_limited", TOO_MANY_REQUESTS),
    // WI 0160 browser reauthorization.
    credential_exists => ("credential_exists", CONFLICT),
    daemon_restarted => ("daemon_restarted", CONFLICT),
    listener_changed => ("listener_changed", CONFLICT),
    reauth_failed => ("reauth_failed", UNAUTHORIZED),
    authorization_expired => ("authorization_expired", FORBIDDEN),
    authorization_revoked => ("authorization_revoked", FORBIDDEN),
    unauthorized => ("unauthorized", UNAUTHORIZED),
    forbidden_workdir => (failure_codes::FORBIDDEN_WORKDIR, FORBIDDEN),
    invalid_request => (failure_codes::INVALID_REQUEST, BAD_REQUEST),
    request_forbidden => ("forbidden", FORBIDDEN),
    request_not_found => ("not_found", NOT_FOUND),
    feature_disabled => ("feature_disabled", SERVICE_UNAVAILABLE),
    feature_degraded => ("feature_degraded", SERVICE_UNAVAILABLE),
    ephemeral_not_allowed => ("ephemeral_not_allowed", FORBIDDEN),
    config_invalid => ("config_invalid", UNPROCESSABLE_ENTITY),
    task_not_found => ("task_not_found", NOT_FOUND),
    run_not_found => ("run_not_found", NOT_FOUND),
    grant_not_found => ("grant_not_found", NOT_FOUND),
    task_exists => ("task_exists", CONFLICT),
    conflict => ("conflict", CONFLICT),
    prerequisite_widening_refused => ("prerequisite_widening_refused", FORBIDDEN),
    method_not_allowed => ("method_not_allowed", METHOD_NOT_ALLOWED),
    output_not_found => ("output_not_found", NOT_FOUND),
    request_too_large => ("request_too_large", PAYLOAD_TOO_LARGE),
    output_too_large => ("output_too_large", PAYLOAD_TOO_LARGE),
    unsupported_media_type => ("unsupported_media_type", UNSUPPORTED_MEDIA_TYPE),
    unprocessable_request => ("unprocessable_request", UNPROCESSABLE_ENTITY),
    unknown_subcommand => (failure_codes::UNKNOWN_SUBCOMMAND, BAD_REQUEST),
    unknown_flag => (failure_codes::UNKNOWN_FLAG, BAD_REQUEST),
    session_not_found => ("session_not_found", NOT_FOUND),
    session_closing => ("session_closing", CONFLICT),
    session_not_ready => ("session_not_ready", CONFLICT),
    command_not_found => ("command_not_found", NOT_FOUND),
    no_workflow => ("no_workflow", NOT_FOUND),
    workflow_not_started => ("workflow_not_started", NOT_FOUND),
    workflow_never_started => ("workflow_never_started", NOT_FOUND),
    idempotency_conflict => ("idempotency_conflict", UNPROCESSABLE_ENTITY),
    idempotency_in_flight => ("idempotency_in_flight", CONFLICT),
    internal => (crate::data::fs::api_db::failure_codes::INTERNAL, INTERNAL_SERVER_ERROR),
    output_listing_unavailable => ("unavailable", SERVICE_UNAVAILABLE),
    worktree_exists => (failure_codes::WORKTREE_EXISTS, CONFLICT),
    merge_conflict => (failure_codes::MERGE_CONFLICT, CONFLICT),
    merge_target_dirty => (failure_codes::MERGE_TARGET_DIRTY, CONFLICT),
    workflow_prerequisites_unresolved => (failure_codes::WORKFLOW_PREREQUISITES_UNRESOLVED, CONFLICT),
    unknown_agent => (failure_codes::UNKNOWN_AGENT, NOT_FOUND),
    agent_untrusted => (failure_codes::AGENT_UNTRUSTED, FORBIDDEN),
    // WI 0141: a command asked for an interaction mode its feature's
    // `allowOverride: false` forbids.
    interaction_override_refused => (crate::command::interaction::INTERACTION_OVERRIDE_REFUSED, BAD_REQUEST),
    // WI 0141 `/v1/terminals`.
    terminal_not_found => ("terminal_not_found", NOT_FOUND),
    terminal_gone => ("terminal_gone", GONE),
    control_held => ("control_held", CONFLICT),
    input_not_allowed => ("input_not_allowed", FORBIDDEN),
    input_backpressure => ("input_backpressure", TOO_MANY_REQUESTS),
    // WI 0143 pull routes (`/v1/worker/{servers,queue,leases}`).
    pull_disabled => ("pull_disabled", SERVICE_UNAVAILABLE),
    server_not_found => ("server_not_found", NOT_FOUND),
    server_not_registered => ("server_not_registered", SERVICE_UNAVAILABLE),
    item_not_found => ("item_not_found", NOT_FOUND),
    lease_not_found => ("lease_not_found", NOT_FOUND),
    lease_ended => ("lease_ended", CONFLICT),
    taken => ("taken", CONFLICT),
    ineligible => ("ineligible", CONFLICT),
    worker_busy => ("worker_busy", CONFLICT),
    link_revoked => ("link_revoked", CONFLICT),
    upstream_unavailable => ("upstream_unavailable", BAD_GATEWAY),
}

impl ApiError {
    /// Generic resource or route not found response.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::request_not_found(message)
    }

    /// Central mapping from command failure codes to HTTP status and response
    /// code. All API handlers should use this instead of maintaining mappings.
    pub fn from_command_error(error: &CommandError) -> Self {
        let code = error.code();
        let status = Self::ALL
            .iter()
            .find_map(|(registered, status)| (*registered == code).then_some(*status))
            .unwrap_or_else(|| {
                StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_REQUEST)
            });
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }

    /// Catalogue parsing always rejects at admission with HTTP 400.
    pub fn command_validation(error: &CommandError) -> Self {
        match error {
            CommandError::UnknownCommand { .. } => Self::unknown_subcommand(error.to_string()),
            CommandError::UnknownFlag { .. } => Self::unknown_flag(error.to_string()),
            _ => Self::invalid_request(error.to_string()),
        }
    }

    /// Session validation historically treats all other failures as bad input.
    pub fn session_creation(error: &CommandError) -> Self {
        use crate::engine::error::EngineError;
        match error {
            CommandError::SessionWorkdirNotAllowed { .. } => {
                Self::forbidden_workdir(error.to_string())
            }
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. }) => {
                Self::agent_untrusted(error.to_string())
            }
            CommandError::Engine(EngineError::UnknownAgent { .. }) => {
                Self::unknown_agent(error.to_string())
            }
            _ => Self::invalid_request(error.to_string()),
        }
    }

    /// Preserve command admission's status and reuse asynchronous failure codes
    /// for typed conditions shared with `result.failure` (WI 0137).
    pub fn command_admission(error: &CommandError) -> Self {
        Self::from_command_error(error)
    }

    /// Retain existing route-specific fields alongside the shared envelope.
    pub fn with_details(self, details: serde_json::Value) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
            code: &'static str,
            #[serde(flatten)]
            details: serde_json::Value,
        }
        let unavailable = self.status == StatusCode::SERVICE_UNAVAILABLE;
        let mut response = (
            self.status,
            Json(Body {
                error: self.message,
                code: self.code,
                details,
            }),
        )
            .into_response();
        if unavailable {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.with_details(serde_json::json!({}))
    }
}

/// Axum's extractor and router rejections otherwise bypass the handlers and
/// return plain text. Keep their status, message, and transport headers while
/// giving them the same envelope as errors returned by application code.
pub(crate) async fn normalize_rejections(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    normalize_response(next.run(request).await).await
}

async fn normalize_response(response: Response) -> Response {
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    if response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return ApiError::internal("Failed to read error response").into_response(),
    };
    let message = if bytes.is_empty() {
        parts
            .status
            .canonical_reason()
            .unwrap_or("Request failed")
            .to_string()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let error = match parts.status {
        StatusCode::NOT_FOUND => ApiError::request_not_found(message),
        StatusCode::METHOD_NOT_ALLOWED => ApiError::method_not_allowed(message),
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::request_too_large(message),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::unsupported_media_type(message),
        StatusCode::UNPROCESSABLE_ENTITY => ApiError::unprocessable_request(message),
        _ if parts.status.is_server_error() => ApiError::internal(message),
        _ => ApiError::invalid_request(message),
    };
    let normalized = error.into_response();
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, normalized.into_body())
}
