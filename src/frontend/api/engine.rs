//! Engine HTTP translation. The Layer 2 host owns every lifecycle decision.
use super::error::ApiError;
use crate::command::{
    daemon_api::{engine::*, DaemonApi, Operation},
    instance::host::HostError,
};
use crate::data::{
    annotations::AnnotationFilter,
    ids::{DialogId, InstanceId, TabId},
    wire::ListQuery,
};
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter},
    Json,
};
use std::{collections::HashMap, sync::Arc};

pub(super) fn route(op: Operation, filter: MethodFilter) -> MethodRouter<Arc<DaemonApi>> {
    if op == Operation::EngineInstancesFrontend {
        return axum::routing::on(filter, super::frontend_socket::upgrade);
    }
    axum::routing::on(filter, handle)
}
fn error(e: HostError) -> Response {
    let status = match &e {
        HostError::NotFound | HostError::UnknownTab | HostError::UnknownDialog => {
            axum::http::StatusCode::NOT_FOUND
        }
        HostError::Draining => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        _ => axum::http::StatusCode::BAD_REQUEST,
    };
    ApiError {
        status,
        code: e.code(),
        message: e.to_string(),
    }
    .into_response()
}
async fn handle(
    State(api): State<Arc<DaemonApi>>,
    axum::Extension(op): axum::Extension<Operation>,
    params: Option<Path<HashMap<String, String>>>,
    Query(pairs): Query<Vec<(String, String)>>,
    body: axum::body::Bytes,
) -> Response {
    let host = api.runtime.engine_host();
    let params = params.map(|Path(p)| p).unwrap_or_default();
    let mut page = ListQuery::default();
    let mut filters = Vec::new();
    for (key, value) in pairs {
        match key.as_str() {
            "cursor" => page.cursor = Some(value),
            "limit" => match value.parse() {
                Ok(n) => page.limit = Some(n),
                Err(_) => {
                    return ApiError::invalid_request("limit must be an integer").into_response()
                }
            },
            "annotation" => filters.push(value),
            _ => {}
        }
    }
    let page = match page.page() {
        Ok(p) => p,
        Err(e) => return ApiError::invalid_request(e.to_string()).into_response(),
    };
    macro_rules! result {
        ($e:expr) => {
            match $e {
                Ok(v) => Json(v).into_response(),
                Err(e) => error(e),
            }
        };
    }
    macro_rules! decode {
        ($t:ty) => {
            match serde_json::from_slice::<$t>(&body) {
                Ok(v) => v,
                Err(_) => {
                    return ApiError::invalid_request("invalid engine request").into_response()
                }
            }
        };
    }
    if op == Operation::EngineInstancesList {
        let filter = match AnnotationFilter::parse(&filters) {
            Ok(f) => f,
            Err(e) => return ApiError::invalid_request(e.to_string()).into_response(),
        };
        return Json(host.list(&filter, &page)).into_response();
    }
    if op == Operation::EngineInstancesCreate {
        return match instances_create(&api, decode!(CreateInstanceRequest)) {
            Ok(v) => (axum::http::StatusCode::CREATED, Json(v)).into_response(),
            Err(e) => error(e),
        };
    }
    let Some(id) = params.get("id").and_then(|s| InstanceId::parse(s).ok()) else {
        return error(HostError::NotFound);
    };
    match op {
        Operation::EngineInstancesGet => result!(host.get(&id).ok_or(HostError::NotFound)),
        Operation::EngineInstancesDelete => {
            result!(host.delete(&id).map(|_| serde_json::json!({})))
        }
        Operation::EngineInstancesPatch => {
            let b = decode!(PatchAnnotations);
            result!(host
                .patch_annotations(&id, &b.annotations)
                .and_then(|_| host.get(&id).ok_or(HostError::NotFound)))
        }
        Operation::EngineTabsList => result!(host
            .tabs(&id)
            .ok_or(HostError::NotFound)
            .map(|items| serde_json::json!({"items": items, "next_cursor": null}))),
        Operation::EngineTabsGet | Operation::EngineTabsPatch => {
            let Some(tab) = params.get("tab").and_then(|s| TabId::parse(s).ok()) else {
                return error(HostError::UnknownTab);
            };
            if op == Operation::EngineTabsPatch {
                let b = decode!(PatchAnnotations);
                if let Err(e) = host.patch_tab_annotations(&id, &tab, &b.annotations) {
                    return error(e);
                }
            }
            result!(host.tab(&id, &tab).ok_or(HostError::UnknownTab))
        }
        Operation::EngineDialogsList => result!(host
            .dialogs(&id)
            .ok_or(HostError::NotFound)
            .map(|items| serde_json::json!({"items": items, "next_cursor": null}))),
        Operation::EngineDialogsAnswer => {
            let Some(dialog) = params
                .get("dialog_id")
                .and_then(|s| DialogId::parse(s).ok())
            else {
                return error(HostError::UnknownDialog);
            };
            result!(dialogs_answer(&api, &id, &dialog, decode!(AnswerRequest)))
        }
        Operation::EngineUiRequests => result!(
            ui_request(
                &api,
                &id,
                decode!(crate::data::frontend_protocol::UiRequest)
            )
            .await
        ),
        _ => unreachable!("engine resource operation"),
    }
}
