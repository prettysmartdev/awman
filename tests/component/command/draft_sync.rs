//! Draft publishing against an owned fake provider and private real draft files.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use awman::command::work_item::draft_sync::{
    DraftSync, DraftSyncError, DraftSyncEvent, DraftSyncMode, DraftSyncRequest, RunningDraftSync,
    SyncPauseReason,
};
use awman::data::fs::context_dirs::ContextDirResolver;
use awman::data::fs::work_item_draft::WorkItemDraft;
use awman::data::issue::{Issue, IssueOperation, IssueSourceError};
use awman::engine::issue::IssueSource;
use awman_test_support::{TestCase, TestSandbox};
use tokio::sync::mpsc::{self, UnboundedReceiver};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Create(String, String),
    Fetch,
    Update(String),
}

enum Failure {
    Transient,
    Auth,
    Permission,
    CreateTimeout,
    UnknownCreate,
    RateLimited,
}

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    ready: Condvar,
}
impl Gate {
    fn wait(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.ready.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // A failing assertion must not leave a blocking provider thread alive.
        self.source.gate.release();
    }
}

struct FakeSource {
    remote: Mutex<Issue>,
    calls: Mutex<Vec<Call>>,
    attempts: mpsc::UnboundedSender<Call>,
    failures: Mutex<VecDeque<Failure>>,
    gate: Gate,
    block_once: Mutex<bool>,
}
impl FakeSource {
    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call.clone());
        self.attempts.send(call).unwrap();
    }
    fn mutate(&self, body: &str) -> Result<Issue, IssueSourceError> {
        let block = std::mem::take(&mut *self.block_once.lock().unwrap());
        if block {
            self.gate.wait();
        }
        let provider = "fake".to_string();
        if let Some(failure) = self.failures.lock().unwrap().pop_front() {
            return Err(match failure {
                Failure::Transient => IssueSourceError::Network {
                    provider,
                    detail: "offline".into(),
                },
                Failure::Auth => IssueSourceError::Unauthorized {
                    provider,
                    hint: "sign in".into(),
                },
                Failure::Permission => IssueSourceError::Forbidden {
                    provider,
                    hint: "read only".into(),
                },
                Failure::CreateTimeout => IssueSourceError::Timeout {
                    provider,
                    operation: IssueOperation::Create,
                },
                Failure::UnknownCreate => IssueSourceError::CreateOutcomeUnknown {
                    provider,
                    detail: "response lost".into(),
                },
                Failure::RateLimited => IssueSourceError::RateLimited { provider },
            });
        }
        let mut issue = self.remote.lock().unwrap();
        issue.body = body.into();
        Ok(issue.clone())
    }
    fn mutations(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !matches!(c, Call::Fetch))
            .cloned()
            .collect()
    }
}
impl IssueSource for FakeSource {
    fn provider_name(&self) -> &str {
        "fake"
    }
    fn provider_prefix(&self) -> &str {
        "fake"
    }
    fn issue_identifier(&self, _: &Issue) -> String {
        "7".into()
    }
    fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
        self.record(Call::Fetch);
        Ok(self.remote.lock().unwrap().clone())
    }
    fn create_issue(&self, title: &str, body: &str, _: &Path) -> Result<Issue, IssueSourceError> {
        self.record(Call::Create(title.into(), body.into()));
        self.mutate(body)
    }
    fn update_issue(&self, _: &Issue, body: &str) -> Result<Issue, IssueSourceError> {
        self.record(Call::Update(body.into()));
        self.mutate(body)
    }
}

struct Fixture {
    _sandbox: TestSandbox,
    source: Arc<FakeSource>,
    draft: Option<WorkItemDraft>,
    path: PathBuf,
    attempts: UnboundedReceiver<Call>,
}
impl Fixture {
    fn new(id: &'static str) -> Self {
        let sandbox = TestSandbox::for_test(TestCase::component(id));
        let resolver = ContextDirResolver::at_home(sandbox.awman_home());
        let paths = resolver.work_item_draft_paths(None, &sandbox.work(), uuid::Uuid::new_v4());
        let draft = WorkItemDraft::create(paths, "seed\n").unwrap();
        let path = draft.paths().host_path.clone();
        let (attempts, receiver) = mpsc::unbounded_channel();
        let source = Arc::new(FakeSource {
            remote: Mutex::new(Issue {
                source_id: "https://tracker.example/items/7".into(),
                id: "7".into(),
                number: Some(7),
                title: "User title".into(),
                body: "seed\n".into(),
                provider: "fake".into(),
            }),
            calls: Mutex::new(vec![]),
            attempts,
            failures: Mutex::new(VecDeque::new()),
            gate: Gate::default(),
            block_once: Mutex::new(false),
        });
        Self {
            _sandbox: sandbox,
            source,
            draft: Some(draft),
            path,
            attempts: receiver,
        }
    }
    fn start(&mut self, create: bool) -> (RunningDraftSync, UnboundedReceiver<DraftSyncEvent>) {
        let mode = if create {
            DraftSyncMode::Create {
                title: "User title".into(),
            }
        } else {
            DraftSyncMode::Amend {
                issue: self.source.remote.lock().unwrap().clone(),
            }
        };
        DraftSync::start(DraftSyncRequest {
            draft: self.draft.take().unwrap(),
            seeded_body: "seed\n".into(),
            mode,
            source: self.source.clone(),
            git_root: self._sandbox.work(),
        })
    }
    fn edit(&self, body: &str) {
        std::fs::write(&self.path, body).unwrap();
    }
    async fn call(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(120), self.attempts.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

async fn event(events: &mut UnboundedReceiver<DraftSyncEvent>) -> DraftSyncEvent {
    tokio::time::timeout(Duration::from_secs(120), events.recv())
        .await
        .unwrap()
        .unwrap()
}
fn remaining(events: &mut UnboundedReceiver<DraftSyncEvent>) -> Vec<DraftSyncEvent> {
    let mut result = vec![];
    while let Ok(event) = events.try_recv() {
        result.push(event);
    }
    result
}

#[tokio::test(start_paused = true)]
async fn interview_creates_once_then_updates_and_emits_only_one_url() {
    let mut f = Fixture::new(
        "command::draft_sync::interview_creates_once_then_updates_and_emits_only_one_url",
    );
    let (running, mut events) = f.start(true);
    f.edit("first edit");
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Created {
            number: Some(7),
            ..
        }
    ));
    f.edit("second edit");
    while f.call().await != Call::Update("second edit".into()) {}
    f.edit("final edit");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(outcome.issue.unwrap().body, "final edit");
    assert_eq!(
        f.source.mutations(),
        vec![
            Call::Create("User title".into(), "first edit".into()),
            Call::Update("second edit".into()),
            Call::Update("final edit".into())
        ]
    );
    assert!(remaining(&mut events).is_empty());
    assert!(!f.path.exists());
}

#[tokio::test(start_paused = true)]
async fn no_edits_and_empty_drafts_make_no_api_calls() {
    for content in ["seed\n", "", " \n\t"] {
        let mut f =
            Fixture::new("command::draft_sync::no_edits_and_empty_drafts_make_no_api_calls");
        let (running, mut events) = f.start(true);
        f.edit(content);
        tokio::time::advance(Duration::from_secs(90)).await;
        let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
        assert!(outcome.issue.is_none());
        assert!(!outcome.uploaded);
        assert!(f.source.calls.lock().unwrap().is_empty());
        assert!(remaining(&mut events).is_empty());
        assert!(!f.path.exists());
    }
}

#[tokio::test(start_paused = true)]
async fn final_upload_covers_edits_inside_the_debounce_window_and_current_is_a_noop() {
    let mut f = Fixture::new("command::draft_sync::final_upload_covers_edits_inside_the_debounce_window_and_current_is_a_noop");
    let (running, mut events) = f.start(false);
    f.edit("last second edit");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(
        f.source.mutations(),
        vec![Call::Update("last second edit".into())]
    );
    assert!(remaining(&mut events).is_empty());

    let mut f = Fixture::new("command::draft_sync::final_upload_covers_edits_inside_the_debounce_window_and_current_is_a_noop_current");
    let (running, mut events) = f.start(true);
    f.edit("published");
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Created { .. }
    ));
    let calls = f.source.calls.lock().unwrap().len();
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(!outcome.uploaded);
    assert_eq!(f.source.calls.lock().unwrap().len(), calls);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_symlink_swap_during_polling_is_never_uploaded_and_finalization_keeps_it() {
    let mut f = Fixture::new("command::draft_sync::a_symlink_swap_during_polling_is_never_uploaded_and_finalization_keeps_it");
    let (running, mut events) = f.start(true);
    tokio::task::yield_now().await;
    let secret = f._sandbox.tmp().join("secret");
    std::fs::write(&secret, "HOST SECRET").unwrap();
    std::fs::remove_file(&f.path).unwrap();
    std::os::unix::fs::symlink(&secret, &f.path).unwrap();
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Warning { .. }
    ));
    let result = running.stop().await.unwrap().final_upload().await;
    assert!(matches!(result, Err(DraftSyncError::Read { .. })));
    assert!(f.source.calls.lock().unwrap().is_empty());
    assert!(f.path.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "HOST SECRET");
}

#[tokio::test(start_paused = true)]
async fn transient_failures_retry_and_report_only_once_per_episode() {
    let mut f = Fixture::new(
        "command::draft_sync::transient_failures_retry_and_report_only_once_per_episode",
    );
    f.source
        .failures
        .lock()
        .unwrap()
        .extend([Failure::Transient, Failure::Transient]);
    let (running, mut events) = f.start(true);
    f.edit("edited");
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Warning { .. }
    ));
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Created { .. }
    ));
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(!outcome.uploaded);
    assert_eq!(f.source.mutations().len(), 3);
    assert!(remaining(&mut events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn auth_permission_and_uncertain_creates_never_retry_even_at_exit() {
    for (failure, reason) in [
        (Failure::Auth, SyncPauseReason::Authentication),
        (Failure::Permission, SyncPauseReason::Permission),
        (
            Failure::CreateTimeout,
            SyncPauseReason::CreateOutcomeUnknown,
        ),
        (
            Failure::UnknownCreate,
            SyncPauseReason::CreateOutcomeUnknown,
        ),
    ] {
        let mut f = Fixture::new(
            "command::draft_sync::auth_permission_and_uncertain_creates_never_retry_even_at_exit",
        );
        f.source.failures.lock().unwrap().push_back(failure);
        let (running, mut events) = f.start(true);
        f.edit("edited");
        let message = event(&mut events).await;
        if reason == SyncPauseReason::CreateOutcomeUnknown {
            assert!(
                matches!(message, DraftSyncEvent::Warning { message } if message.contains("check tracker before retrying"))
            );
        }
        tokio::time::advance(Duration::from_secs(90)).await;
        f.edit("another edit");
        let result = running.stop().await.unwrap().final_upload().await;
        assert!(
            matches!(result, Err(DraftSyncError::Paused { reason: actual, .. }) if actual == reason)
        );
        assert_eq!(f.source.mutations().len(), 1);
        assert!(f.path.exists());
        assert!(remaining(&mut events).is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn remote_edits_pause_amend_and_interview_updates() {
    for create in [false, true] {
        let mut f =
            Fixture::new("command::draft_sync::remote_edits_pause_amend_and_interview_updates");
        let (running, mut events) = f.start(create);
        if create {
            f.edit("first edit");
            assert!(matches!(
                event(&mut events).await,
                DraftSyncEvent::Created { .. }
            ));
        }
        f.source.remote.lock().unwrap().body = "someone else's edit".into();
        f.edit("my edit");
        assert!(
            matches!(event(&mut events).await, DraftSyncEvent::Warning { message } if message.contains("changed on the tracker"))
        );
        f.edit("later edit");
        let result = running.stop().await.unwrap().final_upload().await;
        assert!(matches!(
            result,
            Err(DraftSyncError::Paused {
                reason: SyncPauseReason::RemoteConflict,
                ..
            })
        ));
        assert_eq!(f.source.remote.lock().unwrap().body, "someone else's edit");
        assert_eq!(f.source.mutations().len(), usize::from(create));
        assert!(f.path.exists());
    }
}

#[tokio::test(start_paused = true)]
async fn line_ending_changes_do_not_cause_false_conflicts() {
    let mut f =
        Fixture::new("command::draft_sync::line_ending_changes_do_not_cause_false_conflicts");
    let (running, _) = f.start(false);
    f.source.remote.lock().unwrap().body = "seed\r\n".into();
    f.edit("edit\n");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(outcome.issue.unwrap().body, "edit\n");
}

#[tokio::test(start_paused = true)]
async fn stopping_waits_for_an_in_flight_request_then_final_publishes_the_latest_edit() {
    let mut f = Fixture::new("command::draft_sync::stopping_waits_for_an_in_flight_request_then_final_publishes_the_latest_edit");
    *f.source.block_once.lock().unwrap() = true;
    let (running, mut events) = f.start(true);
    f.edit("first");
    assert_eq!(
        f.call().await,
        Call::Create("User title".into(), "first".into())
    );
    f.edit("latest");
    let stopped = tokio::spawn(running.stop());
    tokio::task::yield_now().await;
    assert!(
        !stopped.is_finished(),
        "stop must wait for the in-flight create"
    );
    assert_eq!(f.source.mutations().len(), 1);
    f.source.gate.release();
    let outcome = stopped
        .await
        .unwrap()
        .unwrap()
        .final_upload()
        .await
        .unwrap();
    assert!(outcome.uploaded);
    assert_eq!(
        f.source.mutations(),
        vec![
            Call::Create("User title".into(), "first".into()),
            Call::Update("latest".into())
        ]
    );
    let emitted = remaining(&mut events);
    assert_eq!(emitted.len(), 1);
    assert!(matches!(emitted[0], DraftSyncEvent::Created { .. }));
}

#[tokio::test(start_paused = true)]
async fn rate_limit_backoff_keeps_latest_content_and_finalization_bypasses_it() {
    let mut f = Fixture::new(
        "command::draft_sync::rate_limit_backoff_keeps_latest_content_and_finalization_bypasses_it",
    );
    f.source
        .failures
        .lock()
        .unwrap()
        .push_back(Failure::RateLimited);
    let (running, mut events) = f.start(true);
    f.edit("first");
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Warning { .. }
    ));
    f.edit("latest");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(
        f.source.mutations(),
        vec![
            Call::Create("User title".into(), "first".into()),
            Call::Create("User title".into(), "latest".into())
        ]
    );
    assert!(matches!(
        remaining(&mut events).as_slice(),
        [DraftSyncEvent::Created { .. }]
    ));
}

#[tokio::test(start_paused = true)]
async fn final_failure_retains_the_draft_and_a_reverted_seed_keeps_the_created_issue() {
    let mut f = Fixture::new("command::draft_sync::final_failure_retains_the_draft_and_a_reverted_seed_keeps_the_created_issue");
    f.source
        .failures
        .lock()
        .unwrap()
        .push_back(Failure::Transient);
    let (running, _) = f.start(false);
    f.edit("edited");
    assert!(matches!(
        running.stop().await.unwrap().final_upload().await,
        Err(DraftSyncError::Upload { .. })
    ));
    assert!(f.path.exists());

    let mut f = Fixture::new("command::draft_sync::final_failure_retains_the_draft_and_a_reverted_seed_keeps_the_created_issue_revert");
    let (running, mut events) = f.start(true);
    f.edit("published");
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Created { .. }
    ));
    f.edit("seed\n");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(!outcome.uploaded);
    assert_eq!(outcome.issue.unwrap().body, "published");
    assert_eq!(f.source.mutations().len(), 1);
    assert!(!f.path.exists());
}

fn session(sandbox: &TestSandbox) -> awman::data::session::Session {
    use awman::data::config::env::EnvSnapshot;
    use awman::data::session::{Session, SessionOpenOptions};
    let env = EnvSnapshot::from_full_env(
        sandbox
            .env()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    Session::open_at_directory_root(
        sandbox.work(),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn preparation_refuses_read_only_context_and_uses_the_actual_keyed_mount() {
    use awman::command::work_item::edit::prepare_draft_edit;
    use awman::command::work_item::resolver::WorkItemError;
    use awman::data::config::overlays::{
        CollectedOverlays, ContextOverlaySpec, ContextScope, OverlayPermission,
    };
    use awman::data::context::ContextKey;
    use awman::data::message::RecordingMessageSink;

    let sandbox = TestSandbox::for_test(TestCase::component("command::draft_sync::preparation_refuses_read_only_context_and_uses_the_actual_keyed_mount"));
    let session = session(&sandbox);
    let key = ContextKey::parse("team-service").unwrap();
    let mut collected = CollectedOverlays {
        context_overlays: vec![ContextOverlaySpec {
            key: Some(key.clone()),
            scope: ContextScope::Repo,
            permission: OverlayPermission::ReadOnly,
        }],
        ..Default::default()
    };
    let mut sink = RecordingMessageSink::new();
    assert!(matches!(
        prepare_draft_edit(&session, None, &mut collected, "raw body", &mut sink),
        Err(WorkItemError::ReadOnlyContext)
    ));
    assert!(sink.all().is_empty());
    collected.context_overlays[0].permission = OverlayPermission::ReadWrite;
    let prepared =
        prepare_draft_edit(&session, None, &mut collected, "raw body", &mut sink).unwrap();
    let resolver = ContextDirResolver::from_env(session.env()).unwrap();
    assert!(prepared
        .draft
        .paths()
        .host_path
        .starts_with(resolver.keyed_repo_dir(&key)));
    assert_eq!(collected.context_overlays.len(), 1);
    assert_eq!(prepared.seeded_body, "raw body");
    assert_eq!(
        awman::data::fs::work_item_draft::read_work_item_draft(&prepared.draft)
            .unwrap()
            .content,
        "raw body"
    );
    assert_eq!(sink.all().len(), 1);
    assert!(sink.all()[0]
        .text
        .contains(prepared.draft.paths().host_path.to_str().unwrap()));
    prepared.draft.remove().unwrap();
}

#[test]
fn preparation_rejects_config_read_only_and_unsupported_mounts_before_creating_a_draft() {
    use awman::command::work_item::edit::prepare_draft_edit;
    use awman::data::config::overlays::CollectedOverlays;
    use awman::data::message::RecordingMessageSink;
    for (config, global) in [
        (
            r#"{"overlays":["context(repo:rw)","context(repo:ro)"]}"#,
            false,
        ),
        (r#"{"runtime":{"kind":"sbx"}}"#, true),
    ] {
        let sandbox = TestSandbox::for_test(TestCase::component("command::draft_sync::preparation_rejects_config_read_only_and_unsupported_mounts_before_creating_a_draft"));
        let directory = if global {
            sandbox.awman_home()
        } else {
            sandbox.work().join(".awman")
        };
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("config.json"), config).unwrap();
        let session = session(&sandbox);
        let mut collected = CollectedOverlays::default();
        let mut sink = RecordingMessageSink::new();
        assert!(prepare_draft_edit(&session, None, &mut collected, "raw body", &mut sink).is_err());
        assert!(sink.all().is_empty());
        assert!(collected.context_overlays.is_empty());
    }
}
