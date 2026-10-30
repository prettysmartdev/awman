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
    unauthorized => ("unauthorized", UNAUTHORIZED),
    forbidden_workdir => (failure_codes::FORBIDDEN_WORKDIR, FORBIDDEN),
    invalid_request => (failure_codes::INVALID_REQUEST, BAD_REQUEST),
    request_forbidden => ("forbidden", FORBIDDEN),
    request_not_found => ("not_found", NOT_FOUND),
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
}

impl ApiError {
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
        match error {
            CommandError::SessionWorkdirNotAllowed { .. } => {
                Self::forbidden_workdir(error.to_string())
            }
            CommandError::WorktreeExists { .. } => Self::worktree_exists(error.to_string()),
            CommandError::MergeConflict { .. } => Self::merge_conflict(error.to_string()),
            CommandError::MergeTargetDirty { .. } => Self::merge_target_dirty(error.to_string()),
            CommandError::WorkflowPrerequisitesUnresolved { .. } => {
                Self::workflow_prerequisites_unresolved(error.to_string())
            }
            CommandError::Engine(crate::engine::error::EngineError::UnknownAgent { .. }) => {
                Self::unknown_agent(error.to_string())
            }
            CommandError::Engine(crate::engine::error::EngineError::AgentDefinitionUntrusted {
                ..
            }) => Self::agent_untrusted(error.to_string()),
            _ => match error.http_status() {
                403 => Self::request_forbidden(error.to_string()),
                404 => Self::request_not_found(error.to_string()),
                500 => Self::internal(error.to_string()),
                400 => Self::command_validation(error),
                status => Self {
                    status: StatusCode::from_u16(status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    code: error.code(),
                    message: error.to_string(),
                },
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn details_preserve_existing_context_fields() {
        let response = ApiError::session_not_ready("session setup failed").with_details(
            serde_json::json!({"setup_status": "failed", "setup_error": {"stage": "clone", "message": "failed"}, "hint": "poll"}),
        );
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "session setup failed");
        assert_eq!(body["code"], "session_not_ready");
        assert_eq!(body["setup_error"]["stage"], "clone");
        assert_eq!(body["hint"], "poll");
    }

    #[tokio::test]
    async fn framework_rejections_keep_status_message_and_headers() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::NOT_FOUND,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            let response = (status, [("allow", "GET")], "original rejection").into_response();
            let response = normalize_response(response).await;
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["allow"], "GET");
            assert!(response.headers().get("content-length").is_none());
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                serde_json::json!({"error": "original rejection", "code": match status.as_u16() {
                    404 => "not_found", 405 => "method_not_allowed", 413 => "request_too_large",
                    415 => "unsupported_media_type", 422 => "unprocessable_request", _ => "invalid_request"
                }})
            );
        }
        let response =
            normalize_response(ApiError::unknown_flag("unknown flag").into_response()).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["code"],
            "unknown_flag"
        );
        let response = normalize_response("success".into_response()).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"success");
    }

    #[tokio::test]
    async fn json_with_charset_is_preserved_and_unavailable_has_retry_header() {
        let mut response = ApiError::unknown_flag("original").into_response();
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
        );
        let response = normalize_response(response).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["code"], "unknown_flag");
        assert_eq!(json["error"], "original");
        let response = ApiError::output_listing_unavailable("busy").into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
    }

    #[test]
    fn shared_conditions_reuse_async_failure_codes() {
        let error = CommandError::WorktreeExists {
            path: "/worktree".into(),
            branch: "branch".into(),
        };
        let api = ApiError::command_admission(&error);
        assert_eq!(api.code, error.code());
        assert_eq!(api.status.as_u16(), error.http_status());
        assert_eq!(api.message, error.to_string());
        let error = CommandError::UnknownFlag {
            command: vec!["exec".into()],
            flag: "--typo".into(),
        };
        assert_eq!(ApiError::command_validation(&error).code, "unknown_flag");
        assert_eq!(ApiError::command_admission(&error).code, error.code());
        let error = CommandError::UnknownCommand {
            path: vec!["typo".into()],
            suggestions: vec![],
        };
        assert_eq!(
            ApiError::command_validation(&error).code,
            "unknown_subcommand"
        );
        assert_eq!(ApiError::command_admission(&error).code, error.code());
        for error in [
            CommandError::SessionWorkdirNotAllowed {
                requested: "/work".into(),
                rule: crate::command::session_create::WorkdirRefusal::NotListed,
                allowed: vec![],
                roots: vec![],
            },
            CommandError::WorkflowFileNotFound {
                path: "/workflow".into(),
            },
            CommandError::SessionRepoUrlEmpty,
            CommandError::Other("failure".into()),
        ] {
            let api = ApiError::command_admission(&error);
            assert_eq!(api.code, error.code());
            assert_eq!(api.status.as_u16(), error.http_status());
            assert_eq!(api.message, error.to_string());
        }
    }
}
