//! Pulled-lease durable context (WI 0146): admission, staging and result
//! policy. Layer 1 (`engine::worker::lease_context`) does the filesystem
//! work; this module decides what the work server is told.
//!
//! Result assembly for `context_changes` lives in [`collect`]. It runs from
//! `PushStage::process` (runner.rs) after the run and before the push, on
//! success, failure and cancel alike, and only while the lease may still
//! report. The runner attaches the stored value to `LeaseResult` or
//! `ReleaseRequest` inside its `may_report()` block, so a lost epoch posts
//! nothing.
use crate::{
    data::{
        execution_event::CommandStatusKind,
        fs::{
            api_db::{CommandFailure, CommandResult},
            context_dirs::ContextDirResolver,
        },
        ids::LeaseId,
    },
    engine::worker::{
        error::WorkerEngineError,
        lease_context::{remove_orphaned_lease_contexts, LeaseContextStage},
    },
};
use awman_wsp::codes;
use std::sync::atomic::AtomicBool;

/// Capability switch for `lease_context` (status and `register.features`).
/// Only daemon-hosted workers with safe context mutations advertise it.
/// Windows context access remains read-only, so pulled snapshots are refused.
pub const SUPPORTED: bool = cfg!(unix);

/// Refuse before any work: `Err(reason)` → `release(ineligible)`.
pub fn admit(item: &awman_wsp::WorkItem, registered: &[&str]) -> Result<(), String> {
    if item.context.is_some()
        && (!SUPPORTED || !registered.contains(&awman_wsp::features::LEASE_CONTEXT))
    {
        return Err("this worker does not support lease_context".into());
    }
    Ok(())
}

/// Before the clone. `Ok(None)` when the item carries no context. A
/// malformed or oversized snapshot is `Err(Context{..})`, which the runner
/// reports as a `preflight` failure before any clone or container.
pub fn stage(
    resolver: &ContextDirResolver,
    lease: &LeaseId,
    item: &awman_wsp::WorkItem,
) -> Result<Option<LeaseContextStage>, WorkerEngineError> {
    item.context
        .as_ref()
        .map(|context| LeaseContextStage::stage(resolver, lease, context))
        .transpose()
}

/// After the run, before the push stage, on success, failure and cancel
/// alike. On `context_too_large` returns the failed `CommandResult` (stage
/// `context`; an earlier failure or cancellation is kept) and metadata-only
/// changes. A failed result never reaches the push, so the branch is not
/// published when its memory update cannot be.
pub fn collect(
    stage: &LeaseContextStage,
    result: CommandResult,
    cancelled: &AtomicBool,
) -> (CommandResult, Option<awman_wsp::ContextChanges>) {
    match stage.collect(cancelled) {
        Ok(changes) => (result, Some(changes)),
        Err(WorkerEngineError::Context { code, message }) => {
            let metadata = if code == codes::CONTEXT_TOO_LARGE {
                stage.collect_metadata(cancelled).ok()
            } else {
                None
            };
            (fail(result, code, message), metadata)
        }
        Err(error) => (
            fail(result, codes::CONTEXT_INVALID, error.to_string()),
            None,
        ),
    }
}

/// Mark the result failed at stage `context`, unless the run already ended
/// without success: then its own failure is the one the server sees.
fn fail(mut result: CommandResult, code: &str, message: String) -> CommandResult {
    if matches!(result.outcome, None | Some(CommandStatusKind::Done)) {
        result.outcome = Some(CommandStatusKind::Error);
        result.exit_code = Some(1);
        result.failure = Some(CommandFailure {
            stage: "context".into(),
            code: code.into(),
            message,
            step: None,
        });
    }
    result
}

/// Startup: staged directories whose lease is not in `active`.
pub fn remove_orphans(resolver: &ContextDirResolver, active: &[LeaseId]) -> usize {
    remove_orphaned_lease_contexts(resolver, active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_worker_registers_lease_context_and_foreground_does_not() {
        let support = crate::engine::worker::lease_workspace::LeaseRuntimeSupport {
            read_only_workspace: false,
            outputs_mount: false,
        };
        let settings = crate::data::config::daemon::DaemonSettings::from_merged(
            &crate::data::config::daemon::DaemonConfig::default(),
        )
        .worker;
        let registered = super::super::capabilities::for_settings(support, &settings);
        assert_eq!(
            registered.contains(&awman_wsp::features::LEASE_CONTEXT),
            cfg!(unix)
        );
        let status = crate::command::daemon_api::capabilities::advertised(false, &registered);
        assert_eq!(
            status.iter().filter(|c| *c == "lease_context").count(),
            usize::from(cfg!(unix)),
            "{status:?}"
        );
        assert!(
            !crate::command::daemon_api::capabilities::advertised(false, &[])
                .iter()
                .any(|c| c == "lease_context")
        );
        let foreground = super::super::capabilities::features(
            support,
            settings.pull_interaction,
            settings.pull_allow_override,
            true,
            true,
            true,
        );
        assert!(!foreground.contains(&awman_wsp::features::LEASE_CONTEXT));
    }

    #[test]
    fn context_too_large_keeps_an_earlier_failure() {
        let done = CommandResult {
            outcome: Some(CommandStatusKind::Done),
            exit_code: Some(0),
            ..Default::default()
        };
        let failed = fail(done, codes::CONTEXT_TOO_LARGE, "big".into());
        assert_eq!(failed.outcome, Some(CommandStatusKind::Error));
        assert_eq!(failed.failure.as_ref().unwrap().stage, "context");
        let cancelled = CommandResult {
            outcome: Some(CommandStatusKind::Cancelled),
            exit_code: Some(130),
            failure: Some(CommandFailure {
                stage: "step".into(),
                code: "cancelled".into(),
                message: "x".into(),
                step: None,
            }),
            ..Default::default()
        };
        let kept = fail(cancelled.clone(), codes::CONTEXT_TOO_LARGE, "big".into());
        assert_eq!(kept, cancelled);
    }
}
