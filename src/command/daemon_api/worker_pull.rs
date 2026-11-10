//! Typed pull operations of the worker feature (WI 0143): connection state
//! per `workServers` entry, the eligible queue, manual claims and awman's
//! lease resources. No HTTP lives here; `src/frontend/api/worker_pull.rs`
//! binds the seven `/v1/worker/{servers,queue,leases}` routes to these.
//!
//! The pull runtime (registration, claim loop, lease runner) is Layer 2
//! worker policy behind [`PullHandle`]. Lease resources are read from the
//! durable `LeaseStore`, so `GET /v1/worker/leases` answers even when no
//! pull worker is running.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::DaemonApi;
use crate::data::config::work_servers::ClaimMode;
use crate::data::error::DataError;
use crate::data::ids::LeaseId;
use crate::data::wire::{List, ListQuery, WireError};
use crate::data::worker::lease_store::{LeaseFilter, LeaseStore};
use crate::data::worker::wire::LeaseResource;
use crate::data::worker::{LeaseRecord, LeaseState};

/// One `workServers` entry as the daemon sees it (`GET /v1/worker/servers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub name: String,
    /// `registered`, `registering`, `disconnected`, `revoked`, `error`,
    /// `retiring` or `disabled`.
    pub state: String,
    pub enabled: bool,
    pub claim: ClaimMode,
    pub max_concurrent_jobs: usize,
    /// Leases of this server the daemon holds now.
    pub running: usize,
    /// Items the server lists as eligible for this worker.
    pub queued_eligible: usize,
    /// Labels this worker reports and configures for itself.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Labels the server assigned at registration (never set by the worker).
    #[serde(default)]
    pub server_labels: BTreeMap<String, String>,
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    pub last_claim_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub use crate::data::worker::wire::{PullStatus, QueueRow, ReleaseBody, ServerSummary};
impl From<&ServerStatus> for ServerSummary {
    fn from(s: &ServerStatus) -> Self {
        Self {
            name: s.name.clone(),
            state: s.state.clone(),
            running: s.running,
            queued_eligible: s.queued_eligible,
        }
    }
}

/// Failures of the pull operations. `code()`/`http_status()` are the wire
/// mapping every transport uses.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("{0}")]
    PullDisabled(String),
    #[error("unknown work server {0:?}")]
    UnknownServer(String),
    #[error("work server {0:?} is not registered")]
    ServerNotRegistered(String),
    #[error("queue item {0:?} not found")]
    ItemNotFound(String),
    #[error("lease not found")]
    LeaseNotFound,
    #[error("lease {0} has already ended")]
    LeaseEnded(String),
    #[error("the item was claimed by another worker")]
    Taken,
    #[error("the item is not eligible for this worker: {0}")]
    Ineligible(String),
    /// The item may still be eligible; this worker has no room for it now.
    #[error("the worker is busy: {0}")]
    Busy(String),
    #[error("the link to work server {0:?} was revoked; run `awman config unset workServers.{0}` and then `awman link` again")]
    LinkRevoked(String),
    #[error("work server unavailable: {0}")]
    Upstream(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] DataError),
}
impl WorkerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PullDisabled(_) => "pull_disabled",
            Self::UnknownServer(_) => "server_not_found",
            Self::ServerNotRegistered(_) => "server_not_registered",
            Self::ItemNotFound(_) => "item_not_found",
            Self::LeaseNotFound => "lease_not_found",
            Self::LeaseEnded(_) => "lease_ended",
            Self::Taken => "taken",
            Self::Ineligible(_) => "ineligible",
            Self::Busy(_) => "worker_busy",
            Self::LinkRevoked(_) => "link_revoked",
            Self::Upstream(_) => "upstream_unavailable",
            Self::Invalid(_) => "invalid_request",
            Self::Store(_) => crate::data::fs::api_db::failure_codes::INTERNAL,
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            Self::PullDisabled(_) | Self::ServerNotRegistered(_) => 503,
            Self::UnknownServer(_) | Self::ItemNotFound(_) | Self::LeaseNotFound => 404,
            Self::LeaseEnded(_)
            | Self::Taken
            | Self::Ineligible(_)
            | Self::Busy(_)
            | Self::LinkRevoked(_) => 409,
            Self::Upstream(_) => 502,
            Self::Invalid(_) => 400,
            Self::Store(_) => 500,
        }
    }
}
impl From<WireError> for WorkerError {
    fn from(e: WireError) -> Self {
        Self::Invalid(e.to_string())
    }
}

/// The Layer 2 pull worker the daemon runs (registration, claim loop, lease
/// runner). The operations below bind to it; without one, the pull routes
/// still answer from config and the lease store.
#[async_trait::async_trait]
pub trait PullHandle: Send + Sync {
    /// An owner-local foreground lease poll renews the execution owner's liveness.
    fn touch_foreground(&self, _id: &LeaseId) {}
    /// Cancel all owned leases, including preparation before a job row exists.
    fn cancel_jobs(&self) {}
    /// One entry per configured `workServers` entry.
    fn servers(&self) -> Vec<ServerStatus>;
    /// Eligible items, all servers merged when `server` is `None`.
    async fn queue(
        &self,
        server: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<List<QueueRow>, WorkerError>;
    /// A manual claim through the shared worker admission path.
    async fn claim(&self, item_id: &str, server: Option<&str>) -> Result<LeaseRecord, WorkerError>;
    async fn prepare_foreground(
        &self,
        _item_id: &str,
        _server: Option<&str>,
    ) -> Result<crate::command::worker::tui_host::ForegroundPick, WorkerError> {
        Err(WorkerError::PullDisabled(
            "foreground hosting is unavailable".into(),
        ))
    }
    async fn foreground_events(
        &self,
        _id: &LeaseId,
        _events: Vec<crate::data::execution_event::ExecutionEvent>,
    ) -> Result<(), WorkerError> {
        Err(WorkerError::LeaseNotFound)
    }
    async fn foreground_terminal(
        &self,
        _id: &LeaseId,
        _frames: Vec<awman_wsp::terminal::LeaseTerminalFrame>,
    ) -> Result<Vec<awman_wsp::terminal::LeaseTerminalFrame>, WorkerError> {
        Err(WorkerError::LeaseNotFound)
    }
    async fn foreground_checkout(
        &self,
        _id: &LeaseId,
        _workdir: std::path::PathBuf,
    ) -> Result<crate::command::worker::tui_host::ForegroundPick, WorkerError> {
        Err(WorkerError::LeaseNotFound)
    }
    async fn foreground_result(
        &self,
        _id: &LeaseId,
        _result: crate::data::worker::wire::ForegroundResult,
    ) -> Result<crate::data::fs::api_db::CommandResult, WorkerError> {
        Err(WorkerError::LeaseNotFound)
    }
    /// Give an active lease back to its server.
    async fn release(
        &self,
        lease: &LeaseRecord,
        reason: awman_wsp::ReleaseReason,
        message: Option<String>,
    ) -> Result<LeaseRecord, WorkerError>;
    /// The `lease_*` / `worker_pull` capability strings this runtime honours
    /// (advertised in `/v1/status` only while it runs).
    fn capabilities(&self) -> Vec<&'static str>;
}

fn lease_store(api: &DaemonApi) -> Result<Option<LeaseStore>, WorkerError> {
    match &api.worker {
        Some(worker) => Ok(Some(LeaseStore::open(worker.store.clone())?)),
        None => Ok(None),
    }
}

fn pull_enabled(api: &DaemonApi) -> Result<(), WorkerError> {
    if api.runtime.config().daemon().worker.pull_enabled {
        Ok(())
    } else {
        Err(WorkerError::PullDisabled(
            "the pull side is off (daemon.features.worker.pull.enabled is false)".into(),
        ))
    }
}

fn handle(api: &DaemonApi) -> Result<&dyn PullHandle, WorkerError> {
    pull_enabled(api)?;
    api.pull.as_deref().ok_or_else(|| {
        WorkerError::PullDisabled("the pull worker is not running in this daemon".into())
    })
}

/// Connection state per `workServers` entry.
pub fn servers_list(api: &DaemonApi) -> Result<Vec<ServerStatus>, WorkerError> {
    if let Some(pull) = api.pull.as_deref() {
        return Ok(pull.servers());
    }
    // No pull worker: every entry is reported from config, with its held
    // leases from the store.
    let config = api.runtime.config();
    let pull_on = config.daemon().worker.pull_enabled;
    let leases = lease_store(api)?;
    config
        .work_servers()
        .into_iter()
        .map(|(name, entry)| {
            let running = match &leases {
                Some(store) => store.active_for_server(&name)?.len(),
                None => 0,
            };
            let disabled = !pull_on || !entry.enabled;
            Ok(ServerStatus {
                state: if disabled { "disabled" } else { "disconnected" }.into(),
                error: (!disabled).then(|| "the pull worker is not running".to_string()),
                enabled: entry.enabled,
                claim: entry.claim,
                max_concurrent_jobs: entry.max_concurrent_jobs,
                running,
                queued_eligible: 0,
                labels: entry.labels,
                server_labels: BTreeMap::new(),
                last_claim_at: None,
                name,
            })
        })
        .collect()
}

pub fn servers_get(api: &DaemonApi, name: &str) -> Result<ServerStatus, WorkerError> {
    servers_list(api)?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| WorkerError::UnknownServer(name.into()))
}

fn known_server(api: &DaemonApi, server: Option<&str>) -> Result<(), WorkerError> {
    match server {
        Some(name) if !api.runtime.config().work_servers().contains_key(name) => {
            Err(WorkerError::UnknownServer(name.into()))
        }
        _ => Ok(()),
    }
}

/// Eligible items, as `{items, next_cursor}`.
pub async fn queue_list(
    api: &DaemonApi,
    server: Option<&str>,
    query: &ListQuery,
) -> Result<List<QueueRow>, WorkerError> {
    // The cursor is the pull worker's own (merged WSP cursors); only the
    // limit follows awman's paging rules here.
    let limit = ListQuery {
        limit: query.limit,
        cursor: None,
    }
    .page()?
    .limit;
    if query.cursor.as_deref() == Some("") {
        return Err(WorkerError::Invalid("invalid cursor".into()));
    }
    known_server(api, server)?;
    handle(api)?
        .queue(server, limit, query.cursor.as_deref())
        .await
}

/// A manual pick; the daemon runs the claimed job (`host: daemon`).
pub async fn queue_claim(
    api: &DaemonApi,
    item_id: &str,
    server: Option<&str>,
) -> Result<LeaseResource, WorkerError> {
    if item_id.is_empty() {
        return Err(WorkerError::Invalid("item id is empty".into()));
    }
    known_server(api, server)?;
    let lease = handle(api)?.claim(item_id, server).await?;
    Ok(LeaseResource::from(&lease))
}

pub async fn queue_prepare_foreground(
    api: &DaemonApi,
    item: &str,
    server: Option<&str>,
) -> Result<crate::command::worker::tui_host::ForegroundPick, WorkerError> {
    known_server(api, server)?;
    handle(api)?.prepare_foreground(item, server).await
}
pub async fn foreground_events(
    api: &DaemonApi,
    id: &LeaseId,
    events: Vec<crate::data::execution_event::ExecutionEvent>,
) -> Result<(), WorkerError> {
    api.pull
        .as_deref()
        .ok_or_else(|| WorkerError::PullDisabled("worker stopped".into()))?
        .foreground_events(id, events)
        .await
}
pub async fn foreground_terminal(
    api: &DaemonApi,
    id: &LeaseId,
    frames: Vec<awman_wsp::terminal::LeaseTerminalFrame>,
) -> Result<Vec<awman_wsp::terminal::LeaseTerminalFrame>, WorkerError> {
    api.pull
        .as_deref()
        .ok_or_else(|| WorkerError::PullDisabled("worker stopped".into()))?
        .foreground_terminal(id, frames)
        .await
}
pub async fn foreground_checkout(
    api: &DaemonApi,
    id: &LeaseId,
    workdir: std::path::PathBuf,
) -> Result<crate::command::worker::tui_host::ForegroundPick, WorkerError> {
    api.pull
        .as_deref()
        .ok_or_else(|| WorkerError::PullDisabled("worker stopped".into()))?
        .foreground_checkout(id, workdir)
        .await
}
pub async fn foreground_result(
    api: &DaemonApi,
    id: &LeaseId,
    result: crate::data::worker::wire::ForegroundResult,
) -> Result<crate::data::fs::api_db::CommandResult, WorkerError> {
    api.pull
        .as_deref()
        .ok_or_else(|| WorkerError::PullDisabled("worker stopped".into()))?
        .foreground_result(id, result)
        .await
}

/// `?state=` on `GET /v1/worker/leases`: one lease state or `active`.
pub fn parse_state_filter(raw: &str) -> Result<&'static [LeaseState], WorkerError> {
    const ALL: &[LeaseState] = &[
        LeaseState::Claimed,
        LeaseState::Preparing,
        LeaseState::Running,
        LeaseState::AwaitingInput,
        LeaseState::Pushing,
        LeaseState::Reporting,
        LeaseState::Done,
        LeaseState::Released,
        LeaseState::Lost,
    ];
    if raw == "active" {
        return Ok(LeaseState::ACTIVE);
    }
    let state = LeaseState::parse(raw).ok_or_else(|| {
        WorkerError::Invalid(format!(
            "invalid state={raw:?} (expected active or a lease state)"
        ))
    })?;
    let at = ALL.iter().position(|s| *s == state).expect("listed state");
    Ok(&ALL[at..=at])
}

/// awman's lease resources, newest claim first.
pub fn leases_list(
    api: &DaemonApi,
    server: Option<&str>,
    state: Option<&str>,
    query: &ListQuery,
) -> Result<List<LeaseResource>, WorkerError> {
    let states = state.map(parse_state_filter).transpose()?;
    let Some(store) = lease_store(api)? else {
        return Ok(super::core::keyed_page(
            Vec::new(),
            query,
            |l: &LeaseResource| &l.id,
        )?);
    };
    let leases: Vec<LeaseResource> = store
        .list(&LeaseFilter {
            server,
            states,
            ..Default::default()
        })?
        .iter()
        .map(LeaseResource::from)
        .collect();
    Ok(super::core::keyed_page(leases, query, |l| &l.id)?)
}

pub fn leases_get(api: &DaemonApi, id: &LeaseId) -> Result<LeaseResource, WorkerError> {
    if let Some(pull) = &api.pull {
        pull.touch_foreground(id);
    }
    lease_store(api)?
        .map(|store| store.get(id))
        .transpose()?
        .flatten()
        .map(|lease| LeaseResource::from(&lease))
        .ok_or(WorkerError::LeaseNotFound)
}

/// Give a lease back to its server (`reason` defaults to `user_declined`).
pub async fn leases_release(
    api: &DaemonApi,
    id: &LeaseId,
    body: ReleaseBody,
) -> Result<LeaseResource, WorkerError> {
    let lease = lease_store(api)?
        .map(|store| store.get(id))
        .transpose()?
        .flatten()
        .ok_or(WorkerError::LeaseNotFound)?;
    if !lease.state.is_active() {
        return Err(WorkerError::LeaseEnded(id.to_string()));
    }
    let reason = body
        .reason
        .unwrap_or(awman_wsp::ReleaseReason::UserDeclined);
    // A lease outlives a disabled pull side: releasing it must still work
    // while the worker runs, so only a missing worker refuses.
    let pull = api.pull.as_deref().ok_or_else(|| {
        WorkerError::PullDisabled("the pull worker is not running in this daemon".into())
    })?;
    let released = pull.release(&lease, reason, body.message).await?;
    Ok(LeaseResource::from(&released))
}

/// `worker.pull` in `/v1/status`.
pub fn pull_status(api: &DaemonApi) -> PullStatus {
    PullStatus {
        servers: servers_list(api)
            .unwrap_or_default()
            .iter()
            .map(ServerSummary::from)
            .collect(),
    }
}

/// Capability strings of the running pull worker; none without one.
pub fn capabilities(api: &DaemonApi) -> Vec<&'static str> {
    if !api.runtime.config().daemon().worker.pull_enabled
        || api
            .runtime
            .feature_record(crate::data::daemon_state::FeatureName::Worker)
            .state
            != super::FeatureState::On
    {
        return Vec::new();
    }
    api.pull
        .as_deref()
        .map(PullHandle::capabilities)
        .unwrap_or_default()
}
