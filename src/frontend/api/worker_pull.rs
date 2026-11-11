//! HTTP bindings of the worker feature's pull routes (WI 0143). Each handler
//! maps one request to a typed Layer 2 operation in
//! `command::daemon_api::worker_pull`; the pull runtime lives there, not here.
use super::error::ApiError;
use crate::command::daemon_api::worker_pull::{self, ReleaseBody, WorkerError};
use crate::command::daemon_api::{DaemonApi, Operation};
use crate::data::ids::LeaseId;
use crate::data::wire::ListQuery;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter},
    Json,
};
use std::{collections::HashMap, sync::Arc};

pub(super) fn route(op: Operation, filter: MethodFilter) -> MethodRouter<Arc<DaemonApi>> {
    debug_assert!(op.pull(), "not a pull operation");
    axum::routing::on(filter, handle)
}

/// The one `{error, code}` mapping for pull failures.
pub fn error(e: WorkerError) -> Response {
    if let WorkerError::Store(inner) = &e {
        tracing::error!(error = %inner, "worker pull store failure");
        return ApiError::internal("failed to read worker leases").into_response();
    }
    let message = e.to_string();
    let api = match &e {
        WorkerError::PullDisabled(_) => ApiError::pull_disabled(message),
        WorkerError::UnknownServer(_) => ApiError::server_not_found(message),
        WorkerError::ServerNotRegistered(_) => ApiError::server_not_registered(message),
        WorkerError::ItemNotFound(_) => ApiError::item_not_found(message),
        WorkerError::LeaseNotFound => ApiError::lease_not_found(message),
        WorkerError::LeaseEnded(_) => ApiError::lease_ended(message),
        WorkerError::Taken => ApiError::taken(message),
        WorkerError::Ineligible(_) => ApiError::ineligible(message),
        WorkerError::Busy(_) => ApiError::worker_busy(message),
        WorkerError::LinkRevoked(_) => ApiError::link_revoked(message),
        WorkerError::Upstream(_) => ApiError::upstream_unavailable(message),
        WorkerError::Invalid(_) => ApiError::invalid_request(message),
        WorkerError::Store(_) => unreachable!("handled above"),
    };
    debug_assert_eq!(api.code, e.code());
    debug_assert_eq!(api.status.as_u16(), e.http_status());
    api.into_response()
}

#[derive(Default)]
struct PullQuery {
    page: ListQuery,
    server: Option<String>,
    state: Option<String>,
    host: Option<String>,
}

fn parse_query(pairs: Vec<(String, String)>, allowed: &[&str]) -> Result<PullQuery, ApiError> {
    let mut query = PullQuery::default();
    for (key, value) in pairs {
        if !allowed.contains(&key.as_str()) {
            return Err(ApiError::invalid_request(format!(
                "unknown query parameter {key:?}"
            )));
        }
        let slot = match key.as_str() {
            "limit" => {
                query.page.limit = Some(
                    value
                        .parse()
                        .map_err(|_| ApiError::invalid_request("limit must be an integer"))?,
                );
                continue;
            }
            "cursor" => &mut query.page.cursor,
            "server" => &mut query.server,
            "state" => &mut query.state,
            "host" => &mut query.host,
            _ => unreachable!("checked against allowed"),
        };
        if slot.replace(value).is_some() {
            return Err(ApiError::invalid_request(format!(
                "{key}= given more than once"
            )));
        }
    }
    Ok(query)
}

fn lease_id(params: &HashMap<String, String>) -> LeaseId {
    LeaseId::parse(&params["id"]).expect("route validated lease id")
}

async fn handle(
    State(api): State<Arc<DaemonApi>>,
    axum::Extension(op): axum::Extension<Operation>,
    params: Option<Path<HashMap<String, String>>>,
    Query(pairs): Query<Vec<(String, String)>>,
    body: axum::body::Bytes,
) -> Response {
    let params = params.map(|Path(p)| p).unwrap_or_default();
    let allowed: &[&str] = match op {
        Operation::WorkerServersList => &["limit", "cursor"],
        Operation::WorkerQueueList => &["server", "limit", "cursor"],
        Operation::WorkerQueueClaim => &["server", "host"],
        Operation::WorkerLeasesList => &["server", "state", "limit", "cursor"],
        _ => &[],
    };
    let query = match parse_query(pairs, allowed) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let result = match op {
        Operation::WorkerServersList => worker_pull::servers_list(&api)
            .and_then(|items| Ok(crate::command::daemon_api::core::page(items, &query.page)?))
            .map(|page| Json(page).into_response()),
        Operation::WorkerServersGet => {
            worker_pull::servers_get(&api, &params["name"]).map(|s| Json(s).into_response())
        }
        Operation::WorkerQueueList => {
            worker_pull::queue_list(&api, query.server.as_deref(), &query.page)
                .await
                .map(|page| Json(page).into_response())
        }
        Operation::WorkerQueueClaim => {
            if !body.is_empty() && serde_json::from_slice::<serde_json::Value>(&body).is_err() {
                return ApiError::invalid_request("claim body must be JSON").into_response();
            }
            if let Some(host) = query.host.as_deref() {
                if host != "foreground" {
                    return ApiError::invalid_request("host must be foreground").into_response();
                }
                return match worker_pull::queue_prepare_foreground(&api, &params["item_id"], query.server.as_deref()).await {
                    Ok(pick) => (StatusCode::CREATED, Json(serde_json::json!({
                        "lease": pick.lease, "command": pick.command, "args": pick.args,
                        "cwd": pick.cwd, "inline_source": pick.inline_source, "output_dir": pick.output_dir,
                        "repo_url": pick.repo_url, "branch": pick.branch, "read_only": pick.read_only,
                    }))).into_response(),
                    Err(e) => error(e),
                };
            }
            worker_pull::queue_claim(&api, &params["item_id"], query.server.as_deref())
                .await
                .map(|lease| (StatusCode::CREATED, Json(lease)).into_response())
        }
        Operation::WorkerLeasesList => worker_pull::leases_list(
            &api,
            query.server.as_deref(),
            query.state.as_deref(),
            &query.page,
        )
        .map(|page| Json(page).into_response()),
        Operation::WorkerLeasesGet => {
            worker_pull::leases_get(&api, &lease_id(&params)).map(|l| Json(l).into_response())
        }
        Operation::WorkerLeasesRelease => {
            let body: ReleaseBody = if body.is_empty() {
                ReleaseBody::default()
            } else {
                match serde_json::from_slice(&body) {
                    Ok(body) => body,
                    Err(e) => {
                        return ApiError::invalid_request(format!("invalid release body: {e}"))
                            .into_response()
                    }
                }
            };
            worker_pull::leases_release(&api, &lease_id(&params), body)
                .await
                .map(|l| Json(l).into_response())
        }
        Operation::WorkerLeasesTerminal => {
            let frames =
                match serde_json::from_slice::<Vec<awman_wsp::terminal::LeaseTerminalFrame>>(&body)
                {
                    Ok(frames) => frames,
                    Err(error) => {
                        return ApiError::invalid_request(format!(
                            "invalid terminal frames: {error}"
                        ))
                        .into_response()
                    }
                };
            worker_pull::foreground_terminal(&api, &lease_id(&params), frames)
                .await
                .map(|items| {
                    Json(serde_json::json!({"items":items,"next_cursor":null})).into_response()
                })
        }
        Operation::WorkerLeasesEvents => {
            let events = match serde_json::from_slice::<
                Vec<crate::data::execution_event::ExecutionEvent>,
            >(&body)
            {
                Ok(events) => events,
                Err(e) => {
                    return ApiError::invalid_request(format!("invalid events: {e}"))
                        .into_response()
                }
            };
            worker_pull::foreground_events(&api, &lease_id(&params), events)
                .await
                .map(|()| StatusCode::NO_CONTENT.into_response())
        }
        Operation::WorkerLeasesCheckout => {
            let workdir = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|body| {
                    body.get("workdir")
                        .and_then(|p| p.as_str())
                        .map(std::path::PathBuf::from)
                });
            let Some(workdir) = workdir else {
                return ApiError::invalid_request("checkout requires workdir").into_response();
            };
            worker_pull::foreground_checkout(&api, &lease_id(&params), workdir)
                .await
                .map(|pick| Json(serde_json::json!({"cwd": pick.cwd})).into_response())
        }
        Operation::WorkerLeasesResult => {
            let result = match serde_json::from_slice::<crate::data::worker::wire::ForegroundResult>(
                &body,
            ) {
                Ok(result) => result,
                Err(e) => {
                    return ApiError::invalid_request(format!("invalid result: {e}"))
                        .into_response()
                }
            };
            worker_pull::foreground_result(&api, &lease_id(&params), result)
                .await
                .map(|result| Json(result).into_response())
        }
        _ => unreachable!("non-pull operation"),
    };
    result.unwrap_or_else(error)
}
