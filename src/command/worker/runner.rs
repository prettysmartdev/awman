//! One lease, one heartbeat fence, one terminal result/release decision.
use super::*;
use crate::command::commands::api_server::{
    event_bus::{EventBus, EventBusSender},
    queue_worker::{LeasePostProcess, QueueWorker, QueueWorkerDeps},
};
use crate::data::{
    execution_event::{CommandStatusKind, EventPayload, ExecutionEvent},
    fs::api_db::{CommandFailure, CommandResult},
    session::{Session, SessionOpenOptions, SessionType},
    worker::{job_store::JobStore, JobSpec, JobWorkspace},
};
use crate::engine::{
    git::{lease_transport::LeaseTransport, push_retry::PushRetryPolicy, GitEngine},
    worker::{
        credential_helper::{lease_transport, LeaseUrlPolicy, WspCredentialSource},
        events::{EventForwarder, OrderedEventSource},
        heartbeat::{HeartbeatLoop, HeartbeatStatus},
        lease_workspace::{LeaseCheckout, LeaseWorkspace},
        push::{LeasePush, PushObserver, PushRetry},
        session::LeaseApi,
    },
};
use awman_wsp::{HeartbeatState, Lease, LeaseResult};
use std::path::{Path, PathBuf};

type RunResult<T> = Result<T, WorkerEngineError>;
fn invalid(error: impl std::fmt::Display) -> WorkerEngineError {
    WorkerEngineError::Invalid(error.to_string())
}
fn failed(stage: &str, code: &str, message: String) -> CommandResult {
    CommandResult {
        outcome: Some(CommandStatusKind::Error),
        exit_code: Some(1),
        failure: Some(CommandFailure {
            stage: stage.into(),
            code: code.into(),
            message,
            step: None,
        }),
        ..Default::default()
    }
}
fn cancelled(mut result: CommandResult) -> CommandResult {
    result.outcome = Some(CommandStatusKind::Cancelled);
    result.exit_code = Some(130);
    result.failure = Some(CommandFailure {
        stage: "step".into(),
        code: "cancelled".into(),
        message: "lease execution cancelled".into(),
        step: None,
    });
    result
}

/// Every post-claim operation uses the same fence. An event or credential
/// response reporting lease loss is as terminal as a heartbeat response.
struct FencedApi {
    session: Arc<WspSession>,
    control: HeartbeatControl,
}
impl FencedApi {
    fn check(&self) -> RunResult<()> {
        if self.control.may_report() && !self.session.is_revoked() {
            Ok(())
        } else {
            self.control.fence();
            Err(WorkerEngineError::LeaseLost)
        }
    }
    fn observe<T>(&self, result: RunResult<T>) -> RunResult<T> {
        if matches!(
            &result,
            Err(WorkerEngineError::LeaseLost | WorkerEngineError::LinkRevoked)
        ) {
            self.control.fence();
        }
        result
    }
}
#[async_trait::async_trait]
impl LeaseApi for FencedApi {
    async fn heartbeat(
        &self,
        lease: &LeaseRef,
        request: &awman_wsp::HeartbeatRequest,
    ) -> RunResult<awman_wsp::HeartbeatResponse> {
        self.check()?;
        self.observe(self.session.heartbeat(lease, request).await)
    }
    async fn post_events(&self, lease: &LeaseRef, events: &[ExecutionEvent]) -> RunResult<usize> {
        self.check()?;
        self.observe(self.session.post_events(lease, events).await)
    }
    async fn credentials(&self, lease: &LeaseRef) -> RunResult<awman_wsp::Credentials> {
        self.check()?;
        self.observe(self.session.credentials(lease).await)
    }
}
struct OrderedSource {
    rx: tokio::sync::mpsc::UnboundedReceiver<ExecutionEvent>,
    control: HeartbeatControl,
    done: bool,
}
#[async_trait::async_trait]
impl OrderedEventSource for OrderedSource {
    async fn next_batch(&mut self, max: usize) -> Option<Vec<ExecutionEvent>> {
        if self.done {
            return None;
        }
        let first = self.rx.recv().await?;
        let mut events = vec![first];
        while events.len() < max {
            let Some(last) = events.last() else {
                break;
            };
            if matches!(last.payload, EventPayload::Done) {
                self.done = true;
                break;
            }
            match self.rx.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }
        for event in &events {
            if let EventPayload::ProgramStatus { state, .. } = &event.payload {
                self.control.set_state(
                    if *state == crate::data::program_status::ProgramState::Blocked {
                        HeartbeatState::AwaitingInput
                    } else {
                        HeartbeatState::Running
                    },
                );
            }
        }
        if events
            .last()
            .is_some_and(|event| matches!(event.payload, EventPayload::Done))
        {
            self.done = true;
        }
        Some(events)
    }
}
struct Observer(EventBusSender);
impl PushObserver for Observer {
    fn retry(&self, retry: &PushRetry) {
        let (phase, message) = retry.status_message();
        self.0.emit(EventPayload::StatusMessage { phase, message });
    }
    fn message(&self, message: crate::data::message::UserMessage) {
        self.0.emit(EventPayload::StatusMessage {
            phase: "push".into(),
            message: message.text,
        });
    }
}
struct PushStage {
    lease: Lease,
    workspace: Arc<LeaseWorkspace>,
    transport: LeaseTransport,
    api: Arc<dyn LeaseApi>,
    active: Arc<Active>,
    interval: Duration,
    env: crate::data::config::env::EnvSnapshot,
    bus: Arc<EventBus>,
    output_dir: PathBuf,
    run_root: PathBuf,
    git_source: Option<crate::engine::git::remote_push::WorkflowGitSource>,
}
#[async_trait::async_trait]
impl LeasePostProcess for PushStage {
    async fn process(&self, mut result: CommandResult) -> CommandResult {
        let cancel = self.active.control.cancelled();
        if cancel.is_cancelled() {
            return cancelled(result);
        }
        if let Some(limit) = &self.lease.item.outputs {
            match collect_outputs(&self.output_dir, limit.max_bytes) {
                Ok(outputs) => *self.active.outputs.lock().unwrap() = Some(outputs),
                Err(error) => return failed("step", "outputs_failed", error),
            }
        }
        if cancel.is_cancelled() {
            return cancelled(result);
        }
        if result.outcome != Some(CommandStatusKind::Done) {
            return result;
        }
        let Some(push_branch) = self.lease.item.push_branch() else {
            return result;
        };
        self.active.control.set_state(HeartbeatState::Pushing);
        let git = GitEngine::with_env(self.env.for_lease_git());
        let transport = self.transport.clone();
        let root = self.run_root.clone();
        let source = self.git_source.clone();
        let url = self.lease.item.repo.url.clone();
        let isolated = tokio::task::spawn_blocking(move || {
            source
                .as_ref()
                .map_or_else(
                    || git.isolate_remote_push(&root, &url),
                    |source| git.isolate_workflow_transport(source, &url),
                )
                .map(|git| Arc::new(git.with_lease_transport(transport)))
        })
        .await;
        let git = match isolated {
            Ok(Ok(git)) => git,
            _ => {
                return failed(
                    "push",
                    "push_failed",
                    "could not isolate lease Git metadata".into(),
                )
            }
        };
        // The isolated Git index has its own info/exclude. Carry only the
        // host's private list of provisioned recipes into that metadata.
        let recipes = self.workspace.private_dir().join("generated-recipes");
        if recipes.exists() {
            let content =
                crate::data::fs::private_files::read_private_json::<Vec<String>>(&recipes)
                    .map(|recipes| recipes.unwrap_or_default().join("\n"));
            match content.and_then(|content| {
                crate::data::fs::config_file::write_config_file_within(
                    git.metadata_dir(),
                    &git.metadata_dir().join("info/exclude"),
                    &format!("{content}\n"),
                )
            }) {
                Ok(()) => {}
                Err(error) => return failed("push", "push_failed", error.to_string()),
            }
        }
        // Commit leftovers only through the isolated host metadata, while the
        // same cancellation/expiry guard bounds every subprocess.
        let commit_git = git.clone();
        let guard = crate::engine::git::remote_push::LeaseGitGuard {
            cancel: cancel.clone(),
            expiry: Some(self.active.control.expiry_watch()),
            deadline: self.active.control.expires_at(),
        };
        let commit = tokio::task::spawn_blocking(move || {
            commit_git.commit_leftovers_guarded(
                "Lease work",
                &mut crate::data::message::RecordingMessageSink::new(),
                &guard,
            )
        })
        .await;
        if !matches!(commit, Ok(Ok(()))) {
            return if cancel.is_cancelled() {
                cancelled(result)
            } else {
                failed("push", "push_failed", "could not commit lease work".into())
            };
        }
        let mut operation = LeasePush::new(
            git,
            push_branch,
            PushRetryPolicy::for_lease(self.active.control.expires_at(), self.interval),
            self.interval,
        )
        .with_expiry(self.active.control.expiry_watch());
        if let Some(helper) = self.transport.helper() {
            operation = operation.with_credentials(
                helper.clone(),
                Arc::new(WspCredentialSource::new(
                    self.api.clone(),
                    LeaseRef::of(&self.lease),
                )),
                self.lease
                    .item
                    .repo
                    .credentials
                    .as_ref()
                    .and_then(|c| c.expires_at),
            );
        }
        match operation.run(&cancel, &Observer(self.bus.sender())).await {
            Ok(outcome) => {
                result.pushed = Some(Some(crate::data::fs::api_db::PushedRef {
                    remote: "origin".into(),
                    branch: push_branch.to_string(),
                    sha: outcome.sha().into(),
                }))
            }
            Err(error) => {
                if matches!(
                    error.kind,
                    crate::engine::worker::push::LeasePushFailureKind::LeaseLost
                        | crate::engine::worker::push::LeasePushFailureKind::LinkRevoked
                ) {
                    self.active.control.fence();
                }
                if cancel.is_cancelled() {
                    return cancelled(result);
                }
                result.outcome = Some(CommandStatusKind::Error);
                result.exit_code = Some(1);
                result.failure = Some(CommandFailure {
                    stage: error.stage().into(),
                    code: error.code().into(),
                    message: error.message,
                    step: None,
                });
            }
        }
        if result.outcome == Some(CommandStatusKind::Done) && !cancel.is_cancelled() {
            let git = GitEngine::with_env(self.env.for_lease_git());
            let root = self.run_root.clone();
            let base = result.git.as_ref().and_then(|git| git.base_sha.clone());
            if let Ok(Ok(git_result)) =
                tokio::task::spawn_blocking(move || git.command_result(&root, base.as_deref()))
                    .await
            {
                result.git = git_result;
            }
        }
        result
    }
}

/// Closes a lease's private session when its run ends.
struct LeaseSessionGuard {
    store: Arc<crate::data::fs::api_db::SqliteSessionStore>,
    sessions: Arc<crate::data::session_manager::SessionManager>,
    key: String,
    id: crate::data::session::SessionId,
}
impl Drop for LeaseSessionGuard {
    fn drop(&mut self) {
        if let Err(error) = self
            .store
            .close_session(&self.key, &Utc::now().to_rfc3339())
        {
            tracing::warn!(%error, session = %self.key, "could not close lease session");
        }
        let _ = self.sessions.remove_by_key(&self.key, self.id);
    }
}

/// A claimed, persisted lease that is ready to run in the daemon.
pub(super) struct ClaimedLease {
    pub(super) lease: Lease,
    pub(super) record: LeaseRecord,
    pub(super) plan: policy::LeasePlan,
    /// Held for the life of the run; proves this process owns the lease.
    pub(super) owner: std::fs::File,
}

impl<F: ApiCommandFrontendFactory + 'static> PullController<F> {
    pub(super) fn start_lease(
        self: &Arc<Self>,
        session: Arc<WspSession>,
        claimed: ClaimedLease,
        permit: Option<WorkerPermit>,
        interval: Duration,
    ) {
        let ClaimedLease {
            lease,
            record,
            plan,
            owner,
        } = claimed;
        let (control, heartbeat) = HeartbeatLoop::spawn(
            session.clone(),
            LeaseRef::of(&lease),
            interval,
            HeartbeatStatus::new(HeartbeatState::Preparing),
            lease.expires_at,
        );
        let active = Arc::new(Active {
            foreground_seen: Default::default(),
            started: AtomicBool::new(true),
            _owner: owner,
            server: record.server.clone(),
            control,
            finish: Mutex::new(Finish::default()),
            outputs: Mutex::new(None),
            done: CancellationToken::new(),
        });
        self.active
            .lock()
            .unwrap()
            .insert(record.id.to_string(), active.clone());
        let this = self.clone();
        tokio::spawn(async move {
            let done_guard = active.done.clone().drop_guard();
            let monitor_active = active.clone();
            let monitor_session = session.clone();
            let monitor_done = active.done.clone();
            let revocation = tokio::spawn(async move {
                tokio::select! { _ = monitor_session.revoked().cancelled_owned() => monitor_active.control.fence(), _ = monitor_done.cancelled() => {} }
            });
            let timeout_done = active.done.clone();
            let timeout_control = active.control.clone();
            let timeout_ms = lease.item.exec.timeout_ms;
            let timeout_task = tokio::spawn(async move {
                if let Some(ms) = timeout_ms {
                    tokio::select! { _ = timeout_done.cancelled() => {}, _ = tokio::time::sleep(Duration::from_millis(ms)) => timeout_control.cancelled().cancel() }
                }
            });
            let started = std::time::Instant::now();
            let mut record = record;
            use futures_util::FutureExt as _;
            let execution = std::panic::AssertUnwindSafe(async {
                let cancelled = active.control.cancelled();
                let _permit = match permit {
                    Some(permit) => permit,
                    None => tokio::select! {
                        biased;
                        _ = cancelled.cancelled() => return Err(WorkerEngineError::Invalid("lease cancelled while waiting for execution capacity".into())),
                        permit = this.share.acquire(crate::data::worker::JobClass::Pull) => permit,
                    },
                };
                this.execute_lease(session.clone(), &lease, &mut record, plan, active.clone(), interval).await
            })
            .catch_unwind()
            .await;
            let (mut result, _output_dir) = match execution {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    let (stage, code) = if error == WorkerEngineError::GitAuth {
                        ("git_auth", "git_auth_failed")
                    } else if matches!(error, WorkerEngineError::Invalid(_)) {
                        ("preflight", "setup_failed")
                    } else {
                        ("setup", "setup_failed")
                    };
                    (failed(stage, code, error.to_string()), None)
                }
                Err(_) => (
                    failed("step", "internal", "lease execution task panicked".into()),
                    None,
                ),
            };
            if active.control.cancelled().is_cancelled() {
                result = cancelled(result);
            }
            let request = {
                let mut finish = active.finish.lock().unwrap();
                finish.reporting = true;
                finish.release.clone()
            };
            if session.is_revoked() {
                active.control.fence();
            }
            let outputs = active.outputs.lock().unwrap().take();
            record.state = LeaseState::Lost;
            if active.control.may_report() {
                record.state = LeaseState::Reporting;
                record.expires_at = active.control.expires_at();
                let _ = this.leases.update(&record);
                let message = match request {
                    Some((reason, message)) => {
                        let reply = terminal_report(
                            &active.control,
                            &session,
                            session.release(
                                &LeaseRef::of(&lease),
                                &ReleaseRequest {
                                    reason,
                                    message: message.clone(),
                                    result: Some(result.clone()),
                                },
                            ),
                        )
                        .await;
                        if reply.is_ok() {
                            record.state = LeaseState::Released;
                            record.release = Some(LeaseRelease { reason, message });
                        }
                        reply
                    }
                    None => {
                        let reply = terminal_report(
                            &active.control,
                            &session,
                            session.result(
                                &LeaseRef::of(&lease),
                                &LeaseResult {
                                    result: result.clone(),
                                    outputs,
                                    outputs_mounted: record
                                        .job_id
                                        .as_deref()
                                        .and_then(|id| {
                                            JobStore::open(this.shared.store.clone())
                                                .ok()?
                                                .outputs_mounted(id)
                                                .ok()
                                                .flatten()
                                        })
                                        .unwrap_or(false),
                                    duration_ms: started
                                        .elapsed()
                                        .as_millis()
                                        .min(u128::from(u64::MAX))
                                        as u64,
                                },
                            ),
                        )
                        .await;
                        if reply.is_ok() {
                            record.state = LeaseState::Done;
                        }
                        reply
                    }
                };
                if let Err(error) = message {
                    tracing::warn!(%error, lease = %record.id, "lease terminal report failed; never re-executing");
                    record.state = LeaseState::Lost;
                }
            }
            active.control.stop();
            let _ = heartbeat.await;
            record.expires_at = active.control.expires_at();
            record.ended_at = Some(Utc::now());
            if let Err(error) = this.leases.update(&record) {
                tracing::error!(%error, "could not persist ended lease");
            }
            if let Some(job) = &record.job_id {
                this.shared.command_cancels.unregister(job);
            }
            this.active.lock().unwrap().remove(record.id.as_str());
            if let Err(error) = this.cleanup_retired() {
                tracing::warn!(%error, "deferred unlink cleanup failed");
            }
            revocation.abort();
            timeout_task.abort();
            drop(done_guard);
        });
    }

    async fn execute_lease(
        &self,
        session: Arc<WspSession>,
        lease: &Lease,
        record: &mut LeaseRecord,
        mut plan: policy::LeasePlan,
        active: Arc<Active>,
        interval: Duration,
    ) -> RunResult<(CommandResult, Option<PathBuf>)> {
        let root = crate::data::worker::paths::work_root(&self.data_paths(), &self.settings());
        let workspace = Arc::new(LeaseWorkspace::prepare(&root, &record.id)?);
        let transport = lease_transport(
            &workspace,
            &lease.item,
            self.helper.clone(),
            LeaseUrlPolicy::default(),
        )?;
        let inline = workspace.inline_work_item(&lease.item.id, &lease.item.body)?;
        let workflow_file = if let Some(toml) = &lease.item.exec.workflow_toml {
            let path = workspace.private_dir().join("workflow.toml");
            use std::io::Write as _;
            crate::data::fs::private_files::create_private_file_new(&path)
                .map_err(invalid)?
                .write_all(toml.as_bytes())
                .map_err(invalid)?;
            for arg in &mut plan.args {
                if arg == "LEASE_WORKFLOW_FILE" {
                    *arg = path.to_string_lossy().into();
                }
            }
            Some(path)
        } else {
            None
        };
        let git = Arc::new(GitEngine::with_env(
            self.shared.sessions.env().for_lease_git(),
        ));
        let clone_ws = workspace.clone();
        let clone_transport = transport.clone();
        let clone_repo = lease.item.repo.clone();
        let clone_git = git.clone();
        let cancel = active.control.cancelled();
        tokio::task::spawn_blocking(move || {
            LeaseCheckout::new(&clone_git, &clone_transport).run(
                &clone_ws,
                &clone_repo,
                &mut crate::data::message::RecordingMessageSink::new(),
                &cancel,
            )
        })
        .await
        .map_err(invalid)?
        .map_err(|error| match error {
            crate::engine::error::EngineError::GitAuth { .. } => WorkerEngineError::GitAuth,
            error => invalid(error),
        })?;
        if active.control.cancelled().is_cancelled() {
            return Err(WorkerEngineError::LeaseLost);
        }
        let cwd = workspace.clone_dir().to_path_buf();
        let mut run_session = Session::open_at_git_root(
            cwd.clone(),
            cwd.clone(),
            SessionOpenOptions {
                env: Some(self.shared.sessions.env().clone()),
                ..Default::default()
            },
        )
        .map_err(invalid)?;
        run_session.set_session_type(SessionType::Remote {
            repo_url: lease.item.repo.url.clone(),
            branch: lease.item.repo.branch.clone(),
            cloned_path: cwd.clone(),
        });
        run_session.set_inline_work_item(inline.path().to_path_buf());
        provision::agents_for_lease(
            &self.shared.engines,
            &run_session,
            &lease.item,
            workspace.private_dir(),
        )
        .await?;
        let session_key = crate::data::ids::WorkerSessionId::new().to_string();
        self.shared
            .store
            .insert_session_full(crate::data::fs::api_db::NewSessionRow {
                id: &session_key,
                workdir: &cwd.to_string_lossy(),
                created_at: &Utc::now().to_rfc3339(),
                setup_status: crate::data::session_setup_event::SessionSetupStatus::Ready,
                kind: crate::data::session::SessionKind::Remote,
                cloned_path: Some(&cwd.to_string_lossy()),
            })
            .map_err(invalid)?;
        let run_session_id = run_session.id();
        self.shared
            .sessions
            .insert_with_key(&session_key, run_session)
            .map_err(invalid)?;
        // The session exists only for this lease. Close it on every exit path
        // so it stops accepting commands and is not listed as active once its
        // clone is gone; the command record and outputs stay readable.
        let _session = LeaseSessionGuard {
            store: self.shared.store.clone(),
            sessions: self.shared.sessions.clone(),
            key: session_key.clone(),
            id: run_session_id,
        };
        let job_id = crate::data::ids::CommandId::new().to_string();
        let spec = JobSpec {
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
        };
        let jobs = JobStore::open(self.shared.store.clone()).map_err(invalid)?;
        jobs.insert(&job_id, &record.origin(), &spec, &Utc::now().to_rfc3339())
            .map_err(invalid)?;
        // Persist the lease grant. The relay and HTTP terminal adapters check
        // the daemon's live policy on each input; freezing it here would make
        // a later remoteInput enable ineffective for an already-running job.
        let input = lease.remote_input;
        // Persist the recovery link before any executable command row exists.
        // A crash between these writes can leave metadata without a row, but
        // never a row that generic push recovery could execute without a lease.
        record.job_id = Some(job_id.clone());
        record.state = LeaseState::Preparing;
        self.leases.update(record).map_err(invalid)?;
        let job_cancel = self.shared.command_cancels.register(&job_id);
        self.shared
            .store
            .insert_running_lease_command(
                &job_id,
                &session_key,
                &plan.command.join(" "),
                &serde_json::to_string(&plan.args).map_err(invalid)?,
                input,
            )
            .map_err(invalid)?;
        record.state = LeaseState::Running;
        self.leases.update(record).map_err(invalid)?;
        active.control.set_state(HeartbeatState::Running);
        let mut engines = self.shared.engines.clone();
        engines.git_engine = git;
        engines.agent_engine = Arc::new(
            (*engines.agent_engine)
                .clone()
                .with_read_only_workspace(lease.item.exec.read_only)
                .with_lease_workspace_scope(cwd),
        );
        let bus =
            Arc::new(EventBus::new(4096).with_awaiting_input(active.control.awaiting_input()));
        let api: Arc<dyn LeaseApi> = Arc::new(FencedApi {
            session: session.clone(),
            control: active.control.clone(),
        });
        let source = OrderedSource {
            rx: bus.subscribe_ordered(),
            control: active.control.clone(),
            done: false,
        };
        let event_control = active.control.clone();
        let event_stop = active.done.child_token();
        let stop_events = event_stop.clone();
        let event_api = api.clone();
        let reference = LeaseRef::of(lease);
        let events = tokio::spawn(async move {
            let result =
                EventForwarder::run(event_api, reference, Box::new(source), event_stop).await;
            if matches!(
                result,
                Err(WorkerEngineError::LeaseLost | WorkerEngineError::LinkRevoked)
            ) {
                event_control.fence();
            }
            result
        });
        let post = Arc::new(PushStage {
            lease: lease.clone(),
            workspace: workspace.clone(),
            transport,
            api,
            active: active.clone(),
            interval,
            env: self.shared.sessions.env().clone(),
            bus: bus.clone(),
            output_dir: self.shared.paths.command_output_dir(&session_key, &job_id),
            run_root: workspace.clone_dir().into(),
            git_source: None,
        });
        let worker = QueueWorker::new(
            "pull".into(),
            QueueWorkerDeps {
                store: self.shared.store.clone(),
                engines,
                sessions: self.shared.sessions.clone(),
                event_buses: self.shared.event_buses.clone(),
                paths: self.shared.paths.clone(),
                frontend_factory: self.factory.clone(),
                lifecycle: self.shared.lifecycle(),
                lifecycle_store: self.shared.lifecycle_store.clone(),
                cancels: self.shared.command_cancels.clone(),
            },
        )
        .with_daemon_settings(live_daemon_settings(self.shared.clone()))
        .with_lease_events(bus)
        .with_lease_post_process(post);
        let command = self
            .shared
            .store
            .get_command(&job_id)
            .map_err(invalid)?
            .ok_or_else(|| invalid("lease command disappeared"))?;
        let cancel = active.control.cancelled();
        let timeout = lease.item.exec.timeout_ms;
        let execute_cancel = job_cancel.clone();
        let execution = worker.execute_lease(command, execute_cancel);
        tokio::pin!(execution);
        let terminal_stop = active.done.child_token();
        let terminal = if lease.terminal {
            let session = session.clone();
            let reference = LeaseRef::of(lease);
            let hub = self.hub.clone();
            let owner = crate::data::terminal::TerminalOwner::Command(
                crate::data::ids::CommandId::parse(&job_id).map_err(invalid)?,
            );
            let stop = terminal_stop.clone();
            let shared = self.shared.clone();
            let granted = lease.remote_input;
            let fence = active.control.clone();
            Some(tokio::spawn(async move {
                let socket = tokio::select! { biased; _ = stop.cancelled() => return, socket = session.open_terminal(&reference) => socket };
                let result = match socket {
                    Ok(mut socket) => {
                        terminal_relay(shared, hub, owner, granted)
                            .run(&mut socket, stop)
                            .await
                    }
                    Err(error) => Err(error),
                };
                if matches!(
                    result,
                    Err(WorkerEngineError::LeaseLost | WorkerEngineError::LinkRevoked)
                ) {
                    fence.fence();
                }
            }))
        } else {
            None
        };
        tokio::select! {
            _ = &mut execution => {},
            _ = cancel.cancelled() => { job_cancel.cancel(); let _ = crate::command::commands::api_server::owned_runtime::stop_command_containers(self.shared.engines.runtime.as_ref(), &job_id); execution.await; },
            _ = job_cancel.cancelled() => {
                active.finish.lock().unwrap().release.get_or_insert((
                    if self.core_shutdown.is_cancelled() { ReleaseReason::Shutdown } else { ReleaseReason::Error }, Some("worker job cancelled by its owner".into()),
                ));
                cancel.cancel(); execution.await;
            },
            _ = async { if let Some(ms) = timeout { tokio::time::sleep(Duration::from_millis(ms)).await; } else { std::future::pending::<()>().await; } } => { cancel.cancel(); job_cancel.cancel(); let _ = crate::command::commands::api_server::owned_runtime::stop_command_containers(self.shared.engines.runtime.as_ref(), &job_id); execution.await; },
        }
        terminal_stop.cancel();
        if let Some(terminal) = terminal {
            let _ = terminal.await;
        }
        // Final events precede the terminal report; expiry/revocation still
        // fence this wait and all outgoing batches.
        let _ = drain_events(events, &active.control).await;
        stop_events.cancel();
        let result = self
            .shared
            .store
            .get_command(&job_id)
            .map_err(invalid)?
            .and_then(|row| row.result)
            .and_then(|json| serde_json::from_str(&json).ok())
            .ok_or_else(|| invalid("lease command has no result"))?;
        let output = self.shared.paths.command_output_dir(&session_key, &job_id);
        drop(workflow_file);
        drop(inline);
        Ok((result, Some(output)))
    }
}

pub(super) fn terminal_relay(
    shared: Arc<crate::command::daemon_runtime::features::worker::WorkerShared>,
    hub: Arc<crate::engine::terminal_hub::TerminalHub>,
    owner: crate::data::terminal::TerminalOwner,
    lease_grants_input: bool,
) -> crate::engine::worker::terminal_relay::TerminalRelay {
    crate::engine::worker::terminal_relay::TerminalRelay::new(hub, owner, lease_grants_input)
        .with_input_policy(Arc::new(move || {
            live_daemon_settings(shared.clone())().terminal.remote_input
        }))
}

/// Inline the output directory up to `max_bytes`. A file that does not fit in
/// the remaining budget is left out; it never fails the run.
fn collect_outputs(root: &Path, max_bytes: u64) -> Result<BTreeMap<String, String>, String> {
    use base64::Engine as _;
    use std::io::Read as _;
    let max = max_bytes.min(32 * 1024 * 1024);
    let listing = crate::data::fs::command_outputs::list_output_dir(
        root,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|_| "could not collect lease outputs".to_string())?;
    let mut remaining = max;
    let mut outputs = BTreeMap::new();
    for file in listing.files {
        if file.size > remaining {
            continue;
        }
        let opened = crate::data::fs::command_outputs::open_output_file(root, &file.path)
            .map_err(|_| "unsafe output path".to_string())?
            .ok_or_else(|| "output disappeared".to_string())?;
        let mut bytes = Vec::new();
        opened
            .file
            .take(remaining.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| "could not read output".to_string())?;
        if bytes.len() as u64 > remaining {
            continue;
        }
        remaining -= bytes.len() as u64;
        outputs.insert(
            file.path,
            base64::engine::general_purpose::STANDARD.encode(bytes),
        );
    }
    Ok(outputs)
}

async fn terminal_report(
    control: &HeartbeatControl,
    session: &WspSession,
    report: impl std::future::Future<Output = RunResult<()>>,
) -> RunResult<()> {
    if !control.may_report() || session.is_revoked() {
        control.fence();
        return Err(WorkerEngineError::LeaseLost);
    }
    tokio::select! {
        biased;
        _ = async { loop { if !control.may_report() || session.is_revoked() { control.fence(); break; } tokio::time::sleep(Duration::from_millis(10)).await; } } => Err(WorkerEngineError::LeaseLost),
        result = report => { if !control.may_report() || session.is_revoked() { control.fence(); Err(WorkerEngineError::LeaseLost) } else { result } },
    }
}

pub(super) fn foreground_events(
    session: Arc<WspSession>,
    lease: &Lease,
    control: HeartbeatControl,
    bus: &EventBus,
    stop: CancellationToken,
) -> tokio::task::JoinHandle<RunResult<u64>> {
    let source = OrderedSource {
        rx: bus.subscribe_ordered(),
        control: control.clone(),
        done: false,
    };
    let api = Arc::new(FencedApi {
        session,
        control: control.clone(),
    });
    let reference = LeaseRef::of(lease);
    tokio::spawn(async move {
        let result = EventForwarder::run(api, reference, Box::new(source), stop).await;
        if matches!(
            result,
            Err(WorkerEngineError::LeaseLost | WorkerEngineError::LinkRevoked)
        ) {
            control.fence();
        }
        result
    })
}

/// Ordinary cancellation still owes its final persisted events. A fence or
/// the last confirmed expiry bounds this wait, including after cancel=true.
pub(super) async fn drain_events(
    mut events: tokio::task::JoinHandle<RunResult<u64>>,
    control: &HeartbeatControl,
) -> Result<(), String> {
    let mut expiry = control.expiry_watch();
    let fenced = control.fenced();
    loop {
        let remaining = (*expiry.borrow_and_update() - Utc::now())
            .to_std()
            .unwrap_or_default();
        tokio::select! {
            biased;
            _ = fenced.cancelled() => break,
            _ = tokio::time::sleep(remaining) => { control.fence(); break; },
            result = &mut events => return result.map_err(|e| e.to_string())?.map(|_| ()).map_err(|e| e.to_string()),
            changed = expiry.changed() => {
                if changed.is_err() {
                    tokio::select! {
                        biased;
                        _ = fenced.cancelled() => {},
                        _ = tokio::time::sleep(remaining) => control.fence(),
                        result = &mut events => return result.map_err(|e| e.to_string())?.map(|_| ()).map_err(|e| e.to_string()),
                    }
                    break;
                }
            },
        }
    }
    events.abort();
    let _ = events.await;
    Err("lease lost while draining final events".into())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_foreground(
    engines: &crate::command::dispatch::Engines,
    session: Arc<WspSession>,
    lease: Lease,
    workspace: Arc<LeaseWorkspace>,
    transport: LeaseTransport,
    active: Arc<Active>,
    interval: Duration,
    bus: Arc<EventBus>,
    output_dir: PathBuf,
    env: crate::data::config::env::EnvSnapshot,
    duration: Duration,
    events: &mut Option<tokio::task::JoinHandle<RunResult<u64>>>,
    result: CommandResult,
    run_root: PathBuf,
    git_source: crate::engine::git::remote_push::WorkflowGitSource,
    outputs_mounted: bool,
    store: Arc<crate::data::fs::api_db::SqliteSessionStore>,
    job_id: String,
) -> Result<CommandResult, String> {
    let api = Arc::new(FencedApi {
        session: session.clone(),
        control: active.control.clone(),
    });
    let stage = PushStage {
        lease: lease.clone(),
        workspace,
        transport,
        api,
        active: active.clone(),
        interval,
        env,
        bus: bus.clone(),
        output_dir,
        run_root,
        git_source: Some(git_source),
    };
    let result = stage.process(result).await;
    let result = if active.control.cancelled().is_cancelled() {
        cancelled(result)
    } else {
        result
    };
    let status = match result.outcome {
        Some(CommandStatusKind::Done) => crate::data::command_status::CommandStatus::Done,
        Some(CommandStatusKind::Cancelled) => crate::data::command_status::CommandStatus::Cancelled,
        _ => crate::data::command_status::CommandStatus::Error,
    };
    store
        .complete_command(
            &job_id,
            status,
            result.exit_code,
            Some(&serde_json::to_string(&result).map_err(|e| e.to_string())?),
        )
        .map_err(|e| e.to_string())?;
    JobStore::open(store)
        .and_then(|store| {
            store.set_outputs_mounted(
                &job_id,
                outputs_mounted && engines.runtime.capabilities().mounts_outputs(true),
            )
        })
        .map_err(|e| e.to_string())?;
    bus.sender().emit(EventPayload::CommandStatus {
        status: result.outcome.unwrap_or(CommandStatusKind::Error),
        exit_code: result.exit_code,
        error: result.error.clone(),
        result: Some(Box::new(result.clone())),
    });
    bus.sender().emit(EventPayload::Done);
    if let Some(events) = events.take() {
        drain_events(events, &active.control).await?;
    }
    if !active.control.may_report() {
        return Err("lease lost; result discarded".into());
    }
    let release = active.finish.lock().unwrap().release.clone();
    if let Some((reason, message)) = release {
        terminal_report(
            &active.control,
            &session,
            session.release(
                &LeaseRef::of(&lease),
                &ReleaseRequest {
                    reason,
                    message,
                    result: Some(result.clone()),
                },
            ),
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(result);
    }
    let outputs = active.outputs.lock().unwrap().take();
    terminal_report(
        &active.control,
        &session,
        session.result(
            &LeaseRef::of(&lease),
            &LeaseResult {
                result: result.clone(),
                outputs,
                outputs_mounted: outputs_mounted
                    && engines.runtime.capabilities().mounts_outputs(true),
                duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
            },
        ),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(result)
}
