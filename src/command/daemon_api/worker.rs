//! Typed worker operations and their shared handles. No HTTP router lives here.
use crate::command::commands::api_server::admission::ApiAdmission;
use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::api_server::session_setup::SessionSetupBus;
use crate::command::commands::api_server::{
    ApiSessionLifecycle, CancelOutcome, CommandCanceller, TokenRegistry,
};
use crate::data::api_lifecycle_store::CommandLifecycleStore;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session_manager::SessionManager;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
// ─── Shared state ────────────────────────────────────────────────────────────
pub struct WorkerApi {
    pub live_config: std::sync::RwLock<Option<crate::data::config::EffectiveConfig>>,
    pub settings_tx: Option<tokio::sync::watch::Sender<crate::data::config::DaemonSettings>>,
    pub store: Arc<SqliteSessionStore>,
    pub lifecycle_store: CommandLifecycleStore,
    pub canceller: CommandCanceller,
    pub setup_cancels: Arc<TokenRegistry>,
    pub paths: ApiPaths,
    pub workdirs: Vec<PathBuf>,
    pub workdir_roots: Vec<PathBuf>,
    pub started_at: Instant,
    pub task_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub engines: crate::command::dispatch::Engines,
    pub sessions: Arc<SessionManager>,
    pub event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    pub setup_buses: tokio::sync::Mutex<HashMap<String, Arc<SessionSetupBus>>>,
}

impl WorkerApi {
    pub fn push_enabled(&self) -> bool {
        self.live_config
            .read()
            .expect("worker config lock")
            .as_ref()
            .map(|config| config.daemon().worker.push_enabled)
            .unwrap_or_else(|| {
                crate::data::config::daemon::DaemonSettings::from_merged(
                    &self
                        .engines
                        .global_config
                        .daemon
                        .clone()
                        .unwrap_or_default(),
                )
                .worker
                .push_enabled
            })
    }
    pub fn admission(&self) -> ApiAdmission {
        let live = self.live_config.read().expect("worker config lock").clone();
        let mut engines = self.engines.clone();
        let (workdirs, roots) = if let Some(config) = live {
            engines.global_config = Arc::new(config.config().clone());
            (
                config
                    .worker_push_work_dirs()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                config
                    .worker_push_work_dir_roots()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            )
        } else {
            (self.workdirs.clone(), self.workdir_roots.clone())
        };
        ApiAdmission::new(
            Arc::clone(&self.store),
            self.lifecycle_store.clone(),
            self.paths.clone(),
            engines,
            Arc::clone(&self.sessions),
            workdirs,
            roots,
        )
    }

    pub fn lifecycle(&self) -> ApiSessionLifecycle {
        ApiSessionLifecycle::new(
            Arc::clone(&self.store),
            self.engines.clone(),
            Arc::clone(&self.sessions),
            self.paths.clone(),
        )
    }
}

use crate::data::error::DataError;
use crate::data::fs::api_db::{CommandListFilter, CommandRecord, SessionRecord};
use crate::data::fs::api_labels::LabelSelector;
use crate::data::ids::{CommandId, WorkerSessionId};
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::session_status::SessionStatus;

/// The route operations expose typed inputs independently of HTTP extractors.
pub fn sessions_list(
    api: &WorkerApi,
    status: Option<SessionStatus>,
    setup: Option<SessionSetupStatus>,
    labels: &[LabelSelector],
) -> Result<Vec<SessionRecord>, DataError> {
    api.store.list_sessions_filtered(status, setup, labels)
}
pub fn sessions_get(
    api: &WorkerApi,
    id: &WorkerSessionId,
) -> Result<Option<SessionRecord>, DataError> {
    api.store.get_session(id.as_str())
}
pub async fn sessions_delete(
    api: &WorkerApi,
    id: &WorkerSessionId,
) -> Result<crate::command::commands::api_server::CloseOutcome, crate::command::error::CommandError>
{
    let outcome = api.lifecycle().close(id.as_str()).await?;
    if let Ok(commands) = api.store.list_commands_for_session(id.as_str(), i64::MAX) {
        for command in commands.into_iter().filter(|c| c.status.is_terminal()) {
            api.engines
                .ephemeral_secrets
                .remove(&CommandId::parse(&command.id).expect("persisted command id"));
        }
    }
    Ok(outcome)
}
pub fn sessions_queue(
    api: &WorkerApi,
    id: &WorkerSessionId,
) -> Result<
    Option<crate::command::commands::api_server::runtime::SessionQueueView>,
    crate::command::commands::api_server::runtime::QueueViewError,
> {
    api.lifecycle().queue_view(id.as_str())
}
pub async fn sessions_create(
    api: &WorkerApi,
    context: crate::data::daemon_state::CommandRequestContext,
    request: crate::command::commands::api_server::admission::SessionSubmission,
    command: Option<crate::command::commands::api_server::admission::CommandSubmission>,
    close_on_finish: bool,
    journal: Option<&crate::command::commands::api_server::admission::IdempotentAdmission>,
) -> Result<
    crate::command::commands::api_server::admission::AdmittedSession,
    crate::command::commands::api_server::admission::AdmissionError,
> {
    if !api.push_enabled() {
        return Err(
            crate::command::commands::api_server::admission::AdmissionError::FeatureDisabled,
        );
    }
    api.admission()
        .with_request_context(context)
        .admit_session(request, command, close_on_finish, journal)
        .await
}
pub async fn runs_create(
    api: &WorkerApi,
    context: crate::data::daemon_state::CommandRequestContext,
    session: crate::command::commands::api_server::admission::SessionSubmission,
    command: crate::command::commands::api_server::admission::CommandSubmission,
    close_on_finish: bool,
    journal: Option<&crate::command::commands::api_server::admission::IdempotentAdmission>,
) -> Result<
    crate::command::commands::api_server::admission::AdmittedSession,
    crate::command::commands::api_server::admission::AdmissionError,
> {
    sessions_create(
        api,
        context,
        session,
        Some(command),
        close_on_finish,
        journal,
    )
    .await
}
pub fn commands_list(
    api: &WorkerApi,
    query: &CommandListFilter<'_>,
) -> Result<Vec<CommandRecord>, DataError> {
    api.store.list_commands_filtered(query)
}
pub fn commands_get(api: &WorkerApi, id: &CommandId) -> Result<Option<CommandRecord>, DataError> {
    api.store.get_command(id.as_str())
}
pub async fn commands_create(
    api: &WorkerApi,
    context: crate::data::daemon_state::CommandRequestContext,
    id: &WorkerSessionId,
    request: crate::command::commands::api_server::admission::CommandSubmission,
    journal: Option<&crate::command::commands::api_server::admission::IdempotentAdmission>,
) -> Result<
    crate::command::commands::api_server::admission::AdmittedCommand,
    crate::command::commands::api_server::admission::AdmissionError,
> {
    if !api.push_enabled() {
        return Err(
            crate::command::commands::api_server::admission::AdmissionError::FeatureDisabled,
        );
    }
    api.admission()
        .with_request_context(context)
        .admit_command(id.as_str(), request, journal)
        .await
}
pub async fn commands_cancel(
    api: &WorkerApi,
    id: &CommandId,
) -> Result<
    crate::command::commands::api_server::CancelOutcome,
    crate::command::commands::api_server::runtime::CancelSessionError,
> {
    let outcome = api
        .lifecycle()
        .cancel_command(
            id.as_str(),
            &api.canceller,
            &api.setup_cancels,
            &api.lifecycle_store,
        )
        .await?;
    if matches!(
        &outcome,
        CancelOutcome::Cancelled(_) | CancelOutcome::AlreadyTerminal(_)
    ) {
        api.engines.ephemeral_secrets.remove(id);
    }
    Ok(outcome)
}
pub fn commands_workflow(
    api: &WorkerApi,
    id: &CommandId,
) -> Result<Option<serde_json::Value>, DataError> {
    let Some(command) = commands_get(api, id)? else {
        return Ok(None);
    };
    api.paths
        .read_command_workflow_state_raw(&command.session_id, id.as_str())?
        .map(|raw| serde_json::from_str(&raw).map_err(|e| DataError::Other(e.to_string())))
        .transpose()
}
pub async fn commands_events(
    api: &WorkerApi,
    command: CommandRecord,
    after: Option<crate::data::execution_event::EventCursor>,
    step: Option<String>,
    poll: std::time::Duration,
) -> crate::command::commands::api_server::command_log_stream::CommandLogStream {
    crate::command::commands::api_server::command_log_stream::CommandLogStream::open(
        api.store.clone(),
        api.paths.clone(),
        api.event_buses.clone(),
        command,
        after,
        step,
        poll,
    )
    .await
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("command not found")]
    CommandNotFound,
    #[error(transparent)]
    Storage(#[from] DataError),
    #[error(transparent)]
    Listing(#[from] std::io::Error),
    #[error(transparent)]
    Path(#[from] crate::data::fs::command_outputs::OutputPathError),
}
fn output_dir(api: &WorkerApi, id: &CommandId) -> Result<PathBuf, OutputError> {
    let command = commands_get(api, id)?.ok_or(OutputError::CommandNotFound)?;
    Ok(api
        .paths
        .command_output_dir(&command.session_id, id.as_str()))
}
pub fn commands_outputs(
    api: &WorkerApi,
    id: &CommandId,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<crate::data::fs::command_outputs::OutputListing, OutputError> {
    Ok(crate::data::fs::command_outputs::list_output_dir(
        &output_dir(api, id)?,
        cancelled,
    )?)
}
pub fn commands_output(
    api: &WorkerApi,
    id: &CommandId,
    path: &str,
) -> Result<Option<crate::data::fs::command_outputs::OpenedOutput>, OutputError> {
    Ok(crate::data::fs::command_outputs::open_output_file(
        &output_dir(api, id)?,
        path,
    )?)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkdirKind {
    Allow,
    Root,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Workdir {
    pub path: PathBuf,
    pub kind: WorkdirKind,
}
pub fn workdirs(api: &WorkerApi) -> Vec<Workdir> {
    let live = api.live_config.read().expect("worker config lock");
    let (allow, roots) = live
        .as_ref()
        .map(|c| {
            (
                c.worker_push_work_dirs()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                c.worker_push_work_dir_roots()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            )
        })
        .unwrap_or_else(|| (api.workdirs.clone(), api.workdir_roots.clone()));
    allow
        .into_iter()
        .map(|path| Workdir {
            path,
            kind: WorkdirKind::Allow,
        })
        .chain(roots.into_iter().map(|path| Workdir {
            path,
            kind: WorkdirKind::Root,
        }))
        .collect()
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueRecord {
    pub command_id: CommandId,
    pub subcommand: String,
    #[serde(default)]
    pub args: serde_json::Value,
    pub status: crate::data::command_status::CommandStatus,
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    pub queued_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub position: Option<i64>,
    pub worker_id: Option<String>,
    pub exit_code: Option<i32>,
    #[serde(deserialize_with = "stored_labels")]
    pub labels: crate::data::fs::api_labels::Labels,
}

pub fn stored_labels<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<crate::data::fs::api_labels::Labels, D::Error> {
    use serde::Deserialize;
    std::collections::BTreeMap::<String, String>::deserialize(d)
        .map(crate::data::fs::api_labels::Labels::from_stored)
}

/// A command's place in the worker's one job model (WI 0143).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandJob {
    pub origin: Option<crate::data::worker::JobOrigin>,
    pub outputs_mounted: Option<bool>,
}
/// The job side tables beside the command rows (created on first use).
pub fn job_store(api: &WorkerApi) -> Result<crate::data::worker::job_store::JobStore, DataError> {
    crate::data::worker::job_store::JobStore::open(api.store.clone())
}
pub fn commands_job(api: &WorkerApi, id: &str) -> Result<CommandJob, DataError> {
    let jobs = job_store(api)?;
    Ok(CommandJob {
        origin: jobs.get(id)?.map(|(origin, _)| origin),
        outputs_mounted: jobs.outputs_mounted(id)?,
    })
}
/// Pushed jobs now running and queued (`/v1/status` `worker.push`). A row
/// with no recorded origin has not been taken by a lease runner, so it is a
/// pushed command.
pub fn push_status(api: &WorkerApi) -> Result<PushStatus, DataError> {
    let jobs = job_store(api)?;
    let pulled: std::collections::HashSet<String> = jobs
        .command_ids(crate::data::worker::JobClass::Pull)?
        .into_iter()
        .collect();
    let count = |status| -> Result<usize, DataError> {
        Ok(api
            .store
            .list_commands_filtered(&CommandListFilter {
                status: Some(status),
                session_id: None,
                labels: &[],
                limit: i64::MAX,
            })?
            .iter()
            .filter(|row| !pulled.contains(&row.id))
            .count())
    };
    Ok(PushStatus {
        running: count(crate::data::command_status::CommandStatus::Running)?,
        queued: count(crate::data::command_status::CommandStatus::Queued)?,
    })
}
/// `worker.push` in `/v1/status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PushStatus {
    pub running: usize,
    pub queued: usize,
}
