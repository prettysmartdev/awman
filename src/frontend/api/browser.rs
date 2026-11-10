//! Browser pairing HTTP adapter (WI 0159, Layer 3).
//!
//! `/v1/browser-links*`, `/v1/browsers*` and `/v1/browser-tickets`. Every
//! decision is `command::daemon_api::browser`; this file decodes bodies,
//! takes the origin/listener from the gate's [`BrowserRequest`] (never from
//! the body) and maps refusals onto the error table. Secrets in responses
//! are serialized once and never logged.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use axum::Json;

use super::error::ApiError;
use super::routes::BrowserRequest;
use crate::command::daemon_api::browser::{
    self as policy, BrowserContext, BrowserError, BrowserPrincipal,
};
use crate::command::daemon_api::{DaemonApi, Operation};
use crate::data::browser_auth::{
    BrowserLinkDecisionRequest, BrowserLinkPollRequest, BrowserTicketRequest,
    TICKET_SUBPROTOCOL_PREFIX,
};
use crate::data::daemon_state::Principal;
use crate::engine::auth::device_flow::retry_after_secs;

/// The router for the browser operations, or `None` for any other.
pub(super) fn route(op: Operation, filter: MethodFilter) -> Option<MethodRouter<Arc<DaemonApi>>> {
    Some(match op {
        Operation::BrowserLinksCreate => axum::routing::on(filter, handle_links_create),
        Operation::BrowserLinksPoll => axum::routing::on(filter, handle_links_poll),
        Operation::BrowserLinksLookup => axum::routing::on(filter, handle_links_lookup),
        Operation::BrowserLinksDecide => axum::routing::on(filter, handle_links_decide),
        Operation::BrowsersList => axum::routing::on(filter, handle_browsers_list),
        Operation::BrowsersDelete => axum::routing::on(filter, handle_browsers_delete),
        Operation::BrowserTicketsCreate => axum::routing::on(filter, handle_tickets_create),
        _ => return None,
    })
}

/// The gate's browser context as the policy layer sees it.
pub(super) fn context(request: &BrowserRequest) -> BrowserContext {
    BrowserContext {
        origin: request.origin.as_str().to_owned(),
        listener: request.listener.clone(),
    }
}

/// `BrowserError` → the API error table.
pub(super) fn error_response(error: BrowserError) -> Response {
    match error {
        BrowserError::NotFound => ApiError::not_found("resource not found").into_response(),
        BrowserError::LinkGone => ApiError::link_gone(error.to_string()).into_response(),
        BrowserError::LinkNotFound => ApiError::link_not_found(error.to_string()).into_response(),
        BrowserError::AlreadyDecided => ApiError::conflict(error.to_string()).into_response(),
        BrowserError::GrantNotFound => ApiError::grant_not_found(error.to_string()).into_response(),
        BrowserError::ResourceNotFound(what) => ApiError::not_found(what).into_response(),
        BrowserError::RateLimited(wait) => {
            let mut response = ApiError::rate_limited(error.to_string()).into_response();
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(retry_after_secs(wait)),
            );
            response
        }
        BrowserError::Internal(message) => {
            tracing::warn!(error = %message, "browser pairing operation failed");
            ApiError::internal("browser pairing failed").into_response()
        }
    }
}

/// `POST /v1/browser-links`: body absent or `{}`. Anything else is refused,
/// so a browser cannot ask for a scope, origin or listener.
async fn handle_links_create(
    State(api): State<Arc<DaemonApi>>,
    browser: Option<axum::Extension<BrowserRequest>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(axum::Extension(browser)) = browser else {
        return ApiError::not_found("resource not found").into_response();
    };
    let empty_object = body.iter().all(u8::is_ascii_whitespace)
        || serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body)
            .is_ok_and(|m| m.is_empty());
    if !empty_object {
        return ApiError::invalid_request("browser link requests take no fields").into_response();
    }
    match policy::links_create(&api, &context(&browser)) {
        Ok(created) => (StatusCode::CREATED, Json(created)).into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /v1/browser-links/{id}/poll` with `{"polling_secret": …}`.
async fn handle_links_poll(
    State(api): State<Arc<DaemonApi>>,
    browser: Option<axum::Extension<BrowserRequest>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let Some(axum::Extension(browser)) = browser else {
        return ApiError::not_found("resource not found").into_response();
    };
    let Ok(request) = serde_json::from_slice::<BrowserLinkPollRequest>(&body) else {
        return ApiError::invalid_request("expected {\"polling_secret\": …}").into_response();
    };
    match policy::links_poll(
        &api,
        &context(&browser),
        &id,
        request.polling_secret.expose(),
    ) {
        Ok(outcome) => Json(outcome).into_response(),
        Err(e) => error_response(e),
    }
}

#[derive(serde::Deserialize)]
struct LookupQuery {
    code: Option<String>,
}

/// `GET /v1/browser-links?code=web-…` (owner socket).
async fn handle_links_lookup(
    State(api): State<Arc<DaemonApi>>,
    Query(query): Query<LookupQuery>,
) -> Response {
    let Some(code) = query.code else {
        return ApiError::invalid_request("code= is required").into_response();
    };
    match policy::links_lookup(&api, &code) {
        Ok(view) => Json(view).into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /v1/browser-links/{id}/decision` with `{"approve": bool}` (owner socket).
async fn handle_links_decide(
    State(api): State<Arc<DaemonApi>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<BrowserLinkDecisionRequest>(&body) else {
        return ApiError::invalid_request("expected {\"approve\": true|false}").into_response();
    };
    match policy::links_decide(&api, &id, request.approve) {
        Ok(decided) => Json(decided).into_response(),
        Err(e) => error_response(e),
    }
}

/// `GET /v1/browsers` (owner socket).
async fn handle_browsers_list(State(api): State<Arc<DaemonApi>>) -> Response {
    Json(policy::browsers_list(&api)).into_response()
}

/// `DELETE /v1/browsers/{id}` (owner socket) → 204.
async fn handle_browsers_delete(
    State(api): State<Arc<DaemonApi>>,
    Path(id): Path<String>,
) -> Response {
    match policy::browsers_delete(&api, &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /v1/browser-tickets` (grant-authenticated).
async fn handle_tickets_create(
    State(api): State<Arc<DaemonApi>>,
    principal: Option<axum::Extension<Principal>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(browser) = principal.and_then(|p| BrowserPrincipal::from_principal(&p.0)) else {
        return ApiError::not_found("resource not found").into_response();
    };
    let Ok(request) = serde_json::from_slice::<BrowserTicketRequest>(&body) else {
        return ApiError::invalid_request(
            "expected {\"route\":\"frontend\",\"instance_id\":…} or {\"route\":\"terminal\",\"terminal_id\":…}",
        )
        .into_response();
    };
    match policy::tickets_create(&api, &browser, &request) {
        Ok(ticket) => (StatusCode::CREATED, Json(ticket)).into_response(),
        Err(e) => error_response(e),
    }
}

/// The ticket offered in `Sec-WebSocket-Protocol`.
///
/// `Ok(Some(ticket))` when exactly one `awman.ticket.<t>` entry and exactly
/// one other (real) protocol are offered; `Ok(None)` when no ticket entry is
/// offered; `Err(())` for any other shape (two tickets, no real protocol,
/// several real protocols).
pub(super) fn offered_ticket(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let offered: Vec<&str> = headers
        .get_all(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .map(|v| v.to_str().map_err(|_| ()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (tickets, real): (Vec<&str>, Vec<&str>) = offered
        .into_iter()
        .partition(|p| p.starts_with(TICKET_SUBPROTOCOL_PREFIX));
    match (tickets.as_slice(), real.len()) {
        ([], _) => Ok(None),
        ([ticket], 1) => {
            let ticket = &ticket[TICKET_SUBPROTOCOL_PREFIX.len()..];
            if ticket.is_empty() {
                Err(())
            } else {
                Ok(Some(ticket.to_owned()))
            }
        }
        _ => Err(()),
    }
}

/// Does the query string carry a credential-looking parameter? Browsers must
/// never put credentials in URLs; such requests are refused outright.
pub(super) fn query_carries_credentials(uri: &axum::http::Uri) -> bool {
    uri.query().is_some_and(|q| {
        let mut url = reqwest::Url::parse("http://localhost").expect("static URL");
        url.set_query(Some(q));
        url.query_pairs().any(|(key, _)| {
            key.eq_ignore_ascii_case("ticket")
                || key.eq_ignore_ascii_case("token")
                || key.eq_ignore_ascii_case("access_token")
        })
    })
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod tests;
