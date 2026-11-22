//! Owned by workflow-scratch-push: command authorization and attempt checks.
use super::ContextApiError;
use crate::data::{
    context::{CommandInvocationPins, ContextFileMeta, ContextRelPath},
    fs::{api_db::SqliteSessionStore, api_paths::ApiPaths, context_dirs::ContextDirResolver},
    ids::CommandId,
};
use crate::engine::workflow::context_files::{WorkflowContextError, WorkflowContextReader};
use std::sync::atomic::AtomicBool;
/// Capability switch: flip only when push scratch routes and retention exist.
pub const SUPPORTED: bool = true;
pub struct WorkflowContextService {
    pins: CommandInvocationPins,
    resolver: ContextDirResolver,
}
impl WorkflowContextService {
    pub fn new(paths: ApiPaths, resolver: ContextDirResolver) -> Self {
        Self {
            pins: CommandInvocationPins::new(paths),
            resolver,
        }
    }
    pub fn list(
        &self,
        store: &SqliteSessionStore,
        id: &CommandId,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ContextFileMeta>, ContextApiError> {
        let Some((reader, attempt)) = self.reader(store, id)? else {
            return Ok(vec![]);
        };
        let items = reader.list(cancelled).map_err(map_reader_error)?;
        if !same_attempt(store, id, attempt)? {
            return Ok(vec![]);
        }
        Ok(items)
    }
    pub fn get(
        &self,
        store: &SqliteSessionStore,
        id: &CommandId,
        path: &str,
    ) -> Result<(Vec<u8>, ContextFileMeta), ContextApiError> {
        // Validate even when the command has no invocation. No raw path reaches storage.
        ContextRelPath::parse(path).map_err(|e| ContextApiError::InvalidPath(e.to_string()))?;
        let (reader, attempt) = self.reader(store, id)?.ok_or_else(not_found)?;
        let file = reader.read(path).map_err(map_reader_error)?;
        if !same_attempt(store, id, attempt)? {
            return Err(not_found());
        }
        Ok(file)
    }

    fn reader(
        &self,
        store: &SqliteSessionStore,
        id: &CommandId,
    ) -> Result<Option<(WorkflowContextReader, u32)>, ContextApiError> {
        // Like /outputs, existence of the command row is sufficient; closing a
        // session or finishing/cancelling a command does not revoke its files.
        let command = store
            .get_command(id.as_str())?
            .ok_or_else(|| ContextApiError::CommandNotFound("command not found".into()))?;
        let Some(pin) = self
            .pins
            .current(&command.session_id, id.as_str(), command.attempt)
            .map_err(|error| {
                tracing::warn!(%error, "workflow context pin unavailable");
                ContextApiError::Unavailable("workflow context is unavailable".into())
            })?
        else {
            return Ok(None);
        };
        let reader = WorkflowContextReader::for_invocation(&self.resolver, pin.invocation_id)
            .map_err(|error| {
                tracing::warn!(%error, "Workflow context directory is unavailable");
                ContextApiError::Unavailable("workflow context is unavailable".into())
            })?;
        Ok(Some((reader, command.attempt)))
    }
}

fn same_attempt(
    store: &SqliteSessionStore,
    id: &CommandId,
    attempt: u32,
) -> Result<bool, ContextApiError> {
    Ok(store
        .get_command(id.as_str())?
        .is_some_and(|row| row.attempt == attempt))
}
fn not_found() -> ContextApiError {
    ContextApiError::NotFound("workflow context file not found".into())
}
fn map_reader_error(error: WorkflowContextError) -> ContextApiError {
    match error {
        WorkflowContextError::NotFound => not_found(),
        WorkflowContextError::Path(e) => ContextApiError::InvalidPath(e.to_string()),
        WorkflowContextError::TooLarge { .. } => ContextApiError::TooLarge(error.to_string()),
        WorkflowContextError::Incomplete => {
            ContextApiError::Unavailable("workflow context listing could not complete".into())
        }
        WorkflowContextError::Data(e) => ContextApiError::Data(e),
    }
}
