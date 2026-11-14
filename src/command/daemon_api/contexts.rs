//! Typed daemon entry points for the keyed repo context routes and a push
//! command's `context_changes`. Thin: every rule lives in
//! `command::context::{repo_api, change_tracker}`.
use crate::{
    command::{
        context::{
            change_tracker::ContextChangeTracker, repo_api::RepoContextService, ContextApiError,
        },
        daemon_api::{worker::WorkerApi, DaemonApi},
    },
    data::{
        context::{ContextChange, ContextFileMeta, WriteOutcome},
        fs::context_dirs::ContextDirResolver,
        ids::CommandId,
    },
};
use std::sync::atomic::AtomicBool;
fn service(api: &DaemonApi) -> Result<RepoContextService, ContextApiError> {
    Ok(RepoContextService::new(ContextDirResolver::from_env(
        api.runtime.config().env(),
    )?))
}
pub fn list(
    api: &DaemonApi,
    key: &str,
    cancelled: &AtomicBool,
) -> Result<Vec<ContextFileMeta>, ContextApiError> {
    service(api)?.list(key, cancelled)
}
pub fn get(
    api: &DaemonApi,
    key: &str,
    path: &str,
) -> Result<(Vec<u8>, ContextFileMeta), ContextApiError> {
    service(api)?.get(key, path)
}
pub fn put(
    api: &DaemonApi,
    key: &str,
    path: &str,
    bytes: &[u8],
    if_match: Option<&str>,
) -> Result<(ContextFileMeta, WriteOutcome), ContextApiError> {
    service(api)?.put(key, path, bytes, if_match)
}
pub fn delete(
    api: &DaemonApi,
    key: &str,
    path: &str,
    if_match: Option<&str>,
) -> Result<(), ContextApiError> {
    service(api)?.delete(key, path, if_match)
}
pub fn command_changes(
    api: &WorkerApi,
    id: &CommandId,
    cancelled: &AtomicBool,
) -> Result<Vec<ContextChange>, ContextApiError> {
    let command = api
        .store
        .get_command(id.as_str())?
        .ok_or_else(|| ContextApiError::CommandNotFound("command not found".into()))?;
    let resolver = {
        let config = api.live_config.read().expect("worker config lock");
        let config = config.as_ref().ok_or_else(|| {
            ContextApiError::Unavailable("worker configuration unavailable".into())
        })?;
        ContextDirResolver::from_env(config.env())?
    };
    let live = crate::command::context::change_tracker::live_dir(&command.id);
    ContextChangeTracker::changes_for(&api.paths, &resolver, &command, live.as_deref(), cancelled)
}
