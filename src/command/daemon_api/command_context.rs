//! Owned by workflow-scratch-push: typed worker scratch entry points.
use crate::{
    command::{
        context::{workflow_api::WorkflowContextService, ContextApiError},
        daemon_api::worker::WorkerApi,
    },
    data::{context::ContextFileMeta, fs::context_dirs::ContextDirResolver, ids::CommandId},
};
use std::sync::atomic::AtomicBool;
fn service(api: &WorkerApi) -> Result<WorkflowContextService, ContextApiError> {
    let config = api.live_config.read().expect("worker config lock");
    let config = config
        .as_ref()
        .ok_or_else(|| ContextApiError::Unavailable("worker configuration unavailable".into()))?;
    Ok(WorkflowContextService::new(
        api.paths.clone(),
        ContextDirResolver::from_env(config.env())?,
    ))
}
pub fn list(
    api: &WorkerApi,
    id: &CommandId,
    cancelled: &AtomicBool,
) -> Result<Vec<ContextFileMeta>, ContextApiError> {
    service(api)?.list(&api.store, id, cancelled)
}
pub fn get(
    api: &WorkerApi,
    id: &CommandId,
    path: &str,
) -> Result<(Vec<u8>, ContextFileMeta), ContextApiError> {
    service(api)?.get(&api.store, id, path)
}
