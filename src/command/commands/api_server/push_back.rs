//! Host-side push-back of a remote session's clone after successful execution.
//!
//! Layer 2 policy for push sessions: the leftover commit and local checks
//! run as before; publication is the shared [`PushPublisher`] (head check,
//! classification, bounded retry) with the session's transport, the push
//! session [`PushRetryPolicy::for_command`] and no lease gate. Each retry is
//! a typed `push_retry` event on the command's bus. A [`PushFailure`] maps
//! to `failure.{stage,code}`: `git_auth`/`git_auth_failed`,
//! `push`/`push_unreachable` (retries exhausted), `push`/`cancelled`, or
//! `push`/`push_failed` (the remote answered and refused, or a local error).
//!
//! `close_on_finish` ordering: the queue worker awaits push-back (this whole
//! retry loop) before it builds and publishes the terminal result, and runs
//! `post_execution_check` (which may close the session and delete the clone)
//! only after that publication.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::event_bus::EventBusSender;
use crate::command::commands::exec_workflow::parse_work_item_number;
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::data::execution_event::{EventPayload, EventSource, EventSourcePhase};
use crate::data::fs::api_db::{failure_codes, failure_stages, PushedRef, SessionRecord};
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::error::EngineError;
use crate::engine::git::push_publish::{
    NoGate, PushFailure, PushObserver, PushOutcome, PushPublisher, PushRetry,
};
use crate::engine::git::push_retry::PushRetryPolicy;
use crate::engine::git::transport::HostGitTransport;
use crate::engine::git::{GitEngine, GitFrontend};

/// Capture publication state before execution; a workflow may push for itself.
pub(crate) struct PushBaseline {
    head: String,
    origin_sha: Option<String>,
    unpublished: bool,
}

/// The pre-command baseline: local head and unpublished state, and the
/// remote branch head through the same publisher (`verify_only`), so the
/// remote check has the push session's transport, retry policy, retry
/// events and cancellation (WI 0145 F1).
pub(crate) async fn capture_baseline(
    git: GitEngine,
    session: SessionRecord,
    ctx: PushBackContext,
    sink: PushEventSink,
) -> Result<Option<PushBaseline>, PushBackError> {
    if session.session_type != "remote" || !session.push {
        return Ok(None);
    }
    let (Some(branch), Some(clone), Some(url)) = (
        session.branch.clone(),
        session.cloned_path.clone(),
        session.repo_url.clone(),
    ) else {
        return Ok(None);
    };
    let PushBackContext {
        cancel,
        deadline,
        transport,
    } = ctx;
    let mut local_guard = crate::engine::git::transport::TransportGuard::new(cancel.clone());
    local_guard.deadline = deadline;
    local_guard.timeout = crate::engine::git::push_retry::PUSH_FIRST_ATTEMPT_TIMEOUT;
    let local = tokio::task::spawn_blocking(move || {
        let git =
            git.isolate_remote_push_guarded(Path::new(&clone), &url, transport, local_guard)?;
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let head = git.head(&mut sink)?;
        let unpublished = git.has_unpublished_commits(&mut sink)?;
        Ok::<_, EngineError>((Arc::new(git), head, unpublished))
    })
    .await
    .map_err(|e| PushBackError::from(EngineError::Git(format!("push task failed: {e}"))))??;
    let (git, head, unpublished) = local;
    let outcome = PushPublisher::new(git, branch, PushRetryPolicy::for_command(deadline))
        .verify_only()
        .run(&cancel, &mut NoGate, &sink)
        .await?;
    let origin_sha = match outcome {
        PushOutcome::NotPushed { remote_sha } => remote_sha,
        PushOutcome::AlreadyPublished { sha } | PushOutcome::Pushed { sha, .. } => Some(sha),
    };
    Ok(Some(PushBaseline {
        head,
        origin_sha,
        unpublished,
    }))
}

/// Why a push did or did not run; projected into the command result by the worker.
#[derive(Debug)]
pub(crate) enum PushBackOutcome {
    NotConfigured,
    NothingToPush,
    Pushed(PushedRef),
}

#[derive(Debug)]
pub(crate) struct PushBackError {
    pub stage: &'static str,
    code: &'static str,
    pub message: String,
}

impl PushBackError {
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl From<EngineError> for PushBackError {
    fn from(error: EngineError) -> Self {
        let (stage, code) = crate::command::session_git_transport::git_failure_code(&error)
            .unwrap_or((failure_stages::PUSH, failure_codes::PUSH_FAILED));
        Self {
            stage,
            code,
            message: error.to_string(),
        }
    }
}

impl From<crate::command::error::CommandError> for PushBackError {
    /// Transport choice failures (`credentials_lost`) keep their own code.
    fn from(error: crate::command::error::CommandError) -> Self {
        Self {
            stage: error.stage(),
            code: error.code(),
            message: error.to_string(),
        }
    }
}

impl From<PushFailure> for PushBackError {
    fn from(failure: PushFailure) -> Self {
        Self {
            stage: failure.stage(),
            code: failure.code(),
            message: failure.message,
        }
    }
}

/// What bounds a push-back: the command's cancellation and deadline, and the
/// session's transport (the same builder as its clone and fetches).
pub(crate) struct PushBackContext {
    pub cancel: CancellationToken,
    pub deadline: Option<DateTime<Utc>>,
    pub transport: HostGitTransport,
}

/// Local preparation of a push-back: leftovers committed, nothing networked.
struct Prepared {
    git: Arc<crate::engine::git::remote_push::RemotePushGit>,
    branch: String,
    baseline: PushBaseline,
    /// Neither the command nor setup produced anything to publish; only a
    /// head check runs (the workflow may have pushed for itself).
    nothing_new: bool,
}

fn prepare(
    git: &GitEngine,
    session: &SessionRecord,
    message: &str,
    baseline: Option<PushBaseline>,
    transport: HostGitTransport,
    guard: crate::engine::git::transport::TransportGuard,
    sink: &mut dyn GitFrontend,
) -> Result<Option<Prepared>, PushBackError> {
    if session.session_type != "remote" || !session.push {
        return Ok(None);
    }
    let (Some(branch), Some(clone), Some(url)) =
        (&session.branch, &session.cloned_path, &session.repo_url)
    else {
        return Ok(None);
    };
    let Some(baseline) = baseline else {
        return Ok(None);
    };
    let git = git.isolate_remote_push_guarded(Path::new(clone), url, transport, guard)?;
    git.commit_leftovers(message, sink)?;
    let head = git.head(sink)?;
    Ok(Some(Prepared {
        git: Arc::new(git),
        branch: branch.clone(),
        nothing_new: head == baseline.head && !baseline.unpublished,
        baseline,
    }))
}

/// Commit leftovers and publish from awman's own clone with the session's
/// transport, retrying only failures before a remote response.
pub(crate) async fn publish_after_success(
    git: GitEngine,
    session: SessionRecord,
    subcommand: String,
    args: Vec<String>,
    baseline: Option<PushBaseline>,
    ctx: PushBackContext,
    sink: PushEventSink,
) -> Result<PushBackOutcome, PushBackError> {
    let PushBackContext {
        cancel,
        deadline,
        transport,
    } = ctx;
    let mut local_sink = PushEventSink(sink.0.clone());
    let mut local_guard = crate::engine::git::transport::TransportGuard::new(cancel.clone());
    local_guard.deadline = deadline;
    local_guard.timeout = crate::engine::git::push_retry::PUSH_FIRST_ATTEMPT_TIMEOUT;
    let message = leftover_commit_message(&session.id, &subcommand, &args);
    let prepared = tokio::task::spawn_blocking(move || {
        prepare(
            &git,
            &session,
            &message,
            baseline,
            transport,
            local_guard,
            &mut local_sink,
        )
    })
    .await
    .map_err(|e| PushBackError::from(EngineError::Git(format!("push task failed: {e}"))))??;
    let Some(prepared) = prepared else {
        return Ok(PushBackOutcome::NotConfigured);
    };
    let mut publisher = PushPublisher::new(
        prepared.git.clone(),
        prepared.branch.clone(),
        PushRetryPolicy::for_command(deadline),
    );
    if prepared.nothing_new {
        publisher = publisher.verify_only();
    }
    // The engine never force-pushes; rejection leaves the local commit in place.
    let outcome = publisher.run(&cancel, &mut NoGate, &sink).await?;
    let pushed = |sha: String| {
        PushBackOutcome::Pushed(PushedRef {
            remote: "origin".into(),
            branch: prepared.branch.clone(),
            sha,
        })
    };
    Ok(match outcome {
        PushOutcome::NotPushed { .. } => PushBackOutcome::NothingToPush,
        // Published before this command (baseline) is nothing to push;
        // published during it (by the workflow, or by an earlier attempt
        // whose response was lost) is a push.
        PushOutcome::AlreadyPublished { sha }
            if prepared.baseline.origin_sha.as_deref() == Some(sha.as_str()) =>
        {
            PushBackOutcome::NothingToPush
        }
        PushOutcome::AlreadyPublished { sha } | PushOutcome::Pushed { sha, .. } => {
            let git = prepared.git.clone();
            let branch = prepared.branch.clone();
            let recorded = sha.clone();
            tokio::task::spawn_blocking(move || git.record_published(&branch, &recorded))
                .await
                .map_err(|e| EngineError::Git(format!("push task failed: {e}")))??;
            pushed(sha)
        }
    })
}

pub(crate) fn leftover_commit_message(
    session_id: &str,
    subcommand: &str,
    args: &[String],
) -> String {
    // Use the command parser so positional prompt text and values of other
    // flags cannot accidentally be interpreted as a work-item flag.
    let path: Vec<&str> = subcommand.split_whitespace().collect();
    let work_item = CommandCatalogue::get()
        .parse_raw_args(&path, args)
        .ok()
        .and_then(|parsed| parsed.flag_string("work-item"))
        .and_then(|value| parse_work_item_number(&value));
    match work_item {
        Some(number) => format!("Implement work item {number:04}"),
        None => format!("awman: {subcommand} in session {session_id}"),
    }
}

/// Forward git command echoes, output and retries to the command log and SSE
/// stream, attributed to the push phase (WI 0137 §5).
pub(crate) struct PushEventSink(pub EventBusSender);

impl PushEventSink {
    fn push_phase(&self) -> EventBusSender {
        self.0
            .with_source(EventSource::phase(EventSourcePhase::Push))
    }
}

impl UserMessageSink for PushEventSink {
    fn write_message(&mut self, message: UserMessage) {
        PushObserver::message(self, message);
    }

    fn replay_queued(&mut self) {}
}

impl GitFrontend for PushEventSink {}

impl PushObserver for PushEventSink {
    /// WI 0145: the typed `push_retry` event; never Git output.
    fn retry(&self, retry: &PushRetry) {
        self.push_phase().emit(retry.event());
    }

    fn message(&self, message: UserMessage) {
        self.push_phase().emit(EventPayload::StatusMessage {
            phase: "push".into(),
            message: message.text,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::git::push_publish::PushFailureKind;

    #[test]
    fn push_failures_keep_their_stage_and_code_in_the_command_result() {
        for (kind, stage, code) in [
            (PushFailureKind::Auth, "git_auth", "git_auth_failed"),
            (PushFailureKind::Unreachable, "push", "push_unreachable"),
            (PushFailureKind::Cancelled, "push", "cancelled"),
            (PushFailureKind::Rejected, "push", "push_failed"),
            (PushFailureKind::Other, "push", "push_failed"),
        ] {
            let error = PushBackError::from(PushFailure::new(
                kind,
                "git push to example.invalid/r.git: m",
            ));
            assert_eq!((error.stage, error.code()), (stage, code), "{kind:?}");
            assert_eq!(error.message, "git push to example.invalid/r.git: m");
        }
        // Local errors before publication keep the generic push failure.
        let local = PushBackError::from(EngineError::Git("local".into()));
        assert_eq!((local.stage, local.code()), ("push", "push_failed"));
    }
}

#[cfg(test)]
mod transport_contract_tests {
    use super::super::event_bus::EventBus;
    use super::*;
    use awman_test_support::fakes::{FakeGitHttp, GitHttpFault};
    use awman_test_support::fixtures::RepoBuilder;
    use awman_test_support::{TestCase, TestSandbox};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pushed_command_adapter_retries_baseline_publishes_and_observes_cancellation() {
        #[cfg(windows)]
        {
            crate::data::fs::git_metadata::GitMetadataCopy::install_windows_publisher(
                awman_host_shims::publish_git_metadata,
            );
            crate::engine::git::transport::install_windows_spawn(awman_host_shims::spawn_git);
        }
        let s=TestSandbox::for_test(TestCase::component("command::push_back::pushed_command_adapter_retries_baseline_publishes_and_observes_cancellation"));
        s.require::<awman_test_support::capability::LocalGit>();
        s.require::<awman_test_support::capability::Loopback>();
        let secret = "test-secret-adapter";
        s.redact(secret);
        let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
        seed.bare_remote("origin");
        let mode = Arc::new(AtomicUsize::new(1));
        let first = AtomicUsize::new(0);
        let fault_mode = mode.clone();
        let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
            .basic_auth("alice", secret)
            .fault(move |r| {
                let mode = fault_mode.load(Ordering::SeqCst);
                let failed = match mode {
                    1 => {
                        r.service() == Some("git-upload-pack")
                            && first.fetch_add(1, Ordering::SeqCst) == 0
                    }
                    2 => r.service() == Some("git-upload-pack"),
                    3 => r.service() == Some("git-receive-pack"),
                    _ => false,
                };
                failed.then_some(GitHttpFault::DropBeforeResponse)
            })
            .start();
        let url = http.url("origin.git");
        let credential = s.root().join("credential");
        std::fs::write(&credential, format!("username=alice\npassword={secret}\n")).unwrap();
        s.command("git")
            .args([
                "config",
                "--global",
                "credential.helper",
                &format!("!cat '{}'", credential.display()),
            ])
            .run()
            .assert_success();
        let env = crate::data::config::env::EnvSnapshot::from_full_env(
            s.env()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        );
        let git = GitEngine::with_env(env);
        let clone = s.work().join("clone");
        mode.store(0, Ordering::SeqCst);
        git.clone_with_transport(
            &url,
            None,
            &clone,
            &HostGitTransport::user(crate::engine::git::transport::CredentialSource::Ambient),
            &mut crate::data::message::RecordingMessageSink::new(),
            &crate::engine::git::transport::TransportGuard::new(CancellationToken::new()),
        )
        .unwrap();
        let paths = crate::data::fs::api_paths::ApiPaths::from_root(s.root().join("api"));
        let store = crate::data::fs::api_db::SqliteSessionStore::open_from_paths(&paths).unwrap();
        let id = crate::data::ids::WorkerSessionId::new();
        store
            .insert_session(
                id.as_str(),
                clone.to_str().unwrap(),
                &Utc::now().to_rfc3339(),
            )
            .unwrap();
        let mut record = store.get_session(id.as_str()).unwrap().unwrap();
        record.session_type = "remote".into();
        record.push = true;
        record.repo_url = Some(url.clone());
        record.cloned_path = Some(clone.display().to_string());
        record.branch = Some("main".into());
        let context = |cancel: CancellationToken| PushBackContext {
            cancel,
            deadline: None,
            transport: HostGitTransport::user(
                crate::engine::git::transport::CredentialSource::Ambient,
            ),
        };
        let bus = EventBus::new(64);
        bus.redaction()
            .add(crate::data::secret::SecretString::new(secret));
        let mut events = bus.subscribe();
        mode.store(1, Ordering::SeqCst);
        let baseline = capture_baseline(
            git.clone(),
            record.clone(),
            context(CancellationToken::new()),
            PushEventSink(bus.sender()),
        )
        .await
        .unwrap();
        let retry = loop {
            let event = events.recv().await.unwrap();
            if matches!(event.payload, EventPayload::PushRetry { .. }) {
                break event;
            }
        };
        assert!(
            matches!(retry.payload,EventPayload::PushRetry { ref operation, attempt:2,.. } if operation=="ls-remote")
        );
        mode.store(0, Ordering::SeqCst);
        std::fs::write(clone.join("command-result"), "successful command").unwrap();
        let result = publish_after_success(
            git.clone(),
            record.clone(),
            "exec prompt".into(),
            vec!["go".into()],
            baseline,
            context(CancellationToken::new()),
            PushEventSink(bus.sender()),
        )
        .await
        .unwrap();
        assert!(matches!(result, PushBackOutcome::Pushed(_)));
        assert_eq!(http.receive_packs(), 1);
        for operation in [2, 3] {
            let bus = EventBus::new(64);
            let mut events = bus.subscribe();
            let cancel = CancellationToken::new();
            let stop = cancel.clone();
            let watcher = tokio::spawn(async move {
                loop {
                    let event = events.recv().await.unwrap();
                    assert!(!serde_json::to_string(&event)
                        .unwrap()
                        .contains("test-secret-adapter"));
                    if matches!(event.payload, EventPayload::PushRetry { .. }) {
                        let cancelled_at = std::time::Instant::now();
                        stop.cancel();
                        break cancelled_at;
                    }
                }
            });
            mode.store(0, Ordering::SeqCst);
            let baseline = capture_baseline(
                git.clone(),
                record.clone(),
                context(CancellationToken::new()),
                PushEventSink(bus.sender()),
            )
            .await
            .unwrap();
            std::fs::write(clone.join("command-result"), format!("next {operation}")).unwrap();
            mode.store(operation, Ordering::SeqCst);
            let error = publish_after_success(
                git.clone(),
                record.clone(),
                "exec prompt".into(),
                vec!["go".into()],
                baseline,
                context(cancel),
                PushEventSink(bus.sender()),
            )
            .await
            .unwrap_err();
            let cancelled_at = watcher.await.unwrap();
            assert_eq!(error.code(), "cancelled");
            // Bound cancellation latency, excluding the metadata copy,
            // commit and first remote attempt before cancellation is requested.
            assert!(cancelled_at.elapsed() < Duration::from_secs(2));
            assert_eq!(http.receive_packs(), 1, "no push after cancellation");
        }
    }
}
