//! Lifetime-scoped manual workers for TUI and foreground CLI hosts.
//!
//! A small actor owns network futures and lease guards. The synchronous panel
//! trait never enters a caller's Tokio runtime or performs worker policy.
use super::queue_source::*;
use super::*;
use crate::command::{commands::api_server::event_bus::EventBus, dispatch::Engines};
use crate::data::{
    fs::api_db::{CommandResult, SqliteSessionStore},
    session::{Session, SessionOpenOptions, SessionType},
    worker::{job_store::JobStore, JobSpec, JobWorkspace},
};
use crate::engine::{
    git::lease_transport::LeaseTransport,
    worker::{
        heartbeat::{HeartbeatLoop, HeartbeatStatus},
        lease_workspace::{InlineWorkItem, LeaseCheckout, LeaseWorkspace},
    },
};
use awman_wsp::{HeartbeatState, Lease};
use std::path::PathBuf;
use tokio::sync::mpsc;

type Reply<T> = std::sync::mpsc::SyncSender<Result<T, String>>;
enum Request {
    Begin(String, Reply<()>),
    Refresh,
    Pick(String, String, Reply<PickRun>),
    Finish(String, LeaseCloseChoice, Reply<()>),
    Complete(String, Box<CommandResult>, Reply<CommandResult>),
    Checkout(String, CheckoutChoice, Reply<PickRun>),
}
#[derive(Default)]
struct View {
    terminal_input: BTreeMap<String, bool>,
    executing: std::collections::BTreeSet<String>,
    closing: BTreeMap<String, LeaseCloseChoice>,
    picks: BTreeMap<String, PickRun>,
    snapshot: QueueSnapshot,
    generation: u64,
    sessions: BTreeMap<String, Session>,
    events: BTreeMap<String, Arc<EventBus>>,
    cancels: BTreeMap<String, CancellationToken>,
    engines: BTreeMap<String, Engines>,
    writers: BTreeMap<String, crate::data::fs::api_command_log::CommandStateWriter>,
    offers: BTreeMap<String, crate::data::prompt::Prompt<CheckoutChoice>>,
}
struct Source {
    tx: mpsc::UnboundedSender<Request>,
    view: Arc<Mutex<View>>,
}
impl Source {
    fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(make(tx))
            .map_err(|_| "manual worker stopped".to_string())?;
        rx.recv().map_err(|_| "manual worker stopped".to_string())?
    }
}
impl QueueSource for Source {
    fn terminal_input_for_lease(&self, id: &str) -> bool {
        self.view
            .lock()
            .unwrap()
            .terminal_input
            .get(id)
            .copied()
            .unwrap_or(false)
    }
    fn begin_lease(&self, id: &str) -> Result<(), String> {
        self.call(|reply| Request::Begin(id.into(), reply))
    }
    fn prepared_run(&self, id: &str) -> Option<PickRun> {
        self.view.lock().unwrap().picks.get(id).cloned()
    }
    fn snapshot(&self) -> QueueSnapshot {
        self.view.lock().unwrap().snapshot.clone()
    }
    fn generation(&self) -> u64 {
        self.view.lock().unwrap().generation
    }
    fn mark_seen(&self) {
        self.view.lock().unwrap().snapshot.new_since_seen = 0;
    }
    fn refresh(&self) {
        let _ = self.tx.send(Request::Refresh);
    }
    fn pick(&self, server: &str, item_id: &str) -> Result<PickRun, PickError> {
        let picked = self
            .call(|reply| Request::Pick(server.into(), item_id.into(), reply))
            .map_err(|e| {
                if e == "taken" {
                    PickError::Taken
                } else if e.starts_with("ineligible") {
                    PickError::Ineligible(e)
                } else {
                    PickError::Failed(e)
                }
            })?;
        self.view
            .lock()
            .unwrap()
            .picks
            .insert(picked.lease_id.clone(), picked.clone());
        Ok(picked)
    }
    fn finish_lease(&self, id: &str, choice: LeaseCloseChoice) -> Result<(), String> {
        self.call(|reply| Request::Finish(id.into(), choice, reply))
    }
    fn checkout_offer(&self, id: &str) -> Option<crate::data::prompt::Prompt<CheckoutChoice>> {
        self.view.lock().unwrap().offers.get(id).cloned()
    }
    fn choose_checkout(&self, id: &str, choice: CheckoutChoice) -> Result<PickRun, PickError> {
        let picked = self
            .call(|reply| Request::Checkout(id.into(), choice, reply))
            .map_err(PickError::Failed)?;
        self.view
            .lock()
            .unwrap()
            .picks
            .insert(picked.lease_id.clone(), picked.clone());
        Ok(picked)
    }
    fn session_for_lease(&self, id: &str) -> Option<Session> {
        self.view.lock().unwrap().sessions.get(id).cloned()
    }
    fn events_for_lease(&self, id: &str) -> Option<Arc<EventBus>> {
        self.view.lock().unwrap().events.get(id).cloned()
    }
    fn cancellation_for_lease(&self, id: &str) -> Option<CancellationToken> {
        self.view.lock().unwrap().cancels.get(id).cloned()
    }
    fn engines_for_lease(&self, id: &str) -> Option<Engines> {
        self.view.lock().unwrap().engines.get(id).cloned()
    }
    fn writer_for_lease(
        &self,
        id: &str,
    ) -> Option<crate::data::fs::api_command_log::CommandStateWriter> {
        self.view.lock().unwrap().writers.get(id).cloned()
    }
    fn output_dir_for_lease(&self, id: &str) -> Option<PathBuf> {
        self.view
            .lock()
            .unwrap()
            .writers
            .get(id)
            .map(|writer| writer.output_dir())
    }
    fn complete_lease(&self, id: &str, result: CommandResult) -> Result<CommandResult, String> {
        self.call(|reply| Request::Complete(id.into(), Box::new(result), reply))
    }
}

pub struct TuiWorkerController;
/// Local frontend handoff; all scheduling and command models remain Layer 0.
pub struct ForegroundPick {
    pub lease: crate::data::worker::wire::LeaseResource,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub inline_source: PathBuf,
    pub output_dir: PathBuf,
    pub repo_url: String,
    pub branch: String,
    pub read_only: bool,
}
impl TuiWorkerController {
    pub fn start(
        session: &Session,
        engines: &Engines,
    ) -> Result<Option<SharedQueueSource>, WorkerError> {
        if !session.effective_config().tui_worker_enabled() {
            return Ok(None);
        }
        Self::start_manual(session, engines).map(Some)
    }
    pub fn start_manual(
        session: &Session,
        engines: &Engines,
    ) -> Result<SharedQueueSource, WorkerError> {
        let session = session.clone();
        let engines = engines.clone();
        let view = Arc::new(Mutex::new(View::default()));
        let actor_view = view.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("awman-manual-worker".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                let runtime = match runtime {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                runtime.block_on(async move {
                    match Host::connect(session, engines, actor_view).await {
                        Ok(mut host) => {
                            let _ = host.refresh().await;
                            let _ = ready_tx.send(Ok(()));
                            host.run(rx).await;
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                });
            })
            .map_err(|e| WorkerError::Upstream(e.to_string()))?;
        ready_rx
            .recv()
            .map_err(|_| WorkerError::Upstream("manual worker startup failed".into()))?
            .map_err(WorkerError::Upstream)?;
        Ok(Arc::new(Source { tx, view }))
    }
}

struct Server {
    session: Arc<WspSession>,
    labels: BTreeMap<String, String>,
    interval: Duration,
}
struct Host {
    terminal_input_policy: Arc<std::sync::atomic::AtomicBool>,
    session: Session,
    engines: Engines,
    settings: WorkerSettings,
    servers: BTreeMap<String, Server>,
    proxies: BTreeMap<String, crate::command::commands::daemon::client::DaemonClient>,
    proxy_runs: BTreeMap<String, ProxyRun>,
    runs: BTreeMap<String, ManualRun>,
    view: Arc<Mutex<View>>,
}
impl Host {
    /// The running daemon, if any, and the work servers its worker feature
    /// is pulling from.
    async fn daemon_servers(
        session: &Session,
    ) -> Result<
        (
            Option<crate::command::commands::daemon::client::DaemonClient>,
            Vec<ServerStatus>,
        ),
        String,
    > {
        let daemon =
            crate::command::commands::daemon::client::DaemonClient::discover(session.env())
                .map_err(|e| e.to_string())?;
        let servers: Vec<ServerStatus> = match &daemon {
            Some(client) => client
                .all(&crate::command::commands::daemon::actions::Request {
                    method: "GET".into(),
                    path: "/v1/worker/servers".into(),
                    query: vec![],
                    body: None,
                })
                .await
                .or_else(|e| match e {
                    crate::command::error::CommandError::DaemonHttpStatus {
                        status: 503, ..
                    } => Ok(vec![]),
                    other => Err(other),
                })
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?,
            None => vec![],
        };
        Ok((daemon, servers))
    }
    /// Whether the daemon holds the registration for `name`.
    fn daemon_owns(daemon_servers: &[ServerStatus], name: &str) -> bool {
        daemon_servers.iter().any(|entry| {
            entry.name == name
                && entry.enabled
                && entry.state != "disabled"
                && entry.state != "retiring"
        })
    }
    async fn connect(
        session: Session,
        engines: Engines,
        view: Arc<Mutex<View>>,
    ) -> Result<Self, String> {
        let settings = session.effective_config().daemon().worker;
        let paths =
            crate::data::fs::DataPaths::from_env(session.env()).map_err(|e| e.to_string())?;
        let links = LinkStore::new(&paths);
        let mut servers = BTreeMap::new();
        let (daemon, daemon_servers) = Self::daemon_servers(&session).await?;
        let mut proxies = BTreeMap::new();
        let runtime = engines.runtime.runtime_name();
        let kind = if runtime.contains("sbx") {
            RuntimeKind::Sbx
        } else if runtime.contains("apple") {
            RuntimeKind::Apple
        } else if runtime.contains("builtin") {
            RuntimeKind::Builtin
        } else {
            RuntimeKind::Docker
        };
        let paths = crate::data::RepoDockerfilePaths::new(session.git_root());
        let (available, _) = engines
            .agent_engine
            .filter_available_agents(&session, paths.discover_agent_dockerfiles());
        let agents: Vec<_> = available
            .into_iter()
            .filter(|(name, _)| {
                engines
                    .runtime
                    .image_exists(&crate::data::image_tags::agent_image_tag(
                        session.git_root(),
                        name,
                    ))
                    .unwrap_or(false)
            })
            .map(|(name, _)| name)
            .collect();
        let mut failures = Vec::new();
        for (name, config) in &settings.work_servers {
            if !config.enabled {
                continue;
            }
            let Some(url) = config.url.as_ref() else {
                continue;
            };
            if Self::daemon_owns(&daemon_servers, name) {
                // The daemon also owns registration while connecting or
                // recovering from an upstream error. Never create a second
                // worker merely because its first register call is pending.
                proxies.insert(
                    name.clone(),
                    daemon.as_ref().expect("daemon server").clone(),
                );
                continue;
            }
            // One unusable server must not take the queue away from the rest.
            let connected: Result<Server, String> = async {
                let token = links
                    .read(name)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        format!("work server {name:?} has no token; run awman worker link")
                    })?;
                let wsp = Arc::new(
                    WspSession::connect(name, url, token.refresh_token)
                        .map_err(|e| e.to_string())?,
                );
                let interaction = config.interaction.unwrap_or(settings.pull_interaction);
                let labels = HostFacts::for_this_build(
                    kind,
                    WorkerHostKind::Tui,
                    agents.clone(),
                    !interaction.is_headless() || settings.pull_allow_override,
                    interaction.is_headless() || settings.pull_allow_override,
                )
                .worker_labels(&config.labels)
                .map_err(|e| e.to_string())?;
                let support =
                    LeaseRuntimeSupport::from_capabilities(engines.runtime.capabilities());
                let registration = wsp
                    .register(&RegisterRequest {
                        name: format!("awman-tui-{name}"),
                        labels: labels.merged(),
                        capacity: config.max_concurrent_jobs,
                        features: super::capabilities::features(
                            support,
                            interaction,
                            settings.pull_allow_override,
                            true,
                            true,
                            true,
                        )
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                let labels = labels
                    .with_server(registration.server_labels)
                    .map_err(|e| e.to_string())?
                    .merged();
                Ok(Server {
                    session: wsp,
                    labels,
                    interval: crate::engine::worker::heartbeat::heartbeat_interval(Some(
                        registration.heartbeat_ms,
                    )),
                })
            }
            .await;
            match connected {
                Ok(server) => {
                    servers.insert(name.clone(), server);
                }
                Err(error) => failures.push(format!("work server {name:?}: {error}")),
            }
        }
        if servers.is_empty() && proxies.is_empty() {
            if let Some(error) = failures.first() {
                return Err(error.clone());
            }
        }
        for error in &failures {
            tracing::warn!(%error, "work server skipped");
        }
        Ok(Self {
            terminal_input_policy: Arc::new(std::sync::atomic::AtomicBool::new(
                session.effective_config().daemon().terminal.remote_input,
            )),
            session,
            engines,
            settings,
            servers,
            proxies,
            proxy_runs: BTreeMap::new(),
            runs: BTreeMap::new(),
            view,
        })
    }
    /// Reload the queue. One unreachable server does not hide the others:
    /// its last known rows stay listed and its error is shown.
    async fn refresh(&self) -> Result<(), String> {
        fn entry(row: QueueRow) -> QueueEntry {
            QueueEntry {
                id: row.id,
                server: row.server,
                title: row.title,
                summary: row.summary,
                kind: if row.kind == awman_wsp::WorkKind::Prompt {
                    "prompt"
                } else {
                    "workflow"
                }
                .into(),
                labels: row.labels,
                priority: row.priority,
                age_ms: row.age_ms,
                for_you: row.for_you,
                body: row.body,
                source_url: row.source_url,
            }
        }
        let mut listed: Vec<(String, Result<Vec<QueueEntry>, String>)> = Vec::new();
        for (name, client) in &self.proxies {
            let rows = async {
                client
                    .all(&crate::command::commands::daemon::actions::Request {
                        method: "GET".into(),
                        path: "/v1/worker/queue".into(),
                        query: vec![("server".into(), name.clone())],
                        body: None,
                    })
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|row| {
                        serde_json::from_value::<QueueRow>(row)
                            .map(entry)
                            .map_err(|e| e.to_string())
                    })
                    .collect::<Result<Vec<_>, String>>()
            }
            .await;
            listed.push((name.clone(), rows));
        }
        for (name, server) in &self.servers {
            let rows = async {
                let mut found = Vec::new();
                let mut cursor = None;
                loop {
                    let page = server
                        .session
                        .queue(500, cursor.as_deref())
                        .await
                        .map_err(|e| e.to_string())?;
                    found.extend(
                        page.items
                            .into_iter()
                            .map(|item| entry(QueueRow::from_wsp(name, item, Utc::now()))),
                    );
                    cursor = page.next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                }
                Ok::<_, String>(found)
            }
            .await;
            listed.push((name.clone(), rows));
        }
        let mut view = self.view.lock().unwrap();
        let mut items = Vec::new();
        let mut errors = Vec::new();
        for (name, rows) in listed {
            match rows {
                Ok(rows) => items.extend(rows),
                Err(error) => {
                    // Keep what this server last listed.
                    items.extend(
                        view.snapshot
                            .items
                            .iter()
                            .filter(|old| old.server == name)
                            .cloned(),
                    );
                    errors.push(format!("{name}: {error}"));
                }
            }
        }
        let added = items
            .iter()
            .filter(|item| {
                !view
                    .snapshot
                    .items
                    .iter()
                    .any(|old| old.id == item.id && old.server == item.server)
            })
            .count();
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        view.snapshot = QueueSnapshot {
            for_you: items.iter().filter(|i| i.for_you).count(),
            new_since_seen: view.snapshot.new_since_seen + added,
            items,
            error: error.clone(),
            loaded: true,
        };
        view.generation += 1;
        error.map_or(Ok(()), Err)
    }
    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<Request>) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                request = rx.recv() => match request {
                    None => break,
                    Some(Request::Begin(id, reply)) => {
                        if self.view.lock().unwrap().executing.contains(&id) {
                            let _ = reply.send(Err("lease is already executing".into()));
                            continue;
                        }
                        let result = if let Some(run) = self.runs.get(&id) {
                            run.active.started.store(true, Ordering::SeqCst);
                            Ok(())
                        } else if let Some(run) = self.proxy_runs.get(&id) {
                            run.bus.sender().emit(crate::data::execution_event::EventPayload::StatusMessage {
                                phase: "preparing".into(), message: "Foreground execution started".into(),
                            });
                            Ok(())
                        } else { Err("lease ended".into()) };
                        if result.is_ok() { self.view.lock().unwrap().executing.insert(id); }
                        let _ = reply.send(result);
                    }
                    Some(Request::Refresh) => {
                        if let Err(e) = self.refresh_policy().await {
                            let mut view = self.view.lock().unwrap();
                            view.snapshot.error = Some(e);
                            view.generation += 1;
                        } else {
                            // `refresh` records its own per-server errors.
                            let _ = self.refresh().await;
                        }
                    }
                    Some(Request::Pick(server, id, reply)) => { let result = self.pick(&server, &id).await; let _ = reply.send(result); }
                    Some(Request::Finish(id, choice, reply)) => { let result = self.finish(&id, choice).await; self.forget_ended(&id); let _ = reply.send(result); }
                    Some(Request::Complete(id, result, reply)) => { let result = self.complete(&id, *result).await; self.forget_ended(&id); let _ = reply.send(result); }
                    Some(Request::Checkout(id, choice, reply)) => { let result = self.checkout(&id, choice).await; let _ = reply.send(result); }
                },
                _ = tick.tick() => { let _ = self.refresh_policy().await; let _ = self.refresh().await; let _ = self.cleanup_retired(); }
            }
        }
        let ids: Vec<_> = self.runs.keys().cloned().collect();
        for id in ids {
            let _ = self.finish(&id, LeaseCloseChoice::Release).await;
        }
        for (id, run) in std::mem::take(&mut self.proxy_runs) {
            let _ = run.client.call(&proxy_request(&id, "release", None)).await;
        }
    }
    fn forget_ended(&self, id: &str) {
        if self.runs.contains_key(id) || self.proxy_runs.contains_key(id) {
            return;
        }
        let mut view = self.view.lock().unwrap();
        view.sessions.remove(id);
        view.picks.remove(id);
        view.executing.remove(id);
        view.closing.remove(id);
        view.events.remove(id);
        view.cancels.remove(id);
        view.engines.remove(id);
        view.writers.remove(id);
        view.terminal_input.remove(id);
        view.offers.remove(id);
    }
    async fn pick(&mut self, server: &str, id: &str) -> Result<PickRun, String> {
        self.refresh_policy().await?;
        if let Some(client) = self.proxies.get(server) {
            let proxy = ProxyRun::prepare(
                client.clone(),
                &self.session,
                &self.engines,
                server,
                id,
                &self.view,
            )
            .await?;
            let picked = proxy.picked.clone();
            self.proxy_runs.insert(picked.lease_id.clone(), proxy);
            return Ok(picked);
        }
        let server_handle = self
            .servers
            .get(server)
            .ok_or_else(|| format!("unknown server {server}"))?;
        let config = self
            .settings
            .work_servers
            .get(server)
            .ok_or("unknown server")?;
        if self
            .runs
            .values()
            .filter(|r| r.record.server == server)
            .count()
            >= config.max_concurrent_jobs
        {
            return Err("ineligible: work server capacity is full".into());
        }
        let daemon = self.session.effective_config().daemon();
        if self.runs.len()
            >= daemon
                .max_concurrent_jobs
                .min(daemon.worker.max_concurrent_jobs)
        {
            return Err("ineligible: worker capacity is full".into());
        }
        let outcome = server_handle
            .session
            .claim(Some(id), 0)
            .await
            // `QueueSource::pick` tells these two conflicts apart by prefix.
            .map_err(|e| match e {
                WorkerEngineError::Taken => "taken".to_string(),
                WorkerEngineError::Ineligible => format!("ineligible: {e}"),
                other => other.to_string(),
            })?;
        let ClaimOutcome::Leased(lease) = outcome else {
            return Err("item no longer available".into());
        };
        let plan = policy::plan(
            &lease.item,
            &server_handle.labels,
            config,
            &self.settings,
            &LeaseRuntimeSupport::from_capabilities(self.engines.runtime.capabilities()),
            self.engines.runtime.display_name(),
        )
        .and_then(|plan| policy::delivery(&lease, &plan, true).map(|()| plan));
        let plan = match plan {
            Ok(plan) => plan,
            Err(message) => {
                let _ = server_handle
                    .session
                    .release(
                        &LeaseRef::of(&lease),
                        &ReleaseRequest {
                            reason: ReleaseReason::Ineligible,
                            message: Some(message.clone()),
                            result: None,
                        },
                    )
                    .await;
                return Err(format!("ineligible: {message}"));
            }
        };
        let mut run = ManualRun::prepare(
            &self.session,
            &self.engines,
            server_handle.session.clone(),
            ManualClaim {
                server,
                lease,
                plan,
                interval: server_handle.interval,
            },
            &|_, _| {},
        )
        .await?;
        let policy = self.terminal_input_policy.clone();
        run.start_terminal(Arc::new(move || {
            policy.load(std::sync::atomic::Ordering::SeqCst)
        }));
        let picked = run.pick();
        {
            let mut view = self.view.lock().unwrap();
            view.sessions
                .insert(picked.lease_id.clone(), run.session.clone());
            view.events.insert(picked.lease_id.clone(), run.bus.clone());
            view.cancels
                .insert(picked.lease_id.clone(), run.active.control.cancelled());
            let mut engines = self.engines.clone();
            view.terminal_input
                .insert(picked.lease_id.clone(), run.lease.remote_input);
            if let Some(bridge) = run.terminal.clone() {
                let hub = Arc::new(crate::engine::terminal_hub::TerminalHub::new(
                    Default::default(),
                ));
                run.local_terminal = Some(super::terminal::relay(
                    hub.clone(),
                    run.writer.command_id(),
                    run.lease.remote_input,
                    run.active.done.child_token(),
                    run.active.control.cancelled(),
                    move |frames| {
                        let bridge = bridge.clone();
                        async move { bridge.exchange(frames).await }
                    },
                ));
                engines.runtime = Arc::new(
                    crate::command::commands::api_server::owned_runtime::CommandOwnedRuntime::new(
                        engines.runtime.clone(),
                        run.writer.command_id(),
                    )
                    .with_hub(hub),
                );
            }
            engines.agent_engine = Arc::new(
                (*engines.agent_engine)
                    .clone()
                    .with_read_only_workspace(run.lease.item.exec.read_only)
                    .with_lease_workspace_scope(run.session.git_root().to_path_buf()),
            );
            view.engines.insert(picked.lease_id.clone(), engines);
            view.writers
                .insert(picked.lease_id.clone(), run.writer.clone());
            if let Some(prompt) =
                super::pick::checkout_offer(&self.session, &self.engines, &run.lease.item)
            {
                view.offers.insert(picked.lease_id.clone(), prompt);
            }
        }
        self.runs.insert(picked.lease_id.clone(), run);
        let _ = self.refresh().await;
        Ok(picked)
    }
    async fn finish(&mut self, id: &str, choice: LeaseCloseChoice) -> Result<(), String> {
        if choice == LeaseCloseChoice::KeepRunning {
            return Ok(());
        }
        {
            let mut view = self.view.lock().unwrap();
            if view.executing.contains(id) {
                view.closing.insert(id.into(), choice);
                if let Some(cancel) = view.cancels.get(id) {
                    cancel.cancel();
                }
                return Ok(());
            }
        }
        if choice == LeaseCloseChoice::Cancel {
            if let Some(run) = self.proxy_runs.get(id) {
                run.cancel.cancel();
            }
            if let Some(run) = self.runs.get(id) {
                run.active.control.cancelled().cancel();
            }
            let result = CommandResult {
                outcome: Some(crate::data::execution_event::CommandStatusKind::Cancelled),
                exit_code: Some(130),
                ..Default::default()
            };
            self.complete(id, result).await?;
            return Ok(());
        }
        if let Some(run) = self.proxy_runs.remove(id) {
            run.cancel.cancel();
            run.client
                .call(&proxy_request(
                    id,
                    "release",
                    Some(serde_json::json!({"reason":"user_declined"})),
                ))
                .await
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        let mut run = self.runs.remove(id).ok_or("lease ended")?;
        run.active.control.cancelled().cancel();
        run.release(
            ReleaseReason::UserDeclined,
            if choice == LeaseCloseChoice::Cancel {
                "user cancelled the tab"
            } else {
                "user released the tab"
            },
        )
        .await
    }
    async fn complete(&mut self, id: &str, result: CommandResult) -> Result<CommandResult, String> {
        let close = self.view.lock().unwrap().closing.remove(id);
        self.view.lock().unwrap().executing.remove(id);
        let release_reason =
            (close == Some(LeaseCloseChoice::Release)).then_some(ReleaseReason::UserDeclined);
        if let Some(mut run) = self.proxy_runs.remove(id) {
            if let Some(terminal) = &run.terminal {
                if let Err(error) = terminal.flush().await {
                    run.cancel.cancel();
                    let _ = run
                        .client
                        .call(&proxy_request(
                            id,
                            "release",
                            Some(serde_json::json!({"reason":"error", "message":error})),
                        ))
                        .await;
                    return Err(error);
                }
            }
            let mounted = self
                .view
                .lock()
                .unwrap()
                .writers
                .get(id)
                .and_then(|writer| writer.result_contributions().outputs_mounted)
                .unwrap_or(false);
            let result = crate::data::worker::wire::ForegroundResult {
                completion: awman_wsp::LeaseResult {
                    result,
                    outputs: None,
                    outputs_mounted: mounted,
                    duration_ms: 0,
                },
                release_reason,
            };
            run.bus
                .sender()
                .emit(crate::data::execution_event::EventPayload::Done);
            if let Some(events) = run.events.take() {
                events.await.map_err(|e| e.to_string())??;
            }
            return run
                .client
                .call_as(&proxy_request(
                    id,
                    "result",
                    Some(serde_json::to_value(result).map_err(|e| e.to_string())?),
                ))
                .await
                .map_err(|e| e.to_string());
        }
        let mut run = self.runs.remove(id).ok_or("lease ended")?;
        if let Some(reason) = release_reason {
            run.active.finish.lock().unwrap().release =
                Some((reason, Some("user released the tab".into())));
        }
        run.complete(&self.engines, result).await
    }
    async fn checkout(&mut self, id: &str, choice: CheckoutChoice) -> Result<PickRun, String> {
        if let Some(run) = self.proxy_runs.get_mut(id) {
            return run.checkout(&self.session, choice, &self.view).await;
        }
        let run = self.runs.get_mut(id).ok_or("lease ended")?;
        if choice == CheckoutChoice::Worktree {
            run.rehome(&self.session, &self.engines).await?;
        }
        let picked = run.pick();
        let mut view = self.view.lock().unwrap();
        view.sessions.insert(id.into(), run.session.clone());
        if let Some(engines) = view.engines.get_mut(id) {
            engines.agent_engine = Arc::new(
                (*engines.agent_engine)
                    .clone()
                    .with_lease_workspace_scope(run.session.git_root().into()),
            );
        }
        Ok(picked)
    }
    async fn refresh_policy(&mut self) -> Result<(), String> {
        self.session.reload_config().map_err(|e| e.to_string())?;
        self.terminal_input_policy.store(
            self.session
                .effective_config()
                .daemon()
                .terminal
                .remote_input,
            std::sync::atomic::Ordering::SeqCst,
        );
        let settings = self.session.effective_config().daemon().worker;
        let settings_changed = settings.work_servers != self.settings.work_servers
            || settings.pull_interaction != self.settings.pull_interaction
            || settings.pull_allow_override != self.settings.pull_allow_override;
        // The daemon may start or stop pulling during this host's lifetime.
        // Whoever holds a server's registration is re-checked between runs,
        // so there is never a second worker, nor a server left to a daemon
        // that has gone.
        let owner_changed = !settings_changed
            && self.runs.is_empty()
            && self.proxy_runs.is_empty()
            && match Self::daemon_servers(&self.session).await {
                Ok((_, daemon_servers)) => settings
                    .work_servers
                    .iter()
                    .filter(|(_, config)| config.enabled && config.url.is_some())
                    .any(|(name, _)| {
                        Self::daemon_owns(&daemon_servers, name) != self.proxies.contains_key(name)
                    }),
                Err(_) => false,
            };
        if settings_changed || owner_changed {
            let replacement = Self::connect(
                self.session.clone(),
                self.engines.clone(),
                self.view.clone(),
            )
            .await?;
            self.servers = replacement.servers;
            self.proxies = replacement.proxies;
        }
        self.settings = settings;
        Ok(())
    }
    fn cleanup_retired(&self) -> Result<(), String> {
        let paths =
            crate::data::fs::DataPaths::from_env(self.session.env()).map_err(|e| e.to_string())?;
        let links = LinkStore::new(&paths);
        let store =
            Arc::new(SqliteSessionStore::open_at(&paths.db_path()).map_err(|e| e.to_string())?);
        let leases = LeaseStore::open(store).map_err(|e| e.to_string())?;
        for name in links.retired().map_err(|e| e.to_string())? {
            if !self.settings.work_servers.contains_key(&name)
                && leases
                    .active_for_server(&name)
                    .map_err(|e| e.to_string())?
                    .is_empty()
            {
                links.remove(&name).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

pub(super) struct ManualRun {
    local_terminal: Option<super::terminal::Relay>,
    pub(super) terminal: Option<Arc<super::terminal::Bridge>>,
    pub(super) record: LeaseRecord,
    lease: Lease,
    wsp: Arc<WspSession>,
    pub(super) session: Session,
    plan: policy::LeasePlan,
    workspace: Arc<LeaseWorkspace>,
    _inline: InlineWorkItem,
    transport: LeaseTransport,
    pub(super) active: Arc<Active>,
    pub(super) bus: Arc<EventBus>,
    interval: Duration,
    store: Arc<SqliteSessionStore>,
    events: Option<tokio::task::JoinHandle<Result<u64, WorkerEngineError>>>,
    started: std::time::Instant,
    _heartbeat: tokio::task::JoinHandle<crate::engine::worker::heartbeat::HeartbeatEnd>,
    output_dir: PathBuf,
    writer: crate::data::fs::api_command_log::CommandStateWriter,
    log: Option<tokio::task::JoinHandle<()>>,
    git_source: crate::engine::git::remote_push::WorkflowGitSource,
    runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine>,
    _timeout: tokio_util::task::AbortOnDropHandle<()>,
}
/// A manually claimed lease and how to run it.
pub(super) struct ManualClaim<'a> {
    pub(super) server: &'a str,
    pub(super) lease: Lease,
    pub(super) plan: policy::LeasePlan,
    /// Heartbeat interval the server asked for.
    pub(super) interval: Duration,
}

impl ManualRun {
    pub(super) fn start_terminal(&mut self, policy: Arc<dyn Fn() -> bool + Send + Sync>) {
        if self.lease.terminal {
            self.terminal = Some(super::terminal::Bridge::start(
                self.wsp.clone(),
                &self.lease,
                self.active.control.clone(),
                self.active.done.child_token(),
                policy,
            ));
        }
    }
    pub(super) fn record_outputs_mounted(&self, mounted: bool) {
        self.writer.record_outputs_mounted(mounted);
    }
    pub(super) async fn rehome(
        &mut self,
        invoker: &Session,
        engines: &Engines,
    ) -> Result<(), String> {
        if super::pick::checkout_offer(invoker, engines, &self.lease.item).is_none() {
            return Err("the selected checkout has a different origin".into());
        }
        let git = &engines.git_engine;
        let head = git
            .head_sha(self.workspace.clone_dir())
            .map_err(|e| e.to_string())?;
        let common = git
            .git_common_dir(invoker.git_root())
            .map_err(|e| e.to_string())?;
        crate::data::fs::git_metadata::GitMetadataCopy::copy(
            &self.workspace.clone_dir().join(".git/objects"),
            &common.join("objects"),
        )
        .map_err(|e| e.to_string())?;
        let path = git
            .worktree_path_named(invoker.git_root(), self.record.id.as_str())
            .map_err(|e| e.to_string())?;
        git.create_worktree_from_commit(
            invoker.git_root(),
            &path,
            &format!("awman/{}", self.record.id),
            &head,
        )
        .map_err(|e| e.to_string())?;
        self.session =
            Session::open_worktree_for(path.clone(), path, invoker).map_err(|e| e.to_string())?;
        self.session
            .set_inline_work_item(self._inline.path().into());
        super::provision::agents_for_lease(
            engines,
            &self.session,
            &self.lease.item,
            self.workspace.private_dir(),
        )
        .await
        .map_err(|e| e.to_string())?;
        self.session.set_session_type(SessionType::Remote {
            repo_url: self.lease.item.repo.url.clone(),
            branch: self.lease.item.repo.branch.clone(),
            cloned_path: self.session.git_root().into(),
        });
        self.git_source = git
            .capture_workflow_git_source(self.session.git_root())
            .map_err(|e| e.to_string())?;
        let jobs = JobStore::open(self.store.clone()).map_err(|e| e.to_string())?;
        if let Some(id) = &self.record.job_id {
            if let Some((origin, mut spec)) = jobs.get(id).map_err(|e| e.to_string())? {
                spec.workspace = JobWorkspace::Local {
                    workdir: self.session.git_root().into(),
                };
                jobs.insert(id, &origin, &spec, &self.record.claimed_at.to_rfc3339())
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    pub(super) fn handoff(&self) -> ForegroundPick {
        ForegroundPick {
            lease: (&self.record).into(),
            command: self.plan.command.clone(),
            args: self.plan.args.clone(),
            cwd: self.session.git_root().into(),
            inline_source: self._inline.path().into(),
            output_dir: self.output_dir.clone(),
            repo_url: self.lease.item.repo.url.clone(),
            branch: self.lease.item.repo.branch.clone(),
            read_only: self.lease.item.exec.read_only,
        }
    }
    pub(super) fn publish(
        &self,
        events: Vec<crate::data::execution_event::ExecutionEvent>,
    ) -> Result<(), WorkerError> {
        if !self.active.control.may_report() {
            return Err(WorkerError::LeaseEnded(self.record.id.to_string()));
        }
        for event in events {
            if event.sequence < self.bus.next_sequence() {
                continue;
            }
            if event.sequence != self.bus.next_sequence() || event.attempt != 1 {
                return Err(WorkerError::Invalid(
                    "foreground events must arrive in sequence for attempt 1".into(),
                ));
            }
            if matches!(
                event.payload,
                crate::data::execution_event::EventPayload::Done
                    | crate::data::execution_event::EventPayload::CommandStatus { .. }
            ) {
                return Err(WorkerError::Invalid(
                    "the worker owns terminal command events".into(),
                ));
            }
            self.active.started.store(true, Ordering::SeqCst);
            let sender = self.bus.sender();
            if let Some(source) = event.source {
                sender.with_source(source).emit(event.payload);
            } else {
                sender.emit(event.payload);
            }
        }
        Ok(())
    }
    pub(super) async fn prepare(
        invoker: &Session,
        engines: &Engines,
        wsp: Arc<WspSession>,
        claim: ManualClaim<'_>,
        on_claim: &(dyn Fn(&LeaseRecord, Arc<Active>) + Sync),
    ) -> Result<Self, String> {
        let ManualClaim {
            server,
            lease,
            mut plan,
            interval,
        } = claim;
        let (control, heartbeat) = HeartbeatLoop::spawn(
            wsp.clone(),
            LeaseRef::of(&lease),
            interval,
            HeartbeatStatus::new(HeartbeatState::Preparing),
            lease.expires_at,
        );
        let mut preparation = None;
        let result = async {
            let paths =
                crate::data::fs::DataPaths::from_env(invoker.env()).map_err(|e| e.to_string())?;
            let root = crate::data::worker::paths::work_root(
                &paths,
                &invoker.effective_config().daemon().worker,
            );
            let mut record = LeaseRecord::claimed(server, &lease, LeaseHost::Tui, Utc::now());
            let owner = crate::data::worker::paths::try_lease_owner(paths.root(), &record.id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "lease already owned".to_string())?;
            let store =
                Arc::new(SqliteSessionStore::open_at(&paths.db_path()).map_err(|e| e.to_string())?);
            let leases = LeaseStore::open(store.clone()).map_err(|e| e.to_string())?;
            leases.insert(&record).map_err(|e| e.to_string())?;
            let active = Arc::new(Active {
                foreground_seen: Default::default(),
                started: AtomicBool::new(false),
                _owner: owner,
                server: server.into(),
                control: control.clone(),
                finish: Default::default(),
                outputs: Default::default(),
                done: CancellationToken::new(),
            });
            preparation = Some((record.clone(), active.clone(), leases.clone()));
            on_claim(&record, active.clone());
            let workspace =
                Arc::new(LeaseWorkspace::prepare(&root, &record.id).map_err(|e| e.to_string())?);
            let helper = super::credential_helper_program().map_err(|e| e.to_string())?;
            let transport = crate::engine::worker::credential_helper::lease_transport(
                &workspace,
                &lease.item,
                helper,
                Default::default(),
            )
            .map_err(|e| e.to_string())?;
            let inline = workspace
                .inline_work_item(&lease.item.id, &lease.item.body)
                .map_err(|e| e.to_string())?;
            if let Some(toml) = &lease.item.exec.workflow_toml {
                let path = workspace.private_dir().join("workflow.toml");
                use std::io::Write;
                crate::data::fs::private_files::create_private_file_new(&path)
                    .map_err(|e| e.to_string())?
                    .write_all(toml.as_bytes())
                    .map_err(|e| e.to_string())?;
                for arg in &mut plan.args {
                    if arg == "LEASE_WORKFLOW_FILE" {
                        *arg = path.to_string_lossy().into();
                    }
                }
            }
            let ws = workspace.clone();
            let trans = transport.clone();
            let repo = lease.item.repo.clone();
            let env = invoker.env().for_lease_git();
            let cancel = control.cancelled();
            tokio::task::spawn_blocking(move || {
                LeaseCheckout::new(&crate::engine::git::GitEngine::with_env(env), &trans).run(
                    &ws,
                    &repo,
                    &mut crate::data::message::RecordingMessageSink::new(),
                    &cancel,
                )
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
            let cwd = workspace.clone_dir().to_path_buf();
            let mut session = Session::open_at_git_root(
                cwd.clone(),
                cwd.clone(),
                SessionOpenOptions {
                    env: Some(invoker.env().clone()),
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
            session.set_inline_work_item(inline.path().to_path_buf());
            session.set_session_type(SessionType::Remote {
                repo_url: lease.item.repo.url.clone(),
                branch: lease.item.repo.branch.clone(),
                cloned_path: cwd,
            });
            super::provision::agents_for_lease(
                engines,
                &session,
                &lease.item,
                workspace.private_dir(),
            )
            .await
            .map_err(|e| e.to_string())?;
            let job_id = crate::data::ids::CommandId::new().to_string();
            record.job_id = Some(job_id.clone());
            record.state = LeaseState::Running;
            leases.update(&record).map_err(|e| e.to_string())?;
            JobStore::open(store.clone())
                .map_err(|e| e.to_string())?
                .insert(
                    &job_id,
                    &record.origin(),
                    &JobSpec {
                        command: plan.command.clone(),
                        args: plan.args.clone(),
                        interaction: plan.interaction,
                        workspace: JobWorkspace::Clone {
                            repo_url: lease.item.repo.url.clone(),
                            base_ref: Some(lease.item.repo.base_ref.clone()),
                            branch: lease.item.repo.branch.clone(),
                            merge_refs: lease.item.repo.merge_refs.clone(),
                            push_branch: lease.item.push_branch().map(str::to_string),
                            read_only: lease.item.exec.read_only,
                            work_root: root,
                        },
                    },
                    &Utc::now().to_rfc3339(),
                )
                .map_err(|e| e.to_string())?;
            let api_paths = crate::data::fs::api_paths::ApiPaths::from_env(invoker.env())
                .map_err(|e| e.to_string())?;
            let session_id = crate::data::ids::WorkerSessionId::new().to_string();
            store
                .insert_session_full(crate::data::fs::api_db::NewSessionRow {
                    id: &session_id,
                    workdir: &session.working_dir().to_string_lossy(),
                    created_at: &Utc::now().to_rfc3339(),
                    setup_status: crate::data::session_setup_event::SessionSetupStatus::Ready,
                    kind: crate::data::session::SessionKind::Remote,
                    cloned_path: Some(&session.working_dir().to_string_lossy()),
                })
                .map_err(|e| e.to_string())?;
            store
                .insert_running_lease_command(
                    &job_id,
                    &session_id,
                    &plan.command.join(" "),
                    &serde_json::to_string(&plan.args).map_err(|e| e.to_string())?,
                    false,
                )
                .map_err(|e| e.to_string())?;
            api_paths
                .prepare_command_dir(&session_id, &job_id)
                .map_err(|e| e.to_string())?;
            let writer = crate::data::fs::api_command_log::CommandStateWriter::new(
                api_paths.clone(),
                &session_id,
                &job_id,
            );
            let output_dir = writer.output_dir();
            Ok::<_, String>((
                record, workspace, inline, transport, session, store, active, output_dir, writer,
                api_paths, session_id,
            ))
        }
        .await;
        match result {
            Ok((
                record,
                workspace,
                inline,
                transport,
                session,
                store,
                active,
                output_dir,
                writer,
                api_paths,
                session_id,
            )) => {
                control.set_state(HeartbeatState::Running);
                let bus =
                    Arc::new(EventBus::new(4096).with_awaiting_input(control.awaiting_input()));
                let events = super::runner::foreground_events(
                    wsp.clone(),
                    &lease,
                    control.clone(),
                    &bus,
                    active.done.child_token(),
                );
                let mut log_rx = bus.subscribe_log();
                let job_id = record.job_id.clone().expect("prepared job");
                let log = tokio::spawn(async move {
                    if let Ok(mut writer) =
                        crate::data::fs::api_command_log::CommandLogWriter::open_append(
                            &api_paths,
                            &session_id,
                            &job_id,
                        )
                        .await
                    {
                        while let Some(event) = log_rx.recv().await {
                            writer.write_event(&event).await;
                            if matches!(
                                event.payload,
                                crate::data::execution_event::EventPayload::Done
                            ) {
                                writer.flush().await;
                                break;
                            }
                        }
                    }
                });
                let git_source = engines
                    .git_engine
                    .capture_workflow_git_source(session.git_root())
                    .map_err(|e| e.to_string())?;
                let timeout = tokio_util::task::AbortOnDropHandle::new(tokio::spawn({
                    let control = control.clone();
                    let timeout = lease.item.exec.timeout_ms;
                    async move {
                        if let Some(ms) = timeout {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            control.cancelled().cancel();
                        }
                    }
                }));
                Ok(Self {
                    local_terminal: None,
                    terminal: None,
                    record,
                    lease,
                    wsp,
                    session,
                    plan,
                    workspace,
                    _inline: inline,
                    transport,
                    active,
                    bus,
                    events: Some(events),
                    started: std::time::Instant::now(),
                    interval,
                    store,
                    _heartbeat: heartbeat,
                    output_dir,
                    writer,
                    log: Some(log),
                    git_source,
                    runtime: engines.runtime.clone(),
                    _timeout: timeout,
                })
            }
            Err(error) => {
                let reason = preparation
                    .as_ref()
                    .and_then(|(_, active, _)| {
                        active
                            .finish
                            .lock()
                            .unwrap()
                            .release
                            .as_ref()
                            .map(|(reason, _)| *reason)
                    })
                    .unwrap_or(ReleaseReason::Error);
                let mut released = false;
                if control.may_report() {
                    released = wsp
                        .release(
                            &LeaseRef::of(&lease),
                            &ReleaseRequest {
                                reason,
                                message: Some(error.clone()),
                                result: None,
                            },
                        )
                        .await
                        .is_ok();
                }
                if let Some((mut record, active, leases)) = preparation {
                    record.state = if released {
                        LeaseState::Released
                    } else {
                        LeaseState::Lost
                    };
                    record.ended_at = Some(Utc::now());
                    record.release = Some(LeaseRelease {
                        reason,
                        message: Some(error.clone()),
                    });
                    let _ = leases.update(&record);
                    active.done.cancel();
                }
                control.stop();
                let _ = heartbeat.await;
                Err(error)
            }
        }
    }
    fn pick(&self) -> PickRun {
        PickRun {
            lease_id: self.record.id.to_string(),
            title: self.record.item.title.clone(),
            server: self.record.server.clone(),
            cwd: self.session.working_dir().to_path_buf(),
            command_line: shell_words::join(
                self.plan
                    .command
                    .iter()
                    .chain(&self.plan.args)
                    .map(String::as_str),
            ),
        }
    }
    pub(super) async fn release(
        &mut self,
        reason: ReleaseReason,
        message: &str,
    ) -> Result<(), String> {
        if let Some(job) = &self.record.job_id {
            crate::command::commands::api_server::owned_runtime::stop_command_containers(
                self.runtime.as_ref(),
                job,
            )
            .map_err(|e| e.to_string())?;
        }
        self.active.control.cancelled().cancel();
        let mut partial = crate::command::commands::api_server::queue_worker::foreground_result(
            &Err(crate::command::error::CommandError::Aborted),
            self.writer.result_contributions(),
        );
        partial.outcome = Some(crate::data::execution_event::CommandStatusKind::Cancelled);
        partial.exit_code = Some(130);
        if let Some(job) = &self.record.job_id {
            let _ = self.store.complete_command(
                job,
                crate::data::command_status::CommandStatus::Cancelled,
                Some(130),
                Some(&serde_json::to_string(&partial).map_err(|e| e.to_string())?),
            );
        }
        // Publish the same terminal event shape as an executed command before
        // giving the lease back, and let both the log and WSP drain it.
        self.bus
            .sender()
            .emit(crate::data::execution_event::EventPayload::CommandStatus {
                status: crate::data::execution_event::CommandStatusKind::Cancelled,
                exit_code: Some(130),
                error: None,
                result: Some(Box::new(partial.clone())),
            });
        self.bus
            .sender()
            .emit(crate::data::execution_event::EventPayload::Done);
        if let Some(events) = self.events.take() {
            let _ = super::runner::drain_events(events, &self.active.control).await;
        }
        if let Some(log) = self.log.take() {
            let _ = log.await;
        }
        let result = if self.active.control.may_report() {
            self.wsp
                .release(
                    &LeaseRef::of(&self.lease),
                    &ReleaseRequest {
                        reason,
                        message: Some(message.into()),
                        result: Some(partial.clone()),
                    },
                )
                .await
                .map_err(|e| e.to_string())
        } else {
            Err("lease lost".into())
        };
        self.record.state = if result.is_ok() {
            LeaseState::Released
        } else {
            LeaseState::Lost
        };
        self.record.ended_at = Some(Utc::now());
        self.record.release = Some(LeaseRelease {
            reason,
            message: Some(message.into()),
        });
        let _ = LeaseStore::open(self.store.clone()).and_then(|s| s.update(&self.record));
        self.active.control.stop();
        result
    }
    pub(super) async fn complete(
        &mut self,
        engines: &Engines,
        result: CommandResult,
    ) -> Result<CommandResult, String> {
        if let Some(terminal) = &self.local_terminal {
            if let Err(error) = terminal.flush().await {
                let _ = self.release(ReleaseReason::Error, &error).await;
                return Err(error);
            }
        }
        let result = super::runner::complete_foreground(
            engines,
            self.wsp.clone(),
            self.lease.clone(),
            self.workspace.clone(),
            self.transport.clone(),
            self.active.clone(),
            self.interval,
            self.bus.clone(),
            self.output_dir.clone(),
            self.session.env().clone(),
            self.started.elapsed(),
            &mut self.events,
            result,
            self.session.git_root().into(),
            self.git_source.clone(),
            self.writer
                .result_contributions()
                .outputs_mounted
                .unwrap_or(false),
            self.store.clone(),
            self.record.job_id.clone().expect("prepared job"),
        )
        .await;
        self.record.state = if result.is_ok() {
            if self.active.finish.lock().unwrap().release.is_some() {
                LeaseState::Released
            } else {
                LeaseState::Done
            }
        } else {
            LeaseState::Lost
        };
        self.record.ended_at = Some(Utc::now());
        let _ = LeaseStore::open(self.store.clone()).and_then(|s| s.update(&self.record));
        if let Some(log) = self.log.take() {
            if result.is_ok() {
                let _ = log.await;
            } else {
                log.abort();
            }
        }
        self.active.control.stop();
        result
    }
}
impl Drop for ManualRun {
    fn drop(&mut self) {
        self.active.control.stop();
        self.active.control.cancelled().cancel();
        self.active.done.cancel();
    }
}

fn proxy_request(
    id: &str,
    operation: &str,
    body: Option<serde_json::Value>,
) -> crate::command::commands::daemon::actions::Request {
    crate::command::commands::daemon::actions::Request {
        method: "POST".into(),
        path: format!("/v1/worker/leases/{id}/{operation}"),
        query: vec![],
        body,
    }
}
struct ProxyRun {
    terminal: Option<super::terminal::Relay>,
    _terminal_stop: tokio_util::sync::DropGuard,
    client: crate::command::commands::daemon::client::DaemonClient,
    picked: PickRun,
    bus: Arc<EventBus>,
    cancel: CancellationToken,
    events: Option<tokio::task::JoinHandle<Result<(), String>>>,
}
impl ProxyRun {
    async fn checkout(
        &mut self,
        invoker: &Session,
        choice: CheckoutChoice,
        view: &Arc<Mutex<View>>,
    ) -> Result<PickRun, String> {
        if choice == CheckoutChoice::FreshClone {
            return Ok(self.picked.clone());
        }
        let response = self
            .client
            .call(&proxy_request(
                &self.picked.lease_id,
                "checkout",
                Some(serde_json::json!({"workdir":invoker.git_root()})),
            ))
            .await
            .map_err(|e| e.to_string())?;
        let cwd: PathBuf =
            serde_json::from_value(response["cwd"].clone()).map_err(|e| e.to_string())?;
        let mut view = view.lock().unwrap();
        let old = view
            .sessions
            .get(&self.picked.lease_id)
            .ok_or("lease session missing")?;
        let mut session = Session::open_worktree_for(cwd.clone(), cwd.clone(), invoker)
            .map_err(|e| e.to_string())?;
        if let Some(inline) = old.inline_work_item() {
            session.set_inline_work_item(inline.into());
        }
        if let SessionType::Remote {
            repo_url, branch, ..
        } = old.session_type()
        {
            session.set_session_type(SessionType::Remote {
                repo_url: repo_url.clone(),
                branch: branch.clone(),
                cloned_path: cwd.clone(),
            });
        }
        view.sessions.insert(self.picked.lease_id.clone(), session);
        if let Some(engines) = view.engines.get_mut(&self.picked.lease_id) {
            engines.agent_engine = Arc::new(
                (*engines.agent_engine)
                    .clone()
                    .with_lease_workspace_scope(cwd.clone()),
            );
        }
        self.picked.cwd = cwd;
        Ok(self.picked.clone())
    }
    async fn prepare(
        client: crate::command::commands::daemon::client::DaemonClient,
        invoker: &Session,
        engines: &Engines,
        server: &str,
        item: &str,
        view: &Arc<Mutex<View>>,
    ) -> Result<Self, String> {
        let value = client
            .call(&crate::command::commands::daemon::actions::Request {
                method: "POST".into(),
                path: claim_path(item),
                query: vec![
                    ("server".into(), server.into()),
                    ("host".into(), "foreground".into()),
                ],
                body: None,
            })
            .await
            .map_err(|e| match &e {
                crate::command::error::CommandError::DaemonHttpStatus { status: 409, body }
                    if body.contains("\"taken\"") =>
                {
                    "taken".to_string()
                }
                crate::command::error::CommandError::DaemonHttpStatus { status: 409, body }
                    if body.contains("\"ineligible\"") =>
                {
                    format!("ineligible: {e}")
                }
                _ => e.to_string(),
            })?;
        let lease: crate::data::worker::wire::LeaseResource =
            serde_json::from_value(value["lease"].clone()).map_err(|e| e.to_string())?;
        let command: Vec<String> =
            serde_json::from_value(value["command"].clone()).map_err(|e| e.to_string())?;
        let args: Vec<String> =
            serde_json::from_value(value["args"].clone()).map_err(|e| e.to_string())?;
        let cwd: PathBuf =
            serde_json::from_value(value["cwd"].clone()).map_err(|e| e.to_string())?;
        let inline: PathBuf =
            serde_json::from_value(value["inline_source"].clone()).map_err(|e| e.to_string())?;
        let text = |key: &str| {
            value[key]
                .as_str()
                .ok_or_else(|| format!("missing handoff {key}"))
                .map(str::to_owned)
        };
        let mut session = Session::open_at_git_root(
            cwd.clone(),
            cwd.clone(),
            SessionOpenOptions {
                env: Some(invoker.env().clone()),
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())?;
        session.set_inline_work_item(inline);
        session.set_session_type(SessionType::Remote {
            repo_url: text("repo_url")?,
            branch: text("branch")?,
            cloned_path: cwd.clone(),
        });
        let job_id = lease
            .job_id
            .as_deref()
            .ok_or("foreground handoff has no job")?;
        let record = client
            .call(&crate::command::commands::daemon::actions::Request {
                method: "GET".into(),
                path: format!("/v1/worker/commands/{job_id}"),
                query: vec![],
                body: None,
            })
            .await
            .map_err(|e| e.to_string())?;
        let session_id = record["session_id"]
            .as_str()
            .ok_or("foreground command has no session")?;
        let paths = crate::data::fs::api_paths::ApiPaths::from_env(invoker.env())
            .map_err(|e| e.to_string())?;
        let writer =
            crate::data::fs::api_command_log::CommandStateWriter::new(paths, session_id, job_id);
        let bus = Arc::new(EventBus::new(4096));
        let mut rx = bus.subscribe_ordered();
        let event_client = client.clone();
        let id = lease.id.clone();
        let events = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if matches!(
                    event.payload,
                    crate::data::execution_event::EventPayload::Done
                ) {
                    break;
                }
                event_client
                    .call(&proxy_request(
                        &id,
                        "events",
                        Some(serde_json::to_value(vec![event]).map_err(|e| e.to_string())?),
                    ))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        let cancel = CancellationToken::new();
        let terminal_stop = CancellationToken::new();
        let picked = PickRun {
            lease_id: lease.id.clone(),
            title: lease.item_title,
            server: server.into(),
            cwd: cwd.clone(),
            command_line: shell_words::join(command.iter().chain(&args).map(String::as_str)),
        };
        let mut engines = engines.clone();
        let read_only = value["read_only"]
            .as_bool()
            .ok_or("missing read_only handoff")?;
        engines.agent_engine = Arc::new(
            (*engines.agent_engine)
                .clone()
                .with_read_only_workspace(read_only)
                .with_lease_workspace_scope(cwd),
        );
        engines.runtime = Arc::new(
            crate::command::commands::api_server::owned_runtime::CommandOwnedRuntime::new(
                engines.runtime.clone(),
                job_id,
            ),
        );
        let mut terminal = None;
        if lease.terminal {
            let hub = Arc::new(crate::engine::terminal_hub::TerminalHub::new(
                Default::default(),
            ));
            let terminal_client = client.clone().with_timeout(Duration::from_secs(10));
            let terminal_id = lease.id.clone();
            terminal = Some(super::terminal::relay(
                hub.clone(),
                job_id,
                lease.remote_input,
                terminal_stop.clone(),
                cancel.clone(),
                move |frames| {
                    let client = terminal_client.clone();
                    let id = terminal_id.clone();
                    async move {
                        let value = client
                            .call(&proxy_request(
                                &id,
                                "terminal",
                                Some(serde_json::to_value(frames).map_err(|e| e.to_string())?),
                            ))
                            .await
                            .map_err(|e| e.to_string())?;
                        serde_json::from_value(value["items"].clone()).map_err(|e| e.to_string())
                    }
                },
            ));
            engines.runtime = Arc::new(
                crate::command::commands::api_server::owned_runtime::CommandOwnedRuntime::new(
                    engines.runtime.clone(),
                    job_id,
                )
                .with_hub(hub),
            );
        }
        {
            let mut view = view.lock().unwrap();
            view.terminal_input
                .insert(lease.id.clone(), lease.remote_input);
            view.sessions.insert(lease.id.clone(), session);
            view.events.insert(lease.id.clone(), bus.clone());
            view.cancels.insert(lease.id.clone(), cancel.clone());
            view.engines.insert(lease.id.clone(), engines.clone());
            view.writers.insert(lease.id.clone(), writer);
            if engines
                .git_engine
                .remote_url(invoker.git_root(), "origin")
                .ok()
                .is_some_and(|origin| {
                    crate::data::worker::paths::same_remote(
                        &origin,
                        value["repo_url"].as_str().unwrap_or(""),
                    )
                })
            {
                view.offers.insert(
                    lease.id.clone(),
                    super::queue_source::checkout_prompt(&picked.title),
                );
            }
        }
        // The lease holder remains authoritative for owner cancellation,
        // heartbeat cancellation, revocation and shutdown across processes.
        let watch_client = client.clone();
        let watch_id = lease.id.clone();
        let watch_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! { _ = watch_cancel.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(200)) => {} }
                let response = watch_client
                    .call(&crate::command::commands::daemon::actions::Request {
                        method: "GET".into(),
                        path: format!("/v1/worker/leases/{watch_id}"),
                        query: vec![],
                        body: None,
                    })
                    .await;
                if response.is_err()
                    || response.as_ref().is_ok_and(|record| {
                        !record["ended_at"].is_null()
                            || record["state"] == "reporting"
                            || record["state"] == "lost"
                    })
                {
                    watch_cancel.cancel();
                    break;
                }
            }
        });
        Ok(Self {
            terminal,
            _terminal_stop: terminal_stop.drop_guard(),
            client,
            picked,
            bus,
            cancel,
            events: Some(events),
        })
    }
}

fn claim_path(item: &str) -> String {
    let mut url = reqwest::Url::parse("http://localhost/v1/worker/queue/").expect("constant URL");
    url.path_segments_mut()
        .expect("hierarchical URL")
        .pop_if_empty()
        .push(item)
        .push("claim");
    url.path().to_string()
}
