//! Shared preparation and execution/finalization for remote work item edits.
use crate::data::config::overlays::{
    parse_overlay_list, CollectedOverlays, ContextOverlaySpec, ContextScope, OverlayPermission,
    TypedOverlay,
};
use crate::data::config::runtime::RuntimeSelection;
use crate::data::error::DataError;
use crate::data::fs::context_dirs::ContextDirResolver;
use crate::data::fs::work_item_draft::WorkItemDraft;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::engine::agent_runtime::{AgentExecution, AgentExitInfo};
use crate::engine::error::EngineError;

use super::draft_sync::{DraftSyncError, DraftSyncEvent, FinalDraftSyncOutcome, RunningDraftSync};
use super::resolver::WorkItemError;

pub struct PreparedDraftEdit {
    pub draft: WorkItemDraft,
    pub seeded_body: String,
}

/// Call before launch/PTY ownership. The caller must use the amended collected
/// overlays when resolving and building the actual agent run options.
pub fn prepare_draft_edit(
    session: &Session,
    origin_url: Option<&str>,
    collected: &mut CollectedOverlays,
    body: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<PreparedDraftEdit, WorkItemError> {
    let config = session.effective_config();
    let runtime = config
        .runtime_selection()
        .map_err(|e| WorkItemError::Data(DataError::Other(format!("unknown runtime: {}", e.0))))?;
    if runtime == RuntimeSelection::DockerSbxExperimental {
        return Err(WorkItemError::Data(DataError::Other(
            "remote work item editing requires a runtime that supports context mounts".into(),
        )));
    }
    let repo_read_only = |spec: &ContextOverlaySpec| {
        spec.scope == ContextScope::Repo && spec.permission == OverlayPermission::ReadOnly
    };
    if collected.context_overlays.iter().any(repo_read_only) {
        return Err(WorkItemError::ReadOnlyContext);
    }
    // Check the effective config too, so even a collector that has discarded a
    // duplicate cannot accidentally widen an explicit read-only permission.
    for raw in config.config().overlays.iter().flatten() {
        let parsed = parse_overlay_list(raw).map_err(|reason| {
            WorkItemError::Data(DataError::InvalidOverlaySpec {
                spec: raw.clone(),
                reason,
            })
        })?;
        if parsed
            .iter()
            .any(|spec| matches!(spec, TypedOverlay::Context(ctx) if repo_read_only(ctx)))
        {
            return Err(WorkItemError::ReadOnlyContext);
        }
    }
    let existing = collected
        .context_overlays
        .iter()
        .find(|spec| spec.scope == ContextScope::Repo);
    let resolver = ContextDirResolver::from_env(session.env()).map_err(WorkItemError::Data)?;
    let mut paths =
        resolver.work_item_draft_paths(origin_url, session.git_root(), uuid::Uuid::new_v4());
    // Match LaunchPolicy's session/overlay context binding. Inferred contexts
    // use repo_dir; keyed and staged sessions must edit the directory mounted
    // at the same container path, rather than an unmounted inferred directory.
    let repo = crate::command::context::scope::RepoContextScope::new(&resolver, session)
        .resolve(existing.and_then(|spec| spec.key.as_ref()), origin_url);
    crate::data::fs::context_dirs::validate_context_path(resolver.awman_home(), &repo.host_dir)
        .map_err(WorkItemError::Data)?;
    let name = paths
        .host_path
        .file_name()
        .expect("generated draft filename")
        .to_owned();
    paths.host_path = repo.host_dir.join("scratch/work-items").join(name);
    let draft = WorkItemDraft::create(paths, body).map_err(WorkItemError::Data)?;
    if existing.is_none() {
        collected.context_overlays.push(ContextOverlaySpec {
            key: None,
            scope: ContextScope::Repo,
            permission: OverlayPermission::ReadWrite,
        });
    }
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Work item draft: {}", draft.paths().host_path.display()),
    });
    Ok(PreparedDraftEdit {
        draft,
        seeded_body: body.into(),
    })
}

pub struct DraftRunOutcome {
    pub execution: Result<AgentExitInfo, EngineError>,
    pub sync: Result<FinalDraftSyncOutcome, DraftSyncError>,
}

/// Wait once, forward sync events, then settle sync on every execution outcome.
/// The caller releases PTY state and replays queued messages after this returns.
pub async fn wait_with_draft_sync(
    execution: &mut AgentExecution,
    sync: RunningDraftSync,
    mut events: tokio::sync::mpsc::UnboundedReceiver<DraftSyncEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    sink: &mut dyn UserMessageSink,
) -> DraftRunOutcome {
    // wait moves the backend, so extract its standalone handle first.
    let mut cancel_handle = execution.cancel_handle();
    let wait = execution.wait();
    tokio::pin!(wait);
    let mut cancelled = false;
    let mut events_open = true;
    let execution = loop {
        tokio::select! {
            result = &mut wait => break result,
            event = events.recv(), if events_open => match event {
                Some(event) => forward_event(event, sink),
                None => events_open = false,
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if let Some(handle) = cancel_handle.take() {
                    // Cancellation is requested once. Keep the single wait
                    // future alive until the backend has released its frontend.
                    let result = tokio::task::spawn_blocking(move || handle.cancel()).await;
                    if !matches!(result, Ok(Ok(()))) {
                        sink.write_message(UserMessage { level: MessageLevel::Warning,
                            text: match result {
                                Ok(Err(error)) => format!("Agent cancellation failed: {error}"),
                                _ => "Agent cancellation task failed".into(),
                            } });
                    }
                }
            }
        }
    };
    let finalize = async move { sync.stop().await?.final_upload().await };
    tokio::pin!(finalize);
    let sync = loop {
        tokio::select! {
            result = &mut finalize => break result,
            event = events.recv(), if events_open => match event {
                Some(event) => forward_event(event, sink),
                None => events_open = false,
            },
        }
    };
    while let Ok(event) = events.try_recv() {
        forward_event(event, sink);
    }
    if let Err(error) = &sync {
        sink.write_message(UserMessage {
            level: MessageLevel::Error,
            text: error.to_string(),
        });
    }
    DraftRunOutcome { execution, sync }
}

fn forward_event(event: DraftSyncEvent, sink: &mut dyn UserMessageSink) {
    let (level, text) = match event {
        DraftSyncEvent::Created {
            source_id, number, ..
        } => {
            let number = number.map(|n| format!(" (#{n})")).unwrap_or_default();
            (
                MessageLevel::Info,
                format!("Created work item: {source_id}{number}"),
            )
        }
        DraftSyncEvent::Warning { message } => (MessageLevel::Warning, message),
        DraftSyncEvent::Error { message } => (MessageLevel::Error, message),
    };
    sink.write_message(UserMessage { level, text });
}
