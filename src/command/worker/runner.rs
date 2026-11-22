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
pub(super) fn failed(stage: &str, code: &str, message: String) -> CommandResult {
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

pub(super) fn secret_redaction(
    secrets: &awman_wsp::ExecSecrets,
) -> crate::engine::auth::redaction::RedactionSet {
    let redaction = crate::engine::auth::redaction::RedactionSet::new();
    if let Some(values) = secrets.shared() {
        for value in values.values() {
            redaction.add(value.clone());
        }
    }
    redaction
}

/// Restart never re-executes a lease; a durable name marker identifies lost values.
pub(super) fn restart_release(record: &LeaseRecord) -> ReleaseRequest<CommandResult> {
    if record.secret_names.is_empty() {
        ReleaseRequest {
            context_changes: None,
            reason: ReleaseReason::Shutdown,
            message: Some("worker restarted".into()),
            result: None,
        }
    } else {
        let message = "the worker restarted and this lease's exec.secrets were held in memory only";
        ReleaseRequest {
            context_changes: None,
            reason: ReleaseReason::Error,
            message: Some(format!("credentials_lost: {message}")),
            result: Some(failed("preflight", "credentials_lost", message.into())),
        }
    }
}

/// Clear the shared push secret store and its names-only marker on every exit.
struct LeaseSecretsGuard {
    id: crate::data::ids::CommandId,
    ephemeral: Arc<crate::command::dispatch::ephemeral_secrets::EphemeralSecrets>,
    lifecycle: crate::data::api_lifecycle_store::CommandLifecycleStore,
}
impl Drop for LeaseSecretsGuard {
    fn drop(&mut self) {
        self.ephemeral.remove(&self.id);
        if let Err(error) = self.lifecycle.remove_secret_names(&self.id) {
            tracing::warn!(%error, "could not remove lease secret names");
        }
    }
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
        // WI 0145: the typed retry event, attributed to the push phase.
        self.0
            .with_source(crate::data::execution_event::EventSource::phase(
                crate::data::execution_event::EventSourcePhase::Push,
            ))
            .emit(retry.event());
    }
    fn message(&self, message: crate::data::message::UserMessage) {
        self.0.emit(EventPayload::StatusMessage {
            phase: "push".into(),
            message: message.text,
        });
    }
}
struct CollectionCancel(
    Arc<std::sync::atomic::AtomicBool>,
    tokio::task::JoinHandle<()>,
);
impl Drop for CollectionCancel {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        self.1.abort();
    }
}
/// Apply the admitted per-item config to the lease's private session and
/// return the fixed commit identity for the host's leftover commit.
///
/// `exec.workflows_dynamic` becomes the session's task config layer: above
/// the worker's and the repository's files, below env, `--set` and
/// `--leader`. The session belongs to this lease only, so nothing reaches
/// the next item or any config file. The identity is resolved by push's
/// policy merge over this session and the admitted `--prerequisite-*` flags;
/// `None` keeps the clone/host identity and the `awman <awman@localhost>`
/// fallback.
pub(super) fn apply_item_config(
    session: &mut Session,
    plan: &policy::LeasePlan,
) -> Result<Option<(String, String)>, crate::data::DataError> {
    if let Some(dynamic) = plan.workflows_dynamic.clone() {
        session.set_task_config(crate::data::config::ConfigFile {
            workflows: Some(crate::data::config::workflows::WorkflowsConfig {
                dynamic: Some(dynamic),
                ..Default::default()
            }),
            ..Default::default()
        })?;
    }
    // The same answer the run's own prerequisite flow injects into every
    // step container, so the leftover commit and the agents' commits agree.
    // An absent `exec.prerequisites` is no request overrides, not "no
    // identity": the worker's own policy still applies, as it does in-run.
    if plan.read_only_prompt {
        return Ok(None);
    }
    use crate::command::commands::workflow_prerequisites as prerequisites;
    let mut flags = plan.prerequisites.clone().unwrap_or_default();
    prerequisites::apply_identity_ceiling(
        crate::command::dispatch::catalogue::FrontendKind::WORKER_LOCAL,
        &mut flags,
        &prerequisites::remote_ceiling(session),
    );
    Ok(
        prerequisites::granted_fixed_identity(&prerequisites::policy_for(session, &flags, None))
            .map(|identity| (identity.name, identity.email)),
    )
}
fn command_scratch(
    paths: crate::data::fs::api_paths::ApiPaths,
    command: &crate::data::fs::api_db::CommandRecord,
) -> (crate::data::fs::api_paths::ApiPaths, String, String, u32) {
    (
        paths,
        command.session_id.clone(),
        command.id.clone(),
        command.attempt,
    )
}
fn collection_task_failed(result: CommandResult, repo_context_requested: bool) -> CommandResult {
    if repo_context_requested && matches!(result.outcome, None | Some(CommandStatusKind::Done)) {
        failed(
            "context",
            "context_invalid",
            "context collection could not complete".into(),
        )
    } else {
        result
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
    /// A fixed commit identity resolved from `exec.prerequisites`; `None`
    /// keeps the clone/host identity and the `awman <awman@localhost>` fallback.
    commit_identity: Option<(String, String)>,
    scratch: Option<(crate::data::fs::api_paths::ApiPaths, String, String, u32)>,
}
#[async_trait::async_trait]
impl LeasePostProcess for PushStage {
    async fn process(&self, mut result: CommandResult) -> CommandResult {
        let cancel = self.active.control.cancelled();
        // Collect before every terminal path and before any branch publication.
        // A lost epoch cannot publish either attachment.
        if self.active.control.may_report() {
            let active = self.active.clone();
            let scratch = self.scratch.clone();
            let env = self.env.clone();
            let item = self.lease.item.clone();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let fenced = active.control.fenced();
            let signal = stop.clone();
            let watcher = tokio::spawn(async move {
                fenced.cancelled().await;
                signal.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let stop_on_drop = CollectionCancel(stop.clone(), watcher);
            let fallback = result.clone();
            let repo_context_requested = self.lease.item.context.is_some();
            result = tokio::task::spawn_blocking(move || {
                let mut result = result;
                if let Some(stage) = active.context_stage.lock().unwrap().as_ref() {
                    let (updated, changes) = super::lease_context::collect(stage, result, &stop);
                    result = updated;
                    *active.context_changes.lock().unwrap() = changes;
                }
                if let Some((paths, session, job, attempt)) = scratch {
                    let attachment =
                        match crate::data::fs::context_dirs::ContextDirResolver::from_env(&env) {
                            Ok(resolver) => super::lease_workflow_context::collect(
                                &paths, &resolver, &item, &session, &job, attempt, &stop,
                            ),
                            Err(_) => item
                                .workflow_context
                                .as_ref()
                                .map(|_| super::lease_workflow_context::omitted()),
                        };
                    *active.workflow_context.lock().unwrap() = attachment;
                }
                result
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "context collection task failed");
                collection_task_failed(fallback, repo_context_requested)
            });
            drop(stop_on_drop);
        }
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
        let identity = self.commit_identity.clone();
        let commit = tokio::task::spawn_blocking(move || {
            commit_git.commit_leftovers_guarded(
                "Lease work",
                &mut crate::data::message::RecordingMessageSink::new(),
                &guard,
                identity
                    .as_ref()
                    .map(|(name, email)| (name.as_str(), email.as_str())),
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
                    sha: outcome.sha().unwrap_or_default().into(),
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
    pub(super) secrets: awman_wsp::ExecSecrets,
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
            secrets,
            owner,
        } = claimed;
        let redaction = secret_redaction(&secrets);
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
            context_stage: Default::default(),
            context_changes: Default::default(),
            workflow_context: Mutex::new(
                lease
                    .item
                    .workflow_context
                    .as_ref()
                    .map(|_| super::lease_workflow_context::omitted()),
            ),
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
                this.execute_lease(session.clone(), &lease, &mut record, plan, &secrets, active.clone(), interval).await
            })
            .catch_unwind()
            .await;
            let (mut result, _output_dir) = match execution {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    let (stage, code) = if let WorkerEngineError::Context { code, .. } = &error {
                        ("preflight", *code)
                    } else if matches!(error, WorkerEngineError::GitAuth { .. }) {
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
            redaction.redact_result(&mut result);
            let request = request.map(|(reason, message)| {
                (reason, message.map(|message| redaction.redact(&message)))
            });
            let outputs = active.outputs.lock().unwrap().take();
            record.state = LeaseState::Lost;
            if active.control.may_report() {
                let context_changes = active.context_changes.lock().unwrap().take();
                let workflow_context = active.workflow_context.lock().unwrap().take();
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
                                    context_changes,
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
                                    context_changes,
                                    workflow_context,
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
            let _ = this.cleanup_images().await;
            if let Err(error) = this.cleanup_retired() {
                tracing::warn!(%error, "deferred unlink cleanup failed");
            }
            revocation.abort();
            timeout_task.abort();
            // Shutdown waits on active tasks. Keep ownership through image
            // cleanup, which still uses the database and runtime; a successor
            // must not reopen that state while an ended lease is mutating it.
            this.active.lock().unwrap().remove(record.id.as_str());
            drop(done_guard);
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_lease(
        &self,
        session: Arc<WspSession>,
        lease: &Lease,
        record: &mut LeaseRecord,
        mut plan: policy::LeasePlan,
        secrets: &awman_wsp::ExecSecrets,
        active: Arc<Active>,
        interval: Duration,
    ) -> RunResult<(CommandResult, Option<PathBuf>)> {
        let resolver =
            crate::data::fs::context_dirs::ContextDirResolver::from_env(self.shared.sessions.env())
                .map_err(invalid)?;
        *active.context_stage.lock().unwrap() =
            super::lease_context::stage(&resolver, &record.id, &lease.item)?;
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
            crate::engine::error::EngineError::GitAuth {
                operation, remote, ..
            } => WorkerEngineError::GitAuth { operation, remote },
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
        if let Some(stage) = active.context_stage.lock().unwrap().as_ref() {
            run_session.set_repo_context(crate::data::session::RepoContextBinding::Staged(
                stage.dir().to_path_buf(),
            ));
        }
        run_session.set_inline_work_item(inline.path().to_path_buf());
        // Before provisioning, which reads the item's agent mapping.
        let commit_identity = apply_item_config(&mut run_session, &plan).map_err(invalid)?;
        provision::agents_for_lease(
            &self.shared.engines,
            &mut run_session,
            &lease.item,
            workspace.private_dir(),
            self.shared.store.clone(),
            record,
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
        let command_id = crate::data::ids::CommandId::parse(&job_id).map_err(invalid)?;
        let _secrets = LeaseSecretsGuard {
            id: command_id.clone(),
            ephemeral: self.shared.engines.ephemeral_secrets.clone(),
            lifecycle: self.shared.lifecycle_store.clone(),
        };
        if let Some(values) = secrets.shared() {
            let names: Vec<_> = values.names().collect();
            self.shared
                .lifecycle_store
                .record_secret_names(&command_id, &names)
                .map_err(invalid)?;
            self.shared
                .engines
                .ephemeral_secrets
                .insert(command_id, values);
        }
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
        engines.lease_workflow_review = plan.workflow_review.clone();
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
            commit_identity,
            scratch: Some(command_scratch(
                self.shared.paths.clone(),
                &self
                    .shared
                    .store
                    .get_command(&job_id)
                    .map_err(invalid)?
                    .ok_or_else(|| invalid("lease command is missing"))?,
            )),
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
        let _terminal_guard = terminal_stop.clone().drop_guard();
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
                use crate::engine::worker::terminal_relay::{
                    LeaseTerminalOpener, TerminalRelayOutcome,
                };
                let mut opener = LeaseTerminalOpener::new(session, reference);
                let result = tokio::select! {
                    biased;
                    _ = fence.cancelled().cancelled_owned() => return,
                    result = terminal_relay(shared, hub, owner, granted)
                        .run_reconnecting(&mut opener, stop) => result,
                };
                if matches!(
                    result,
                    TerminalRelayOutcome::LeaseLost | TerminalRelayOutcome::LinkRevoked
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
        commit_identity: None,
        scratch: None,
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
    // Stop terminal opening before the upstream result/release is polled.
    active.done.cancel();
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
                    context_changes: None,
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
                context_changes: None,
                workflow_context: None,
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

#[cfg(test)]
mod context_contract_tests {
    use super::*;
    use crate::data::fs::{
        api_command_log::CommandStateWriter, api_paths::ApiPaths, context_dirs::ContextDirResolver,
    };
    struct PendingApi;
    #[async_trait::async_trait]
    impl LeaseApi for PendingApi {
        async fn heartbeat(
            &self,
            _: &LeaseRef,
            _: &awman_wsp::HeartbeatRequest,
        ) -> RunResult<awman_wsp::HeartbeatResponse> {
            std::future::pending().await
        }
        async fn post_events(&self, _: &LeaseRef, events: &[ExecutionEvent]) -> RunResult<usize> {
            Ok(events.len())
        }
        async fn credentials(&self, _: &LeaseRef) -> RunResult<awman_wsp::Credentials> {
            Err(WorkerEngineError::LeaseLost)
        }
    }
    #[test]
    fn artifact_task_failure_keeps_the_outcome_but_repo_failure_prevents_push() {
        let done = CommandResult {
            outcome: Some(CommandStatusKind::Done),
            exit_code: Some(0),
            ..Default::default()
        };
        assert_eq!(collection_task_failed(done.clone(), false), done);
        assert_eq!(
            collection_task_failed(done, true).outcome,
            Some(CommandStatusKind::Error)
        );
        let original = failed("step", "fixture", "already failed".into());
        assert_eq!(collection_task_failed(original.clone(), true), original);
    }
    #[tokio::test]
    async fn post_process_uses_the_recorded_attempt_pin_and_fences_lost_epoch_artifacts() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component("command::worker::runner::post_process_uses_the_recorded_attempt_pin_and_fences_lost_epoch_artifacts"));
        let env = crate::data::config::env::EnvSnapshot::from_instance_env(
            sandbox
                .env()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
            sandbox.work().to_path_buf(),
            vec![],
        );
        let resolver = ContextDirResolver::from_env(&env).unwrap();
        let paths = ApiPaths::from_root(sandbox.work().join("api"));
        let store = crate::data::fs::api_db::SqliteSessionStore::open_from_paths(&paths).unwrap();
        store
            .insert_session(
                "session",
                sandbox.work().to_str().unwrap(),
                &Utc::now().to_rfc3339(),
            )
            .unwrap();
        store
            .enqueue_command("job", "session", "exec workflow", "[]", "unused")
            .unwrap();
        store
            .with_conn(|conn| {
                conn.execute("UPDATE commands SET attempt = 2 WHERE id = 'job'", [])?;
                Ok(())
            })
            .unwrap();
        let command = store.get_command("job").unwrap().unwrap();
        let bus = Arc::new(EventBus::for_attempt(32, command.attempt));
        let state = crate::data::workflow_state::WorkflowState::new(
            "notes".into(),
            &[],
            "hash".into(),
            None,
        );
        let dir = resolver.workflow_dir(state.invocation_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.md"), b"note").unwrap();
        let writer = CommandStateWriter::new(paths.clone(), "session", "job");
        super::super::events::persist_workflow_snapshot(&writer, &bus.sender(), &state);
        let mut item = awman_wsp::conformance::work_item("notes");
        item.workflow_context = Some(awman_wsp::ItemWorkflowContext { max_bytes: 1024 });
        let lease = Lease {
            id: "server-lease".into(),
            epoch: 1,
            expires_at: Utc::now() + chrono::Duration::minutes(5),
            item,
            terminal: false,
            remote_input: false,
        };
        let api: Arc<dyn LeaseApi> = Arc::new(PendingApi);
        let (control, heartbeat) = HeartbeatLoop::spawn(
            api.clone(),
            LeaseRef::of(&lease),
            Duration::from_secs(60),
            HeartbeatStatus::new(HeartbeatState::Running),
            lease.expires_at,
        );
        let active = Arc::new(Active {
            foreground_seen: Default::default(),
            started: AtomicBool::new(true),
            _owner: crate::data::fs::private_files::create_private_file_new(
                &sandbox.work().join("owner"),
            )
            .unwrap(),
            server: "test".into(),
            control: control.clone(),
            finish: Default::default(),
            outputs: Default::default(),
            context_stage: Default::default(),
            context_changes: Default::default(),
            workflow_context: Default::default(),
            done: CancellationToken::new(),
        });
        let workspace = Arc::new(
            LeaseWorkspace::prepare(
                &sandbox.work().join("workspaces"),
                &crate::data::ids::LeaseId::new(),
            )
            .unwrap(),
        );
        let post = PushStage {
            lease,
            workspace,
            transport: LeaseTransport::default(),
            api,
            active: active.clone(),
            interval: Duration::from_secs(60),
            env,
            bus: bus.clone(),
            output_dir: sandbox.work().join("output"),
            run_root: sandbox.work().to_path_buf(),
            git_source: None,
            commit_identity: None,
            scratch: Some(command_scratch(paths, &command)),
        };
        post.process(failed("step", "fixture", "already failed".into()))
            .await;
        let attachment = active.workflow_context.lock().unwrap().take().unwrap();
        assert!(!attachment.truncated);
        assert_eq!(attachment.items[0].path, "note.md");
        assert_eq!(
            attachment.items[0].content_base64.as_deref(),
            Some("bm90ZQ==")
        );
        control.fence();
        post.process(failed("step", "fixture", "already failed".into()))
            .await;
        assert!(
            active.workflow_context.lock().unwrap().is_none(),
            "lost epoch must skip collection"
        );
        let session = WspSession::connect("test", "https://unused.invalid", String::new()).unwrap();
        let polled = AtomicBool::new(false);
        let result = terminal_report(&control, &session, async {
            polled.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(WorkerEngineError::LeaseLost)));
        assert!(
            !polled.load(std::sync::atomic::Ordering::Relaxed),
            "lost epoch must never poll the report future"
        );
        control.stop();
        heartbeat.await.unwrap();
    }
}

#[cfg(test)]
mod lease_secret_tests {
    use super::*;

    fn lease() -> Lease {
        let mut item = awman_wsp::conformance::work_item("secret-lifetime");
        item.exec.secrets = awman_wsp::ExecSecrets::new([
            (
                "TOKEN".into(),
                crate::data::secret::SecretString::new("used-lease-secret"),
            ),
            (
                "UNUSED".into(),
                crate::data::secret::SecretString::new("unused-lease-secret"),
            ),
        ])
        .unwrap();
        Lease {
            id: "server-lease".into(),
            epoch: 1,
            expires_at: Utc::now() + chrono::Duration::minutes(1),
            item,
            terminal: false,
            remote_input: false,
        }
    }

    #[test]
    fn lease_record_strips_values_but_keeps_restart_marker_and_old_records_decode() {
        let lease = lease();
        let record = LeaseRecord::claimed("work", &lease, LeaseHost::Daemon, Utc::now());
        assert!(record.item.exec.secrets.is_empty());
        assert_eq!(record.secret_names, ["TOKEN", "UNUSED"]);
        assert!(!format!("{record:?}").contains("-lease-secret"));
        let wire = serde_json::to_string(&record).unwrap();
        assert!(!wire.contains("-lease-secret"));
        let recovery = restart_release(&record);
        assert_eq!(recovery.reason, ReleaseReason::Error);
        assert!(recovery.message.unwrap().starts_with("credentials_lost: "));
        let failure = recovery.result.unwrap().failure.unwrap();
        assert_eq!(
            (failure.stage.as_str(), failure.code.as_str()),
            ("preflight", "credentials_lost")
        );
        let mut old = serde_json::to_value(&record).unwrap();
        old.as_object_mut().unwrap().remove("secret_names");
        let old = serde_json::from_value::<LeaseRecord>(old).unwrap();
        assert!(old.secret_names.is_empty());
        assert_eq!(restart_release(&old).reason, ReleaseReason::Shutdown);
    }

    #[test]
    fn unused_values_are_never_injected_but_redact_results_and_release_messages() {
        let lease = lease();
        let redaction = secret_redaction(&lease.item.exec.secrets);
        let options = crate::engine::agent::AgentRunOptions {
            command_secrets: lease.item.exec.secrets.shared(),
            env_passthrough: Some(vec!["TOKEN".into()]),
            ..Default::default()
        };
        let env = options.secret_env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "TOKEN");
        let mut result = failed(
            "preflight",
            "setup_failed",
            "unused-lease-secret used-lease-secret".into(),
        );
        result.final_message = Some("unused-lease-secret".into());
        redaction.redact_result(&mut result);
        assert_eq!(result.final_message.as_deref(), Some("***"));
        assert_eq!(result.failure.unwrap().message, "*** ***");
        assert_eq!(
            redaction.redact("release unused-lease-secret"),
            "release ***"
        );
    }

    #[test]
    fn secret_guard_cleans_memory_and_lifecycle_markers_during_unwind() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component("command::worker::runner::lease_secret_tests::secret_guard_cleans_memory_and_lifecycle_markers_during_unwind"));
        let store = Arc::new(
            crate::data::fs::api_db::SqliteSessionStore::open(&sandbox.xdg_data()).unwrap(),
        );
        let lifecycle =
            crate::data::api_lifecycle_store::CommandLifecycleStore::open(store).unwrap();
        let memory =
            Arc::new(crate::command::dispatch::ephemeral_secrets::EphemeralSecrets::default());
        let id = crate::data::ids::CommandId::new();
        let values = lease().item.exec.secrets.shared().unwrap();
        lifecycle
            .record_secret_names(&id, &["TOKEN", "UNUSED"])
            .unwrap();
        memory.insert(id.clone(), values);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = LeaseSecretsGuard {
                id: id.clone(),
                ephemeral: memory.clone(),
                lifecycle: lifecycle.clone(),
            };
            assert!(memory
                .for_execution(&id, Some(&["TOKEN".into()]))
                .unwrap()
                .is_some());
            panic!("simulate runner unwind");
        }));
        assert!(result.is_err());
        assert!(memory.for_execution(&id, None).unwrap().is_none());
        assert!(lifecycle.secret_names(&id).unwrap().is_none());
    }
}

#[cfg(test)]
mod item_config_tests {
    use super::*;
    use crate::data::config::flags::{IdentityFlag, PrerequisiteFlags};

    fn plan(
        dynamic: Option<crate::data::config::workflows::DynamicConfig>,
        prerequisites: Option<PrerequisiteFlags>,
    ) -> policy::LeasePlan {
        policy::LeasePlan {
            workflow_review: None,
            interaction: crate::data::config::daemon::Interaction::Headless,
            command: vec!["exec".into(), "workflow".into()],
            args: vec![],
            prerequisites,
            read_only_prompt: false,
            workflows_dynamic: dynamic,
        }
    }

    #[test]
    fn per_item_dynamic_config_and_identity_stay_on_that_items_session() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(
            "command::worker::runner::item_config_tests::per_item_dynamic_config_and_identity_stay_on_that_items_session",
        ));
        let home = sandbox.root().join("data");
        let env = crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            home.to_string_lossy().into_owned(),
        )]);
        let global = crate::data::config::file::global_config_path(&env).unwrap();
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        let written = r#"{"workflows":{"dynamic":{"defaultLeader":"claude::opus","agentsToModels":{"claude":["opus"]},"guidance":["worker rule"]}}}"#;
        std::fs::write(&global, written).unwrap();
        let open = |name: &str| {
            let root = sandbox.root().join(name);
            std::fs::create_dir_all(&root).unwrap();
            Session::open_at_directory_root(
                root,
                SessionOpenOptions {
                    env: Some(env.clone()),
                    ..Default::default()
                },
            )
            .unwrap()
        };

        let mut first = open("first");
        let identity = apply_item_config(
            &mut first,
            &plan(
                Some(crate::data::config::workflows::DynamicConfig {
                    default_leader: Some("codex::gpt-5".into()),
                    agents_to_models: Some(
                        [("codex".to_string(), vec!["gpt-5".to_string()])].into(),
                    ),
                    guidance: Some(vec!["item rule".into()]),
                }),
                Some(PrerequisiteFlags {
                    identity: Some(IdentityFlag::Fixed {
                        name: "Lease Bot".into(),
                        email: "bot@example.test".into(),
                    }),
                    ..Default::default()
                }),
            ),
        )
        .unwrap();
        assert_eq!(
            identity,
            Some(("Lease Bot".to_string(), "bot@example.test".to_string()))
        );
        let resolved = first.effective_config().dynamic_workflows().unwrap();
        assert_eq!(resolved.default_leader.unwrap().value, "codex::gpt-5");
        let mapping = resolved.agents_to_models.unwrap().value;
        assert_eq!(mapping.keys().collect::<Vec<_>>(), ["codex"]);
        let guidance: Vec<_> = resolved.guidance.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(guidance, ["worker rule", "item rule"]);

        // The next item gets the worker's configuration back, and the
        // worker's file was never written.
        let mut next = open("next");
        assert_eq!(
            apply_item_config(&mut next, &plan(None, None)).unwrap(),
            None
        );
        let resolved = next.effective_config().dynamic_workflows().unwrap();
        assert_eq!(resolved.default_leader.unwrap().value, "claude::opus");
        assert_eq!(
            resolved
                .agents_to_models
                .unwrap()
                .value
                .keys()
                .collect::<Vec<_>>(),
            ["claude"]
        );
        assert_eq!(std::fs::read_to_string(&global).unwrap(), written);

        // Too much guidance after merging fails like the config key.
        let mut over = open("over");
        let error = apply_item_config(
            &mut over,
            &plan(
                Some(crate::data::config::workflows::DynamicConfig {
                    guidance: Some((0..50).map(|i| format!("rule {i}")).collect()),
                    ..Default::default()
                }),
                None,
            ),
        );
        assert!(error.is_err());
    }

    /// The leftover commit follows the worker's policy when the lease sends no
    /// `exec.prerequisites`, and a repository identity never lifts a worker deny.
    #[test]
    fn leftover_identity_follows_worker_policy_and_its_deny_beats_the_repo() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(
            "command::worker::runner::item_config_tests::leftover_identity_follows_worker_policy_and_its_deny_beats_the_repo",
        ));
        let run = |name: &str, global: &str, repo: Option<&str>, plan: &policy::LeasePlan| {
            let home = sandbox.root().join(name).join("data");
            let env = crate::data::config::env::EnvSnapshot::with_overrides([(
                "AWMAN_HOME",
                home.to_string_lossy().into_owned(),
            )]);
            let path = crate::data::config::file::global_config_path(&env).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, global).unwrap();
            let root = sandbox.root().join(name).join("clone");
            std::fs::create_dir_all(root.join(".awman")).unwrap();
            if let Some(repo) = repo {
                std::fs::write(root.join(".awman/config.json"), repo).unwrap();
            }
            let mut session = Session::open_at_git_root(
                root.clone(),
                root,
                SessionOpenOptions {
                    env: Some(env),
                    ..Default::default()
                },
            )
            .unwrap();
            apply_item_config(&mut session, plan).unwrap()
        };
        let allow_bot = r#"{"workflows":{"prerequisites":{"gitIdentity":{"use":"allow","name":"Bot","email":"bot@example.invalid"}}}}"#;
        let deny = r#"{"workflows":{"prerequisites":{"gitIdentity":{"use":"deny"}}}}"#;
        let repo_canary = r#"{"workflows":{"prerequisites":{"gitIdentity":{"use":"allow","name":"Review Canary","email":"review-canary@example.invalid"}}}}"#;

        // No wire object: the worker's own fixed identity, as the agents get.
        assert_eq!(
            run("inherit", allow_bot, None, &plan(None, None)),
            Some(("Bot".to_string(), "bot@example.invalid".to_string()))
        );
        // An empty wire object is the same request.
        assert_eq!(
            run(
                "empty",
                allow_bot,
                None,
                &plan(None, Some(PrerequisiteFlags::default()))
            ),
            Some(("Bot".to_string(), "bot@example.invalid".to_string()))
        );
        // A worker deny holds against the repository's own identity.
        assert_eq!(
            run("deny", deny, Some(repo_canary), &plan(None, None)),
            None
        );
        // A read-only prompt resolves nothing.
        let mut read_only = plan(None, None);
        read_only.read_only_prompt = true;
        assert_eq!(run("read-only", allow_bot, None, &read_only), None);
    }
}
