//! Typed engine resource operations. Admission and arbitration belong to the host.
use super::DaemonApi;
use crate::command::instance::host::HostError;
use crate::data::{
    annotations::Annotations,
    dialog::DialogResponse,
    frontend_protocol::{AnsweredBy, UiOutcome, UiRequest},
    ids::{DialogId, InstanceId},
};
use serde::{Deserialize, Serialize};
#[derive(Default, Deserialize)]
pub struct CreateInstanceRequest {
    #[serde(default)]
    pub annotations: Annotations,
}
#[derive(Serialize)]
pub struct CreateInstanceResponse {
    pub id: InstanceId,
}
#[derive(Deserialize)]
pub struct PatchAnnotations {
    pub annotations: Annotations,
}
#[derive(Deserialize)]
pub struct AnswerRequest {
    pub response: DialogResponse,
}
pub fn instances_create(
    api: &DaemonApi,
    body: CreateInstanceRequest,
) -> Result<CreateInstanceResponse, HostError> {
    Ok(CreateInstanceResponse {
        id: api
            .runtime
            .engine_host()
            .create_instance(body.annotations)?,
    })
}
pub fn dialogs_answer(
    api: &DaemonApi,
    id: &InstanceId,
    dialog: &DialogId,
    body: AnswerRequest,
) -> Result<crate::data::instance_snapshot::DialogAnswerOutcome, HostError> {
    api.runtime
        .engine_host()
        .answer(id, dialog, body.response, AnsweredBy::Api)
}
pub async fn ui_request(
    api: &DaemonApi,
    id: &InstanceId,
    request: UiRequest,
) -> Result<UiOutcome, HostError> {
    api.runtime
        .engine_host()
        .ui_request(id, None, request)
        .await
}
