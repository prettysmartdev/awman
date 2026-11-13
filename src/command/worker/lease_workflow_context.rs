//! Admission and result attachment for requested workflow artifacts.
use crate::{
    data::{
        context::CommandInvocationPins,
        fs::{api_paths::ApiPaths, context_dirs::ContextDirResolver},
    },
    engine::worker::lease_workflow_context::WorkflowArtifactCollector,
};
use std::sync::atomic::AtomicBool;

/// Capability switch: flip only when daemon-hosted artifact attachments exist.
pub const SUPPORTED: bool = true;

pub fn admit(item: &awman_wsp::WorkItem, registered: &[&str]) -> Result<(), String> {
    if item.workflow_context.is_some()
        && (!SUPPORTED || !registered.contains(&awman_wsp::features::LEASE_WORKFLOW_CONTEXT))
    {
        return Err("ineligible: this worker does not support lease_workflow_context".into());
    }
    Ok(())
}

pub fn collect(
    paths: &ApiPaths,
    resolver: &ContextDirResolver,
    item: &awman_wsp::WorkItem,
    session_id: &str,
    job_id: &str,
    attempt: u32,
    cancelled: &AtomicBool,
) -> Option<awman_wsp::WorkflowContextResult> {
    let request = item.workflow_context.as_ref()?;
    let pins = CommandInvocationPins::new(paths.clone());
    let pin = match pins.current(session_id, job_id, attempt) {
        Ok(Some(pin)) => pin,
        Ok(None) => {
            return Some(awman_wsp::WorkflowContextResult {
                collection_error: None,
                items: vec![],
                next_cursor: None,
                truncated: false,
            });
        }
        Err(error) => {
            tracing::warn!(%error, "could not resolve workflow artifact invocation pin");
            return Some(omitted());
        }
    };
    let collector = match WorkflowArtifactCollector::for_invocation(
        resolver,
        pin.invocation_id,
        request.max_bytes,
    ) {
        Ok(collector) => collector,
        Err(error) => {
            tracing::warn!(%error, "could not prepare workflow artifact collection");
            return Some(omitted());
        }
    };
    let result = collector.collect(cancelled);
    // A rerun can replace the pin while the directory is being read. Refuse
    // to attach artifacts unless this result still belongs to the current
    // command attempt and invocation.
    match pins.current(session_id, job_id, attempt) {
        Ok(Some(current)) if current.invocation_id == pin.invocation_id => Some(result),
        Ok(_) => Some(omitted()),
        Err(error) => {
            tracing::warn!(%error, "could not recheck workflow artifact invocation pin");
            Some(omitted())
        }
    }
}

pub(super) fn omitted() -> awman_wsp::WorkflowContextResult {
    awman_wsp::WorkflowContextResult {
        collection_error: Some(awman_wsp::WorkflowContextCollectionError::Unavailable),
        items: vec![],
        next_cursor: None,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn daemon_registers_the_artifact_capability_when_supported() {
        let settings = crate::data::config::daemon::WorkerSettings {
            enabled: true,
            max_concurrent_jobs: 1,
            push_enabled: false,
            push_interaction: crate::data::config::daemon::Interaction::Headless,
            push_allow_override: false,
            push_work_dirs: vec![],
            push_work_dir_roots: vec![],
            pull_enabled: true,
            pull_interaction: crate::data::config::daemon::Interaction::Headless,
            pull_allow_override: false,
            pull_work_root: None,
            work_servers: Default::default(),
        };
        let support = crate::engine::worker::lease_workspace::LeaseRuntimeSupport {
            read_only_workspace: false,
            outputs_mount: false,
        };
        assert!(super::super::capabilities::for_settings(support, &settings)
            .contains(&awman_wsp::features::LEASE_WORKFLOW_CONTEXT));
    }
}
