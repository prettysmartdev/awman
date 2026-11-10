//! A WSP mock server plus a reference lease run built from awman's Layer 1
//! worker primitives (WI 0143).
//!
//! [`Wsp`] serves `awman_wsp::mock::MockWorkServer` on loopback and links
//! workers to it through the real device-link flow, giving each a real
//! [`WspSession`] (bearer refresh, revocation latch).
//!
//! [`run_lease`] runs one claimed lease the way the pull worker composes the
//! engine pieces: heartbeat loop → owner-only lease workspace → lease
//! transport (credential helper) → [`LeaseCheckout`] (clone, setup fetch,
//! branch, merge refs) → the "agent" (a closure editing the clone) →
//! [`LeasePush`] with retries, expiry window and credential refresh → the
//! `CommandResult` posted as the WSP result, or discarded when the lease was
//! lost. Every step is product code; only the sequencing lives here.
//!
//! Included with `#[path = "helpers/wsp_lease.rs"] mod wsp_lease;` next to
//! `smart_http.rs`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use awman::data::fs::api_db::{CommandFailure, CommandResult, GitResult};
use awman::data::ids::LeaseId;
use awman::data::message::UserMessage;
use awman::engine::error::EngineError;
use awman::engine::git::lease_transport::{HelperProgram, LeaseUrlPolicy};
use awman::engine::git::push_retry::PushRetryPolicy;
use awman::engine::git::GitEngine;
use awman::engine::worker::credential_helper::{lease_transport, WspCredentialSource};
use awman::engine::worker::heartbeat::{
    HeartbeatControl, HeartbeatEnd, HeartbeatLoop, HeartbeatStatus,
};
use awman::engine::worker::lease_workspace::{LeaseCheckout, LeaseWorkspace};
use awman::engine::worker::push::{LeasePush, LeasePushOutcome, PushObserver, PushRetry};
use awman::engine::worker::{LeaseApi, LeaseRef, WspSession};
use awman_wsp::mock::{MockWorkServer, ServingMock};
use awman_wsp::{
    CredentialKind, Credentials, HeartbeatState, Lease, LeaseResult, RegisterRequest,
    RegisterResponse, WorkItem,
};
use chrono::Utc;
use tokio_util::sync::CancellationToken;

/// A served mock work server.
pub struct Wsp {
    pub mock: Arc<MockWorkServer>,
    pub serving: ServingMock,
}

/// One linked, registered worker.
pub struct Worker {
    pub session: Arc<WspSession>,
    pub refresh_token: String,
    pub registration: RegisterResponse,
}

impl Wsp {
    pub async fn start() -> Self {
        let mock = Arc::new(MockWorkServer::new());
        let serving = mock.clone().serve().await.expect("serve mock work server");
        Self { mock, serving }
    }

    pub fn base(&self) -> &str {
        &self.serving.base_url
    }

    /// Link through the device flow (approved with `server_labels`) and
    /// register with `capacity`, `features` and worker `labels`.
    pub async fn worker(
        &self,
        name: &str,
        capacity: usize,
        features: &[&str],
        labels: BTreeMap<String, String>,
        server_labels: BTreeMap<String, String>,
    ) -> Worker {
        let refresh_token = self.link(server_labels).await;
        let session = Arc::new(
            WspSession::connect("srv", self.base(), refresh_token.clone()).expect("connect"),
        );
        let registration = session
            .register(&RegisterRequest {
                name: name.into(),
                labels,
                capacity,
                features: features.iter().map(|f| f.to_string()).collect(),
            })
            .await
            .expect("register");
        Worker {
            session,
            refresh_token,
            registration,
        }
    }

    /// Run the device-link flow and return the refresh token.
    pub async fn link(&self, server_labels: BTreeMap<String, String>) -> String {
        let linker = awman_wsp::Client::new(self.base(), String::new()).unwrap();
        let start = linker.start_link().await.expect("start link");
        self.mock.approve_link(&start.link_id, server_labels);
        linker
            .poll_link(&start.link_id)
            .await
            .expect("poll link")
            .expect("approved")
            .refresh_token
    }

    pub fn enqueue(&self, item: WorkItem) {
        self.mock.enqueue(item, Utc::now()).unwrap();
    }
}

/// A work item for `repo_url` that leaves its result on `wsp/<id>`.
pub fn item(id: &str, repo_url: &str) -> WorkItem {
    let mut item = awman_wsp::conformance::work_item(id);
    item.repo.url = repo_url.into();
    item.repo.base_ref = "main".into();
    item.repo.branch = format!("wsp/{id}");
    item.body = format!("# Work Item: {id}\n\nDo the thing for {id}.\n");
    item
}

/// Bearer credentials for `token`.
pub fn bearer(token: &str, expires_in: Option<chrono::Duration>) -> Credentials {
    Credentials {
        kind: CredentialKind::Bearer,
        token: Some(token.into()),
        username: None,
        expires_at: expires_in.map(|d| Utc::now() + d),
    }
}

pub fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Claim the next eligible item (or `item_id`), panicking on errors.
pub async fn claim(session: &WspSession, item_id: Option<&str>) -> Option<Lease> {
    match session.claim(item_id, 0).await.expect("claim") {
        awman_wsp::ClaimOutcome::Leased(lease) => Some(lease),
        awman_wsp::ClaimOutcome::Empty => None,
    }
}

/// What the "agent" does inside the clone. Returns `Err(message)` to fail
/// the step.
pub type Agent = Box<dyn FnOnce(&Path) -> Result<(), String> + Send>;

/// An agent that writes `file` and commits it.
pub fn commit_file(file: &'static str, contents: &'static str) -> Agent {
    Box::new(move |clone: &Path| {
        std::fs::write(clone.join(file), contents).map_err(|e| e.to_string())?;
        super::smart_http::commit_all(clone, &format!("agent: {file}"));
        Ok(())
    })
}

/// How [`run_lease`] runs.
pub struct RunOptions {
    pub work_root: PathBuf,
    pub helper: HelperProgram,
    pub policy: LeaseUrlPolicy,
    pub heartbeat: Duration,
    /// `None`: the production policy for the lease expiry.
    pub push_policy: Option<PushRetryPolicy>,
    /// Hold the run in "agent" state until this token fires (cancel tests).
    pub hold_until: Option<CancellationToken>,
    /// Whether the runtime mounted `/awman/output` (`outputs_mounted`).
    pub outputs_mounted: bool,
}

impl RunOptions {
    pub fn new(work_root: &Path, helper: HelperProgram) -> Self {
        Self {
            work_root: work_root.to_path_buf(),
            helper,
            // Loopback http is allowed by the default policy; local paths are not.
            policy: LeaseUrlPolicy::default(),
            heartbeat: Duration::from_millis(300),
            push_policy: None,
            hold_until: None,
            outputs_mounted: false,
        }
    }
}

/// What happened to one lease.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub result: CommandResult,
    /// The result was posted to the server (false: discarded, lease lost).
    pub posted: bool,
    pub heartbeat_end: Option<HeartbeatEnd>,
    /// Every message the Git machinery and the push wrote (logs).
    pub messages: Vec<String>,
    pub retries: Vec<PushRetry>,
    pub clone_dir: PathBuf,
    pub started: Instant,
    pub finished: Instant,
}

#[derive(Default)]
struct Observer {
    retries: Mutex<Vec<PushRetry>>,
    messages: Mutex<Vec<String>>,
}

impl PushObserver for Observer {
    fn retry(&self, retry: &PushRetry) {
        let (_, message) = retry.status_message();
        self.messages.lock().unwrap().push(message);
        self.retries.lock().unwrap().push(retry.clone());
    }
    fn message(&self, message: UserMessage) {
        self.messages.lock().unwrap().push(message.text);
    }
}

fn failure(stage: &str, code: &str, message: impl Into<String>) -> CommandFailure {
    CommandFailure {
        stage: stage.into(),
        code: code.into(),
        message: message.into(),
        step: None,
    }
}

fn engine_failure(error: &EngineError) -> CommandFailure {
    use awman::data::fs::api_db::{failure_codes, failure_stages};
    match error {
        EngineError::GitAuth { .. } => failure(
            failure_stages::GIT_AUTH,
            failure_codes::GIT_AUTH_FAILED,
            error.to_string(),
        ),
        _ => failure(
            failure_stages::SETUP,
            failure_codes::SETUP_FAILED,
            error.to_string(),
        ),
    }
}

/// Run `lease` to a WSP result. See the module docs.
pub async fn run_lease(
    session: Arc<WspSession>,
    lease: Lease,
    agent: Agent,
    options: RunOptions,
) -> RunReport {
    use awman::data::execution_event::CommandStatusKind;
    let started = Instant::now();
    let lease_ref = LeaseRef::of(&lease);
    let api: Arc<dyn LeaseApi> = session.clone();
    let (control, beat) = HeartbeatLoop::spawn(
        api.clone(),
        lease_ref.clone(),
        options.heartbeat,
        HeartbeatStatus::new(HeartbeatState::Preparing),
        lease.expires_at,
    );
    let observer = Arc::new(Observer::default());
    let workspace =
        LeaseWorkspace::prepare(&options.work_root, &LeaseId::new()).expect("workspace");
    let clone_dir = workspace.clone_dir().to_path_buf();

    let mut result = CommandResult::default();
    let outcome: Result<String, CommandFailure> =
        async {
            let transport = lease_transport(
                &workspace,
                &lease.item,
                options.helper.clone(),
                options.policy,
            )
            .map_err(|e| failure("preflight", "invalid_lease", e.to_string()))?;
            // Clone + branch + merge refs (blocking Git, cancellable).
            let (workspace, transport) = {
                let repo = lease.item.repo.clone();
                let cancel = control.cancelled();
                let messages = observer.clone();
                let transport = transport.clone();
                tokio::task::spawn_blocking(move || {
                    let mut sink = awman::data::message::RecordingMessageSink::new();
                    let outcome = LeaseCheckout::new(&GitEngine::new(), &transport)
                        .run(&workspace, &repo, &mut sink, &cancel);
                    messages
                        .messages
                        .lock()
                        .unwrap()
                        .extend(sink.queued().iter().map(|m| m.text.clone()));
                    outcome.map(|_| (workspace, transport))
                })
                .await
                .unwrap()
                .map_err(|e| engine_failure(&e))?
            };
            control.set_state(HeartbeatState::Running);
            if let Some(hold) = &options.hold_until {
                let cancelled = control.cancelled();
                tokio::select! {
                    _ = hold.cancelled() => {}
                    _ = cancelled.cancelled() => {}
                }
            }
            if control.cancelled().is_cancelled() {
                return Err(failure(
                    "step",
                    "cancelled",
                    "the work server cancelled the lease",
                ));
            }
            let dir = workspace.clone_dir().to_path_buf();
            tokio::task::spawn_blocking(move || agent(&dir))
                .await
                .unwrap()
                .map_err(|m| failure("step", "step_failed", m))?;
            control.set_state(HeartbeatState::Pushing);
            let git = GitEngine::new()
                .isolate_remote_push(workspace.clone_dir(), &lease.item.repo.url)
                .map_err(|e| engine_failure(&e))?
                .with_lease_transport(transport.clone());
            let policy = options.push_policy.clone().unwrap_or_else(|| {
                PushRetryPolicy::for_lease(control.expires_at(), options.heartbeat)
            });
            let mut push = LeasePush::new(
                Arc::new(git),
                lease.item.repo.branch.clone(),
                policy,
                options.heartbeat,
            )
            .with_expiry(control.expiry_watch());
            if let Some(helper) = transport.helper() {
                let expires = lease
                    .item
                    .repo
                    .credentials
                    .as_ref()
                    .and_then(|c| c.expires_at);
                push = push.with_credentials(
                    helper.clone(),
                    Arc::new(WspCredentialSource::new(api.clone(), lease_ref.clone())),
                    expires,
                );
            }
            let pushed = push
                .run(&control.cancelled(), observer.as_ref())
                .await
                .map_err(|f| failure(f.stage(), f.code(), f.message.clone()))?;
            // Keep the workspace alive until the push finished.
            drop(workspace);
            Ok(match pushed {
                LeasePushOutcome::Pushed { sha, .. }
                | LeasePushOutcome::AlreadyPublished { sha } => sha,
            })
        }
        .await;

    match outcome {
        Ok(sha) => {
            result.exit_code = Some(0);
            result.outcome = Some(CommandStatusKind::Done);
            result.git = Some(GitResult {
                branch: Some(lease.item.repo.branch.clone()),
                head_sha: Some(sha),
                ..Default::default()
            });
        }
        Err(f) => {
            result.outcome = Some(if f.code == "cancelled" {
                CommandStatusKind::Cancelled
            } else {
                CommandStatusKind::Error
            });
            result.failure = Some(f);
        }
    }
    control.stop();
    let heartbeat_end = tokio::time::timeout(Duration::from_secs(10), beat)
        .await
        .ok()
        .and_then(Result::ok);
    let posted = report(
        &session,
        &control,
        &lease_ref,
        &result,
        options.outputs_mounted,
        started,
    )
    .await;
    let messages = observer.messages.lock().unwrap().clone();
    let retries = observer.retries.lock().unwrap().clone();
    RunReport {
        result,
        posted,
        heartbeat_end,
        messages,
        retries,
        clone_dir,
        started,
        finished: Instant::now(),
    }
}

/// Post the result unless the lease was lost.
async fn report(
    session: &WspSession,
    control: &HeartbeatControl,
    lease: &LeaseRef,
    result: &CommandResult,
    outputs_mounted: bool,
    started: Instant,
) -> bool {
    if !control.may_report() {
        return false;
    }
    session
        .result(
            lease,
            &LeaseResult {
                result: result.clone(),
                outputs: None,
                outputs_mounted,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        )
        .await
        .is_ok()
}

/// Wait until `cond` holds (polling), or panic after `timeout`.
pub async fn eventually(timeout: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
