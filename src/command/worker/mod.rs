//! Production daemon pull controller. Policy, execution and lease ownership
//! live here; HTTP and terminal frontends adapt the typed PullHandle.
mod capabilities;
pub mod events;
pub mod pick;
pub mod policy;
mod provision;
pub mod queue_source;
mod runner;
mod terminal;
#[cfg(test)]
mod tests;
pub mod tui_host;

use crate::command::{
    commands::api_server::queue_worker::ApiCommandFrontendFactory,
    daemon_api::worker_pull::{PullHandle, QueueRow, ServerStatus, WorkerError},
    daemon_runtime::features::worker::{live_daemon_settings, WorkerShared},
};
use crate::data::{
    config::{daemon::WorkerSettings, work_servers::ClaimMode},
    wire::{List, ListQuery},
    worker::{
        labels::{HostFacts, RuntimeKind, WorkerHostKind},
        lease_store::LeaseStore,
        link_store::LinkStore,
        JobClass, LeaseHost, LeaseRecord, LeaseRelease, LeaseState,
    },
};
use crate::engine::{
    daemon::core::Core,
    worker::{
        error::WorkerEngineError,
        executor_share::{WorkerExecutorShare, WorkerPermit},
        heartbeat::HeartbeatControl,
        lease_workspace::LeaseRuntimeSupport,
        session::{LeaseRef, WspSession},
    },
};
use awman_wsp::{ClaimOutcome, RegisterRequest, ReleaseReason, ReleaseRequest};
use chrono::Utc;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Both daemon and foreground hosts re-execute the same private helper.
pub(super) fn credential_helper_program(
) -> std::io::Result<crate::engine::worker::credential_helper::HelperProgram> {
    std::env::current_exe().map(crate::engine::worker::credential_helper::HelperProgram::awman)
}

struct Registered {
    session: Arc<WspSession>,
    status: ServerStatus,
    heartbeat: Duration,
    url: String,
    registration: RegisterRequest,
}
#[derive(Default)]
struct Finish {
    release: Option<(ReleaseReason, Option<String>)>,
    reporting: bool,
}
struct Active {
    foreground_seen: Mutex<Option<std::time::Instant>>,
    started: AtomicBool,
    _owner: std::fs::File,
    server: String,
    control: HeartbeatControl,
    finish: Mutex<Finish>,
    outputs: Mutex<Option<BTreeMap<String, String>>>,
    done: CancellationToken,
}

pub struct PullController<F: ApiCommandFrontendFactory + 'static> {
    shared: Arc<WorkerShared>,
    factory: Arc<F>,
    leases: LeaseStore,
    servers: RwLock<BTreeMap<String, Registered>>,
    active: Mutex<BTreeMap<String, Arc<Active>>>,
    foreground: tokio::sync::Mutex<BTreeMap<String, (tui_host::ManualRun, WorkerPermit)>>,
    claim_gates: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    share: Arc<WorkerExecutorShare>,
    hub: Arc<crate::engine::terminal_hub::TerminalHub>,
    accepting: AtomicBool,
    shutting_down: AtomicBool,
    claim_stop: Mutex<CancellationToken>,
    core_shutdown: CancellationToken,
    helper: crate::engine::worker::credential_helper::HelperProgram,
}

impl<F: ApiCommandFrontendFactory + 'static> PullController<F> {
    pub fn new(
        shared: Arc<WorkerShared>,
        factory: Arc<F>,
        core: &Core,
    ) -> Result<Arc<Self>, crate::engine::error::EngineError> {
        let helper = credential_helper_program()
            .map_err(|e| crate::engine::error::EngineError::Other(e.to_string()))?;
        Ok(Arc::new(Self {
            leases: LeaseStore::open(shared.store.clone())?,
            shared,
            factory,
            servers: Default::default(),
            active: Default::default(),
            foreground: Default::default(),
            claim_gates: Default::default(),
            share: crate::command::commands::api_server::push_job::worker_share(core.executor()),
            hub: core.terminal_hub().clone(),
            accepting: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            claim_stop: Mutex::new(CancellationToken::new()),
            core_shutdown: core.shutdown(),
            helper,
        }))
    }
    fn settings(&self) -> WorkerSettings {
        live_daemon_settings(self.shared.clone())().worker
    }
    fn data_paths(&self) -> crate::data::fs::DataPaths {
        self.shared.paths.data_paths()
    }
    fn support(&self) -> LeaseRuntimeSupport {
        LeaseRuntimeSupport::from_capabilities(self.shared.engines.runtime.capabilities())
    }
    fn running(&self, server: &str) -> usize {
        self.active
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.server == server)
            .count()
    }
    fn map_error(server: &str, error: WorkerEngineError) -> WorkerError {
        match error {
            WorkerEngineError::Taken => WorkerError::Taken,
            WorkerEngineError::Ineligible => {
                WorkerError::Ineligible("server refused affinity".into())
            }
            WorkerEngineError::LinkRevoked => WorkerError::LinkRevoked(server.into()),
            WorkerEngineError::Server { status: 404, .. } => {
                WorkerError::ItemNotFound("item".into())
            }
            other => WorkerError::Upstream(other.to_string()),
        }
    }
    fn worker_labels(
        &self,
        configured: BTreeMap<String, String>,
        interaction: Option<crate::data::config::daemon::Interaction>,
    ) -> Result<awman_wsp::labels::WorkerLabels, WorkerError> {
        let settings = self.settings();
        let interaction = interaction.unwrap_or(settings.pull_interaction);
        let runtime = self.shared.engines.runtime.runtime_name();
        let kind = if runtime.contains("sbx") || runtime.contains("sandbox") {
            RuntimeKind::Sbx
        } else if runtime.contains("apple") {
            RuntimeKind::Apple
        } else if runtime.contains("builtin") {
            RuntimeKind::Builtin
        } else {
            RuntimeKind::Docker
        };
        HostFacts::for_this_build(
            kind,
            WorkerHostKind::Daemon,
            self.agents_with_images(),
            !interaction.is_headless() || settings.pull_allow_override,
            interaction.is_headless() || settings.pull_allow_override,
        )
        .worker_labels(&configured)
        .map_err(|e| WorkerError::Invalid(e.to_string()))
    }
    fn agents_with_images(&self) -> Vec<String> {
        let mut roots = self.shared.workdirs.clone();
        if let Ok(sessions) = self.shared.store.list_sessions() {
            roots.extend(sessions.into_iter().map(|s| s.workdir.into()));
        }
        roots.sort();
        roots.dedup();
        let mut agents = std::collections::BTreeSet::new();
        for root in roots {
            let Ok(session) = crate::data::session::Session::open_at_git_root(
                root.clone(),
                root.clone(),
                crate::data::session::SessionOpenOptions {
                    env: Some(self.shared.sessions.env().clone()),
                    ..Default::default()
                },
            ) else {
                continue;
            };
            let paths = crate::data::RepoDockerfilePaths::new(&root);
            let (available, _) = self
                .shared
                .engines
                .agent_engine
                .filter_available_agents(&session, paths.discover_agent_dockerfiles());
            for (name, _) in available {
                let tag = crate::data::image_tags::agent_image_tag(&root, &name);
                if self
                    .shared
                    .engines
                    .runtime
                    .image_exists(&tag)
                    .unwrap_or(false)
                {
                    agents.insert(name);
                }
            }
        }
        agents.into_iter().collect()
    }
    async fn refresh_servers(&self) -> Result<(), WorkerError> {
        let settings = self.settings();
        let names: Vec<_> = self.servers.read().unwrap().keys().cloned().collect();
        for name in names {
            if !settings.work_servers.contains_key(&name) && self.running(&name) == 0 {
                self.servers.write().unwrap().remove(&name);
            }
        }
        if !settings.pull_enabled {
            return Ok(());
        }
        for (name, config) in &settings.work_servers {
            if !config.enabled {
                continue;
            }
            let Some(url) = &config.url else {
                continue;
            };
            let existing = self
                .servers
                .read()
                .unwrap()
                .get(name)
                .filter(|entry| &entry.url == url)
                .map(|entry| entry.session.clone());
            let session = match existing {
                Some(session) => session,
                None => {
                    // A URL change drains the old session before using new credentials.
                    if self.running(name) > 0 {
                        continue;
                    }
                    // One unusable entry must not stop the other servers (or
                    // the feature) from working, so it is skipped, not fatal.
                    let token = match LinkStore::new(&self.data_paths()).read(name) {
                        Ok(Some(token)) => token,
                        Ok(None) => continue,
                        Err(error) => {
                            tracing::warn!(server = %name, %error, "work server token is unreadable");
                            continue;
                        }
                    };
                    match WspSession::connect(name, url, token.refresh_token) {
                        Ok(session) => Arc::new(session),
                        Err(error) => {
                            tracing::warn!(server = %name, %error, "work server is unusable");
                            continue;
                        }
                    }
                }
            };
            let labels = match self.worker_labels(config.labels.clone(), config.interaction) {
                Ok(labels) => labels,
                Err(error) => {
                    tracing::warn!(server = %name, %error, "work server labels are invalid");
                    continue;
                }
            };
            let mut registration_settings = settings.clone();
            registration_settings.pull_interaction =
                config.interaction.unwrap_or(settings.pull_interaction);
            let request = RegisterRequest {
                name: format!("awman-{name}"),
                labels: labels.merged(),
                capacity: config.max_concurrent_jobs,
                features: capabilities::for_settings(self.support(), &registration_settings)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            };
            {
                let mut servers = self.servers.write().unwrap();
                if let Some(entry) = servers.get_mut(name) {
                    if entry.url == *url
                        && entry.registration == request
                        && entry.status.state == "registered"
                    {
                        entry.status.claim = config.claim;
                        entry.status.enabled = config.enabled;
                        entry.status.max_concurrent_jobs = config.max_concurrent_jobs;
                        continue;
                    }
                }
            }
            // Server labels are server-controlled input: a bad set marks this
            // server as failed instead of failing every server.
            let registration = match session.register(&request).await {
                Ok(registration) => labels
                    .clone()
                    .with_server(registration.server_labels.clone())
                    .map(|_| registration)
                    .map_err(|e| format!("server labels refused: {e}")),
                Err(error) => Err(error.to_string()),
            };
            match registration {
                Ok(registration) => {
                    let last_claim_at = self
                        .servers
                        .read()
                        .unwrap()
                        .get(name)
                        .and_then(|entry| entry.status.last_claim_at);
                    self.servers.write().unwrap().insert(
                        name.clone(),
                        Registered {
                            session,
                            registration: request,
                            url: url.clone(),
                            heartbeat: crate::engine::worker::heartbeat::heartbeat_interval(Some(
                                registration.heartbeat_ms,
                            )),
                            status: ServerStatus {
                                name: name.clone(),
                                state: "registered".into(),
                                enabled: true,
                                claim: config.claim,
                                max_concurrent_jobs: config.max_concurrent_jobs,
                                running: self.running(name),
                                queued_eligible: 0,
                                labels: labels.merged(),
                                server_labels: registration.server_labels,
                                last_claim_at,
                                error: None,
                            },
                        },
                    );
                }
                Err(error) => {
                    let state = if session.is_revoked() {
                        "revoked"
                    } else {
                        "error"
                    };
                    let running = self.running(name);
                    // Also record a first failure, so `/v1/worker/servers`
                    // shows why the server is not registered.
                    let mut servers = self.servers.write().unwrap();
                    let entry = servers.entry(name.clone()).or_insert_with(|| Registered {
                        session: session.clone(),
                        registration: request,
                        url: url.clone(),
                        heartbeat: crate::engine::worker::heartbeat::heartbeat_interval(None),
                        status: ServerStatus {
                            name: name.clone(),
                            state: state.into(),
                            enabled: true,
                            claim: config.claim,
                            max_concurrent_jobs: config.max_concurrent_jobs,
                            running,
                            queued_eligible: 0,
                            labels: labels.merged(),
                            server_labels: Default::default(),
                            last_claim_at: None,
                            error: None,
                        },
                    });
                    entry.status.state = state.into();
                    entry.status.error = Some(error);
                    drop(servers);
                    if session.is_revoked() {
                        for active in self
                            .active
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|a| &a.server == name)
                        {
                            active.control.fence();
                        }
                    }
                }
            }
        }
        Ok(())
    }
    /// Recovery precedes the push pool's generic stale-row recovery. A pull
    /// row must never be re-queued and executed without its original lease.
    pub async fn recover(&self) -> Result<(), WorkerError> {
        // A feature restart first drains tasks from its prior generation;
        // persisted recovery must not race a still-running lease owner.
        self.stop_now().await;
        self.refresh_servers().await?;
        for mut record in self.leases.active()? {
            let Some(_owner) =
                crate::data::worker::paths::try_lease_owner(self.data_paths().root(), &record.id)?
            else {
                continue;
            };
            if let Some(job) = &record.job_id {
                crate::command::commands::api_server::owned_runtime::stop_command_containers(
                    self.shared.engines.runtime.as_ref(),
                    job,
                )
                .map_err(|e| WorkerError::Upstream(e.to_string()))?;
                self.shared.store.complete_command(
                    job,
                    crate::data::command_status::CommandStatus::Cancelled,
                    Some(130),
                    None,
                )?;
            }
            let session = self
                .servers
                .read()
                .unwrap()
                .get(&record.server)
                .map(|entry| entry.session.clone());
            record.state = LeaseState::Lost;
            if record.expires_at > Utc::now() {
                if let Some(session) = session {
                    let reference = LeaseRef {
                        server_lease_id: record.server_lease_id.clone(),
                        epoch: record.epoch,
                    };
                    if session
                        .release(
                            &reference,
                            &ReleaseRequest {
                                reason: ReleaseReason::Shutdown,
                                message: Some("worker restarted".into()),
                                result: None,
                            },
                        )
                        .await
                        .is_ok()
                    {
                        record.state = LeaseState::Released;
                        record.release = Some(LeaseRelease {
                            reason: ReleaseReason::Shutdown,
                            message: Some("worker restarted".into()),
                        });
                    }
                }
            }
            record.ended_at = Some(Utc::now());
            self.leases.update(&record)?;
        }
        let root = crate::data::worker::paths::work_root(&self.data_paths(), &self.settings());
        crate::engine::worker::lease_workspace::remove_orphaned_lease_dirs(&root, &[]);
        self.cleanup_retired()?;
        Ok(())
    }
    fn cleanup_retired(&self) -> Result<(), WorkerError> {
        let links = LinkStore::new(&self.data_paths());
        for name in links.retired()? {
            if !self.settings().work_servers.contains_key(&name)
                && self.leases.active_for_server(&name)?.is_empty()
            {
                links.remove(&name)?;
            }
        }
        Ok(())
    }
    pub async fn run_until(
        self: Arc<Self>,
        stop: CancellationToken,
    ) -> Result<(), crate::engine::error::EngineError> {
        // A child token: a `shutdown` release stops claiming without
        // cancelling the whole worker feature (push included).
        *self.claim_stop.lock().unwrap() = stop.child_token();
        self.accepting.store(true, Ordering::SeqCst);
        self.shutting_down.store(false, Ordering::SeqCst);
        let mut claims = BTreeMap::<String, tokio::task::JoinHandle<()>>::new();
        while !stop.is_cancelled() {
            if let Err(error) = self.refresh_servers().await {
                tracing::warn!(%error, "work server registration failed");
            }
            if let Err(error) = self.cleanup_retired() {
                tracing::warn!(%error, "retired work server cleanup failed");
            }
            let settings = self.settings();
            for (name, config) in &settings.work_servers {
                if settings.pull_enabled
                    && config.enabled
                    && config.claim == ClaimMode::Auto
                    && self.running(name) < config.max_concurrent_jobs
                    && claims.get(name).is_none_or(|task| task.is_finished())
                {
                    let this = self.clone();
                    let task_name = name.clone();
                    claims.insert(
                        name.clone(),
                        tokio::spawn(async move {
                            if let Err(error) = this.claim_next(None, &task_name).await {
                                tracing::debug!(%error, "automatic work claim deferred");
                            }
                        }),
                    );
                }
            }
            // Refresh counts even when all execution slots are occupied.
            // The queue endpoint updates the same status snapshot.
            for name in settings.work_servers.keys() {
                let session = self
                    .servers
                    .read()
                    .unwrap()
                    .get(name)
                    .filter(|e| e.status.state == "registered")
                    .map(|e| e.session.clone());
                if let Some(session) = session {
                    let refresh = async {
                        let mut count = 0;
                        let mut cursor = None;
                        loop {
                            let page = session.queue(500, cursor.as_deref()).await?;
                            count += page.items.len();
                            cursor = page.next_cursor;
                            if cursor.is_none() {
                                break;
                            }
                        }
                        Ok::<_, WorkerEngineError>(count)
                    };
                    let count = tokio::select! { biased; _ = stop.cancelled() => break, count = refresh => count };
                    if let Ok(count) = count {
                        if let Some(entry) = self.servers.write().unwrap().get_mut(name) {
                            entry.status.queued_eligible = count;
                        }
                    }
                }
            }
            tokio::select! { biased; _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
        }
        self.accepting.store(false, Ordering::SeqCst);
        for task in claims.into_values() {
            let _ = task.await;
        }
        // A drain preserves active leases. Only stop_now releases shutdown.
        loop {
            let pending: Vec<_> = self
                .active
                .lock()
                .unwrap()
                .values()
                .map(|a| a.done.clone())
                .collect();
            if pending.is_empty() {
                break;
            }
            for done in pending {
                done.cancelled().await;
            }
        }
        Ok(())
    }
    pub async fn stop_now(&self) {
        self.accepting.store(false, Ordering::SeqCst);
        self.claim_stop.lock().unwrap().cancel();
        self.cancel_for_shutdown();
        let runs = std::mem::take(&mut *self.foreground.lock().await);
        for (id, (mut run, _permit)) in runs {
            let _ = run
                .release(ReleaseReason::Shutdown, "daemon stopped immediately")
                .await;
            run.active.done.cancel();
            self.active.lock().unwrap().remove(&id);
        }
        loop {
            let pending: Vec<_> = self
                .active
                .lock()
                .unwrap()
                .values()
                .map(|active| active.done.clone())
                .collect();
            if pending.is_empty() {
                break;
            }
            for done in pending {
                done.cancelled().await;
            }
        }
    }
    pub fn cancel_for_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for active in self.active.lock().unwrap().values() {
            let mut finish = active.finish.lock().unwrap();
            if !finish.reporting {
                finish
                    .release
                    .get_or_insert((ReleaseReason::Shutdown, None));
            }
            active.control.cancelled().cancel();
        }
    }
    async fn claim_next(
        self: &Arc<Self>,
        item: Option<&str>,
        name: &str,
    ) -> Result<Option<LeaseRecord>, WorkerError> {
        self.claim_next_for(item, name, false).await
    }
    async fn claim_next_for(
        self: &Arc<Self>,
        item: Option<&str>,
        name: &str,
        foreground: bool,
    ) -> Result<Option<LeaseRecord>, WorkerError> {
        let stop = self.claim_stop.lock().unwrap().clone();
        if !self.accepting.load(Ordering::SeqCst) || stop.is_cancelled() {
            return Err(WorkerError::PullDisabled("worker is draining".into()));
        }
        let settings = self.settings();
        let config = settings
            .work_servers
            .get(name)
            .ok_or_else(|| WorkerError::UnknownServer(name.into()))?;
        if !settings.pull_enabled || !config.enabled {
            return Err(WorkerError::PullDisabled("pull is disabled".into()));
        }
        if self.running(name) >= config.max_concurrent_jobs {
            return Err(WorkerError::Ineligible(
                "work server capacity is full".into(),
            ));
        }
        let permit: WorkerPermit = tokio::select! { biased; _ = stop.cancelled() => return Err(WorkerError::PullDisabled("worker is draining".into())), permit = self.share.acquire(JobClass::Pull) => permit };
        // Live policy must still permit the claim after waiting for capacity.
        if !self
            .settings()
            .work_servers
            .get(name)
            .is_some_and(|c| c.enabled && (item.is_some() || c.claim == ClaimMode::Auto))
            || !self.settings().pull_enabled
        {
            return Err(WorkerError::PullDisabled(
                "work server retired while waiting".into(),
            ));
        }
        let (session, heartbeat) = self
            .servers
            .read()
            .unwrap()
            .get(name)
            .filter(|entry| {
                entry.status.state == "registered" && config.url.as_ref() == Some(&entry.url)
            })
            .map(|entry| (entry.session.clone(), entry.heartbeat))
            .ok_or_else(|| WorkerError::ServerNotRegistered(name.into()))?;
        // Never hold a claim gate across executor acquisition. Gates are per
        // server, preserving its start order without blocking other servers.
        let gate = self
            .claim_gates
            .lock()
            .unwrap()
            .entry(name.into())
            .or_default()
            .clone();
        let _gate = tokio::select! { biased; _ = stop.cancelled() => return Err(WorkerError::PullDisabled("worker is draining".into())), gate = gate.lock() => gate };
        if self.running(name) >= config.max_concurrent_jobs {
            return Err(WorkerError::Ineligible(
                "work server capacity is full".into(),
            ));
        }
        let server_labels = self
            .servers
            .read()
            .unwrap()
            .get(name)
            .map(|entry| entry.status.server_labels.clone())
            .unwrap_or_default();
        // Admission was possible when this poll began. An idle long-poll
        // must not reserve an execution slot: pushed work and other servers
        // can use it. A returned lease heartbeats while reacquiring its turn.
        let permit = if item.is_none() {
            drop(permit);
            None
        } else {
            Some(permit)
        };
        let claim = async {
            loop {
                match session
                    .claim(item, if item.is_none() { 30_000 } else { 0 })
                    .await
                {
                    Err(WorkerEngineError::Taken) if item.is_none() => continue,
                    outcome => return outcome.map_err(|e| Self::map_error(name, e)),
                }
            }
        };
        let policy_changed = async {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let latest = self.settings();
                if !latest.pull_enabled
                    || !latest.work_servers.get(name).is_some_and(|entry| {
                        entry.enabled
                            && entry.url == config.url
                            && (item.is_some() || entry.claim == ClaimMode::Auto)
                    })
                {
                    break;
                }
            }
        };
        let outcome = tokio::select! { biased;
            _ = stop.cancelled() => return Err(WorkerError::PullDisabled("worker is draining".into())),
            _ = policy_changed => return Err(WorkerError::PullDisabled("work server policy changed".into())),
            outcome = claim => outcome?,
        };
        let ClaimOutcome::Leased(lease) = outcome else {
            return Ok(None);
        };
        if stop.is_cancelled()
            || !self.accepting.load(Ordering::SeqCst)
            || !self.settings().pull_enabled
            || !self.settings().work_servers.get(name).is_some_and(|entry| {
                entry.enabled
                    && entry.url == config.url
                    && (item.is_some() || entry.claim == ClaimMode::Auto)
            })
        {
            if lease.expires_at > Utc::now() {
                session
                    .release(
                        &LeaseRef::of(&lease),
                        &ReleaseRequest {
                            reason: if self.core_shutdown.is_cancelled()
                                || self.shutting_down.load(Ordering::SeqCst)
                            {
                                ReleaseReason::Shutdown
                            } else {
                                ReleaseReason::Ineligible
                            },
                            message: Some("worker policy changed during claim".into()),
                            result: None,
                        },
                    )
                    .await
                    .map_err(|error| Self::map_error(name, error))?;
            }
            return Err(WorkerError::PullDisabled(
                "worker policy changed during claim".into(),
            ));
        }
        let latest = self.settings();
        let plan = latest
            .work_servers
            .get(name)
            .ok_or_else(|| "work server retired during claim".to_string())
            .and_then(|server| {
                if self.running(name) >= server.max_concurrent_jobs {
                    return Err("work server capacity changed during claim".into());
                }
                let labels = self
                    .worker_labels(server.labels.clone(), server.interaction)
                    .map_err(|error| error.to_string())?
                    .with_server(server_labels)
                    .map_err(|error| error.to_string())?
                    .merged();
                policy::plan(
                    &lease.item,
                    &labels,
                    server,
                    &latest,
                    &self.support(),
                    self.shared.engines.runtime.display_name(),
                )
                .and_then(|plan| policy::delivery(&lease, &plan, true).map(|()| plan))
                .and_then(|plan| {
                    if !foreground && lease.item.exec.dynamic && !lease.item.exec.yolo && !self.factory.supports_workflow_dialogs() {
                        Err("exec.yolo=false requires a foreground interactive host with workflow dialogs; this daemon frontend has no dialog response channel".into())
                    } else { Ok(plan) }
                })
            });
        let plan = match plan {
            Ok(plan) if lease.expires_at > Utc::now() => plan,
            other => {
                let reason = other
                    .err()
                    .unwrap_or_else(|| "lease already expired".into());
                if lease.expires_at > Utc::now() {
                    session
                        .release(
                            &LeaseRef::of(&lease),
                            &ReleaseRequest {
                                reason: ReleaseReason::Ineligible,
                                message: Some(reason.clone()),
                                result: None,
                            },
                        )
                        .await
                        .map_err(|e| Self::map_error(name, e))?;
                }
                return Err(WorkerError::Ineligible(reason));
            }
        };
        if foreground {
            let root = self.data_paths().home().to_path_buf();
            let invoker = crate::data::session::Session::open_at_git_root(
                root.clone(),
                root,
                crate::data::session::SessionOpenOptions {
                    env: Some(self.shared.sessions.env().clone()),
                    ..Default::default()
                },
            )
            .map_err(WorkerError::Store)?;
            let preparing_id = Mutex::new(None);
            let run = tui_host::ManualRun::prepare(
                &invoker,
                &self.shared.engines,
                session,
                tui_host::ManualClaim {
                    server: name,
                    lease,
                    plan,
                    interval: heartbeat,
                },
                &|record, active| {
                    *preparing_id.lock().unwrap() = Some(record.id.to_string());
                    self.active
                        .lock()
                        .unwrap()
                        .insert(record.id.to_string(), active);
                },
            )
            .await;
            let run = match run {
                Ok(run) => run,
                Err(error) => {
                    if let Some(id) = preparing_id.into_inner().unwrap() {
                        self.active.lock().unwrap().remove(&id);
                    }
                    return Err(WorkerError::Upstream(error));
                }
            };
            let mut run = run;
            let settings = live_daemon_settings(self.shared.clone());
            run.start_terminal(Arc::new(move || settings().terminal.remote_input));
            let record = run.record.clone();
            *run.active.foreground_seen.lock().unwrap() = Some(std::time::Instant::now());
            let owner = self.clone();
            let watched = run.active.clone();
            let watched_record = record.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = watched.done.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                    }
                    let stale = watched
                        .foreground_seen
                        .lock()
                        .unwrap()
                        .is_some_and(|seen| seen.elapsed() >= Duration::from_secs(30));
                    if stale {
                        let _ = PullHandleAdapter(owner)
                            .release(
                                &watched_record,
                                ReleaseReason::Error,
                                Some("foreground execution owner disconnected".into()),
                            )
                            .await;
                        return;
                    }
                }
            });
            let job_cancel = self
                .shared
                .command_cancels
                .register(record.job_id.as_deref().expect("prepared job"));
            let active = run.active.clone();
            let mut cancellation_record = record.clone();
            let leases = self.leases.clone();
            let shared = self.shared.clone();
            let shutdown = self.core_shutdown.clone();
            tokio::spawn(async move {
                let control_cancel = active.control.cancelled();
                tokio::select! {
                    _ = active.done.cancelled() => {},
                    _ = job_cancel.cancelled() => {
                        active.finish.lock().unwrap().release.get_or_insert((
                            if shutdown.is_cancelled() { ReleaseReason::Shutdown } else { ReleaseReason::Error },
                            Some("worker job cancelled by its owner".into()),
                        ));
                        active.control.cancelled().cancel();
                    },
                    _ = control_cancel.cancelled() => {},
                }
                if !active.done.is_cancelled() {
                    cancellation_record.state = if active.control.may_report() {
                        LeaseState::Reporting
                    } else {
                        LeaseState::Lost
                    };
                    let _ = leases.update(&cancellation_record);
                    active.done.cancelled().await;
                }
                if let Some(job_id) = &cancellation_record.job_id {
                    shared.command_cancels.unregister(job_id);
                }
            });
            self.active
                .lock()
                .unwrap()
                .insert(record.id.to_string(), run.active.clone());
            self.foreground.lock().await.insert(
                record.id.to_string(),
                (run, permit.expect("manual claim permit")),
            );
            return Ok(Some(record));
        }
        let record = LeaseRecord::claimed(name, &lease, LeaseHost::Daemon, Utc::now());
        let owner =
            crate::data::worker::paths::try_lease_owner(self.data_paths().root(), &record.id)?
                .ok_or_else(|| WorkerError::Invalid("lease already owned".into()))?;
        if let Err(error) = self.leases.insert(&record) {
            let _ = session
                .release(
                    &LeaseRef::of(&lease),
                    &ReleaseRequest {
                        reason: ReleaseReason::Error,
                        message: Some("could not persist lease".into()),
                        result: None,
                    },
                )
                .await;
            return Err(error.into());
        }
        self.start_lease(
            session,
            runner::ClaimedLease {
                lease,
                record: record.clone(),
                plan,
                owner,
            },
            permit,
            heartbeat,
        );
        if let Some(entry) = self.servers.write().unwrap().get_mut(name) {
            entry.status.last_claim_at = Some(Utc::now());
        }
        Ok(Some(record))
    }
}

// The trait is implemented on a light Arc wrapper so a manual claim can
// transfer owned controller state into a supervised lease task.
pub struct PullHandleAdapter<F: ApiCommandFrontendFactory + 'static>(pub Arc<PullController<F>>);
#[async_trait::async_trait]
impl<F: ApiCommandFrontendFactory + 'static> PullHandle for PullHandleAdapter<F> {
    fn touch_foreground(&self, id: &crate::data::ids::LeaseId) {
        if let Some(active) = self.0.active.lock().unwrap().get(id.as_str()) {
            let mut seen = active.foreground_seen.lock().unwrap();
            if seen.is_some() {
                *seen = Some(std::time::Instant::now());
            }
        }
    }
    fn cancel_jobs(&self) {
        for active in self.0.active.lock().unwrap().values() {
            let mut finish = active.finish.lock().unwrap();
            if !finish.reporting {
                finish.release.get_or_insert((
                    awman_wsp::ReleaseReason::Error,
                    Some("worker jobs cancelled by their owner".into()),
                ));
                active.control.cancelled().cancel();
            }
        }
    }
    fn servers(&self) -> Vec<ServerStatus> {
        let settings = self.0.settings();
        let mut result = Vec::new();
        for (name, config) in &settings.work_servers {
            let mut status = self
                .0
                .servers
                .read()
                .unwrap()
                .get(name)
                .map(|entry| entry.status.clone())
                .unwrap_or_else(|| ServerStatus {
                    name: name.clone(),
                    state: "disconnected".into(),
                    enabled: config.enabled,
                    claim: config.claim,
                    max_concurrent_jobs: config.max_concurrent_jobs,
                    running: 0,
                    queued_eligible: 0,
                    labels: config.labels.clone(),
                    server_labels: Default::default(),
                    last_claim_at: None,
                    error: None,
                });
            status.running = self.0.running(name);
            if !config.enabled || !settings.pull_enabled {
                status.state = if status.running > 0 {
                    "retiring"
                } else {
                    "disabled"
                }
                .into();
            }
            result.push(status);
        }
        for (name, entry) in self.0.servers.read().unwrap().iter() {
            if !settings.work_servers.contains_key(name) {
                let running = self.0.running(name);
                if running > 0 {
                    let mut status = entry.status.clone();
                    status.state = "retiring".into();
                    status.enabled = false;
                    status.running = running;
                    result.push(status);
                }
            }
        }
        result
    }
    async fn queue(
        &self,
        server: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<List<QueueRow>, WorkerError> {
        let settings = self.0.settings();
        let names: Vec<_> = match server {
            Some(name)
                if settings
                    .work_servers
                    .get(name)
                    .is_some_and(|entry| !entry.enabled) =>
            {
                return Err(WorkerError::PullDisabled(format!(
                    "work server {name:?} is disabled"
                )));
            }
            Some(name) => vec![name.to_string()],
            None => settings
                .work_servers
                .iter()
                .filter(|(_, entry)| entry.enabled)
                .map(|(name, _)| name.clone())
                .collect(),
        };
        let mut rows = Vec::new();
        for name in names {
            let session = self
                .0
                .servers
                .read()
                .unwrap()
                .get(&name)
                .filter(|entry| entry.status.state == "registered")
                .map(|entry| entry.session.clone())
                .ok_or_else(|| WorkerError::ServerNotRegistered(name.clone()))?;
            let mut next = None;
            loop {
                let page = session
                    .queue(500, next.as_deref())
                    .await
                    .map_err(|e| PullController::<F>::map_error(&name, e))?;
                rows.extend(
                    page.items
                        .into_iter()
                        .map(|item| QueueRow::from_wsp(&name, item, Utc::now())),
                );
                next = page.next_cursor;
                if next.is_none() {
                    break;
                }
            }
            if let Some(entry) = self.0.servers.write().unwrap().get_mut(&name) {
                entry.status.queued_eligible = rows.iter().filter(|r| r.server == name).count();
            }
        }
        rows.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then(a.enqueued_at.cmp(&b.enqueued_at))
                .then(a.server.cmp(&b.server))
                .then(a.id.cmp(&b.id))
        });
        Ok(crate::command::daemon_api::core::page(
            rows,
            &ListQuery {
                limit: Some(limit),
                cursor: cursor.map(str::to_string),
            },
        )?)
    }
    async fn claim(&self, item_id: &str, server: Option<&str>) -> Result<LeaseRecord, WorkerError> {
        let name = match server {
            Some(name) => name.to_owned(),
            None => {
                let settings = self.0.settings();
                if settings.work_servers.len() != 1 {
                    return Err(WorkerError::Invalid("select a work server".into()));
                }
                settings.work_servers.keys().next().unwrap().clone()
            }
        };
        self.0
            .claim_next(Some(item_id), &name)
            .await?
            .ok_or_else(|| WorkerError::ItemNotFound(item_id.into()))
    }
    async fn release(
        &self,
        lease: &LeaseRecord,
        reason: ReleaseReason,
        message: Option<String>,
    ) -> Result<LeaseRecord, WorkerError> {
        if reason == ReleaseReason::Shutdown {
            self.0.shutting_down.store(true, Ordering::SeqCst);
            self.0.accepting.store(false, Ordering::SeqCst);
            self.0.claim_stop.lock().unwrap().cancel();
        }
        let running_foreground = {
            let runs = self.0.foreground.lock().await;
            runs.get(lease.id.as_str())
                .filter(|(run, _)| run.active.started.load(Ordering::SeqCst))
                .map(|(run, _)| run.active.clone())
        };
        if let Some(active) = running_foreground {
            active.finish.lock().unwrap().release = Some((reason, message.clone()));
            active.control.cancelled().cancel();
            if tokio::time::timeout(Duration::from_secs(15), active.done.cancelled())
                .await
                .is_ok()
            {
                return self
                    .0
                    .leases
                    .get(&lease.id)?
                    .ok_or(WorkerError::LeaseNotFound);
            }
            // A disconnected foreground owner cannot hold release forever.
            // The fallback below stops its command containers before reporting.
        }
        if let Some((mut run, _permit)) = self.0.foreground.lock().await.remove(lease.id.as_str()) {
            run.active.control.cancelled().cancel();
            let released = run
                .release(
                    reason,
                    message.as_deref().unwrap_or("lease released by its owner"),
                )
                .await;
            run.active.done.cancel();
            self.0.active.lock().unwrap().remove(lease.id.as_str());
            released.map_err(WorkerError::Upstream)?;
            return Ok(run.record.clone());
        }
        let active = self
            .0
            .active
            .lock()
            .unwrap()
            .get(lease.id.as_str())
            .cloned()
            .ok_or_else(|| WorkerError::LeaseEnded(lease.id.to_string()))?;
        {
            let mut finish = active.finish.lock().unwrap();
            if finish.reporting {
                return Err(WorkerError::LeaseEnded(lease.id.to_string()));
            }
            finish.release.get_or_insert((reason, message));
            active.control.cancelled().cancel();
        }
        active.done.cancelled().await;
        self.0
            .leases
            .get(&lease.id)?
            .ok_or(WorkerError::LeaseNotFound)
    }
    fn capabilities(&self) -> Vec<&'static str> {
        let mut settings = self.0.settings();
        if !self.0.accepting.load(Ordering::SeqCst) || !settings.pull_enabled {
            return Vec::new();
        }
        if settings.work_servers.values().any(|server| {
            server.enabled && server.interaction.is_some_and(|mode| !mode.is_headless())
        }) {
            settings.pull_interaction = crate::data::config::daemon::Interaction::Interactive;
        }
        let mut caps = capabilities::for_settings(self.0.support(), &settings);
        caps.push("worker_pull");
        caps
    }
    async fn prepare_foreground(
        &self,
        item: &str,
        server: Option<&str>,
    ) -> Result<tui_host::ForegroundPick, WorkerError> {
        let settings = self.0.settings();
        let name = match server {
            Some(name) => name,
            None if settings.work_servers.len() == 1 => {
                settings.work_servers.keys().next().unwrap()
            }
            None => return Err(WorkerError::Invalid("select a work server".into())),
        };
        let record = self
            .0
            .claim_next_for(Some(item), name, true)
            .await?
            .ok_or_else(|| WorkerError::ItemNotFound(item.into()))?;
        let runs = self.0.foreground.lock().await;
        Ok(runs
            .get(record.id.as_str())
            .ok_or(WorkerError::LeaseNotFound)?
            .0
            .handoff())
    }
    async fn foreground_events(
        &self,
        id: &crate::data::ids::LeaseId,
        events: Vec<crate::data::execution_event::ExecutionEvent>,
    ) -> Result<(), WorkerError> {
        let runs = self.0.foreground.lock().await;
        let (run, _) = runs.get(id.as_str()).ok_or(WorkerError::LeaseNotFound)?;
        run.publish(events)
    }
    async fn foreground_terminal(
        &self,
        id: &crate::data::ids::LeaseId,
        frames: Vec<awman_wsp::terminal::LeaseTerminalFrame>,
    ) -> Result<Vec<awman_wsp::terminal::LeaseTerminalFrame>, WorkerError> {
        let bridge = {
            let runs = self.0.foreground.lock().await;
            let (run, _) = runs.get(id.as_str()).ok_or(WorkerError::LeaseNotFound)?;
            if !run.active.control.may_report() {
                return Err(WorkerError::LeaseEnded(id.to_string()));
            }
            run.terminal
                .clone()
                .ok_or_else(|| WorkerError::Invalid("lease has no terminal".into()))?
        };
        bridge.exchange(frames).await.map_err(WorkerError::Upstream)
    }
    async fn foreground_checkout(
        &self,
        id: &crate::data::ids::LeaseId,
        workdir: std::path::PathBuf,
    ) -> Result<tui_host::ForegroundPick, WorkerError> {
        let invoker = crate::data::session::Session::open_at_git_root(
            workdir.clone(),
            workdir,
            crate::data::session::SessionOpenOptions {
                env: Some(self.0.shared.sessions.env().clone()),
                ..Default::default()
            },
        )?;
        let mut runs = self.0.foreground.lock().await;
        let (run, _) = runs
            .get_mut(id.as_str())
            .ok_or(WorkerError::LeaseNotFound)?;
        run.rehome(&invoker, &self.0.shared.engines)
            .await
            .map_err(WorkerError::Invalid)?;
        Ok(run.handoff())
    }
    async fn foreground_result(
        &self,
        id: &crate::data::ids::LeaseId,
        result: crate::data::worker::wire::ForegroundResult,
    ) -> Result<crate::data::fs::api_db::CommandResult, WorkerError> {
        let (mut run, _permit) = self
            .0
            .foreground
            .lock()
            .await
            .remove(id.as_str())
            .ok_or(WorkerError::LeaseNotFound)?;
        if let Some(reason) = result.release_reason {
            run.active.finish.lock().unwrap().release =
                Some((reason, Some("frontend released the work item".into())));
            run.active.control.cancelled().cancel();
        }
        run.record_outputs_mounted(result.completion.outputs_mounted);
        let result = run
            .complete(&self.0.shared.engines, result.completion.result)
            .await;
        run.active.done.cancel();
        self.0.active.lock().unwrap().remove(id.as_str());
        result.map_err(WorkerError::Upstream)
    }
}
