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
            DraftSyncMode::Update {
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
    /// The next provider mutation. Production re-fetches the issue before
    /// every update (the conflict check); those reads are expected and skipped.
    async fn mutation(&mut self) -> Call {
        loop {
            match self.call().await {
                Call::Fetch => continue,
                other => return other,
            }
        }
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

// ---------------------------------------------------------------------------
// Existing-issue updates (WI 0059): `DraftSyncMode::Update` under the paused
// clock. These sessions never create, and publish only the body.
// ---------------------------------------------------------------------------

fn only_updates(f: &Fixture) -> Vec<String> {
    f.source
        .mutations()
        .into_iter()
        .map(|call| match call {
            Call::Update(body) => body,
            other => panic!("an existing-issue edit must only update: {other:?}"),
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn updates_publish_user_edits_after_the_quiet_period_and_never_create() {
    let mut f = Fixture::new(
        "command::draft_sync::updates_publish_user_edits_after_the_quiet_period_and_never_create",
    );
    let (running, mut events) = f.start(false);
    f.edit("first");
    assert_eq!(f.mutation().await, Call::Update("first".into()));
    f.edit("second");
    assert_eq!(f.mutation().await, Call::Update("second".into()));
    f.edit("final");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    let issue = outcome.issue.unwrap();
    assert_eq!(
        (issue.body.as_str(), issue.title.as_str()),
        ("final", "User title")
    );
    assert_eq!(only_updates(&f), ["first", "second", "final"]);
    // Updating an existing issue announces nothing new to open.
    assert!(!remaining(&mut events)
        .iter()
        .any(|e| matches!(e, DraftSyncEvent::Created { .. })));
    assert_eq!(f.source.remote.lock().unwrap().title, "User title");
    assert!(!f.path.exists());
}

#[tokio::test(start_paused = true)]
async fn unchanged_empty_and_whitespace_drafts_never_update() {
    for content in ["seed\n", "", " \n\t"] {
        let mut f =
            Fixture::new("command::draft_sync::unchanged_empty_and_whitespace_drafts_never_update");
        let (running, mut events) = f.start(false);
        f.edit(content);
        tokio::time::advance(Duration::from_secs(90)).await;
        let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
        assert!(!outcome.uploaded, "{content:?}");
        assert!(f.source.mutations().is_empty(), "{content:?}");
        assert_eq!(f.source.remote.lock().unwrap().body, "seed\n");
        assert!(remaining(&mut events).is_empty());
        assert!(
            !f.path.exists(),
            "a draft with nothing to publish is cleaned up"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn same_size_and_renamed_saves_are_detected_by_content_not_metadata() {
    // Same length and the original modification time restored.
    let mut f = Fixture::new(
        "command::draft_sync::same_size_and_renamed_saves_are_detected_by_content_not_metadata",
    );
    let (running, _events) = f.start(false);
    let modified = std::fs::metadata(&f.path).unwrap().modified().unwrap();
    f.edit("seeD\n");
    std::fs::File::options()
        .write(true)
        .open(&f.path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&f.path).unwrap().len(),
        "seed\n".len() as u64
    );
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(only_updates(&f), ["seeD\n"]);

    // An editor's atomic save: write a sibling, rename it over the draft.
    let mut f = Fixture::new(
        "command::draft_sync::same_size_and_renamed_saves_are_detected_by_content_not_metadata",
    );
    let (running, _events) = f.start(false);
    let sibling = f.path.with_extension("swap");
    std::fs::write(&sibling, "renamed save\n").unwrap();
    std::fs::rename(&sibling, &f.path).unwrap();
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(only_updates(&f), ["renamed save\n"]);
    assert!(!f.path.exists());
}

#[tokio::test(start_paused = true)]
async fn a_draft_removed_and_restored_mid_run_still_publishes_the_restored_content() {
    let mut f = Fixture::new(
        "command::draft_sync::a_draft_removed_and_restored_mid_run_still_publishes_the_restored_content",
    );
    let (running, _events) = f.start(false);
    std::fs::remove_file(&f.path).unwrap();
    tokio::time::advance(Duration::from_secs(3)).await;
    f.edit("restored\n");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(only_updates(&f), ["restored\n"]);
}

#[tokio::test(start_paused = true)]
async fn a_conflict_found_only_at_finalization_publishes_nothing_and_keeps_the_draft() {
    let mut f = Fixture::new(
        "command::draft_sync::a_conflict_found_only_at_finalization_publishes_nothing_and_keeps_the_draft",
    );
    let (running, _events) = f.start(false);
    f.source.remote.lock().unwrap().body = "someone else's edit".into();
    f.edit("my edit");
    let result = running.stop().await.unwrap().final_upload().await;
    assert!(matches!(
        result,
        Err(DraftSyncError::Paused {
            reason: SyncPauseReason::RemoteConflict,
            ..
        })
    ));
    assert!(f.source.mutations().is_empty());
    assert_eq!(f.source.remote.lock().unwrap().body, "someone else's edit");
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "my edit");
}

#[tokio::test(start_paused = true)]
async fn a_conflict_after_a_periodic_update_stops_publishing_for_good() {
    let mut f = Fixture::new(
        "command::draft_sync::a_conflict_after_a_periodic_update_stops_publishing_for_good",
    );
    let (running, mut events) = f.start(false);
    f.edit("first");
    assert_eq!(f.mutation().await, Call::Update("first".into()));
    // `update` is recorded before the fake applies it; only change the
    // tracker once "first" is really there, or the fake overwrites the edit.
    tracker_holds(&f.source, "first").await;
    f.source.remote.lock().unwrap().body = "tracker edit".into();
    f.edit("second");
    assert!(
        matches!(event(&mut events).await, DraftSyncEvent::Warning { message } if message.contains("changed on the tracker"))
    );
    tokio::time::advance(Duration::from_secs(120)).await;
    f.edit("third");
    let result = running.stop().await.unwrap().final_upload().await;
    assert!(matches!(
        result,
        Err(DraftSyncError::Paused {
            reason: SyncPauseReason::RemoteConflict,
            ..
        })
    ));
    assert_eq!(only_updates(&f), ["first"]);
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "third");
}

#[tokio::test(start_paused = true)]
async fn update_auth_and_permission_failures_pause_publishing_and_retain_the_draft() {
    for (failure, reason) in [
        (Failure::Auth, SyncPauseReason::Authentication),
        (Failure::Permission, SyncPauseReason::Permission),
    ] {
        let mut f = Fixture::new(
            "command::draft_sync::update_auth_and_permission_failures_pause_publishing_and_retain_the_draft",
        );
        f.source.failures.lock().unwrap().push_back(failure);
        let (running, mut events) = f.start(false);
        f.edit("edited");
        assert!(matches!(
            event(&mut events).await,
            DraftSyncEvent::Warning { .. }
        ));
        tokio::time::advance(Duration::from_secs(90)).await;
        f.edit("edited again");
        let result = running.stop().await.unwrap().final_upload().await;
        assert!(
            matches!(result, Err(DraftSyncError::Paused { reason: actual, .. }) if actual == reason),
            "{reason:?}"
        );
        assert_eq!(
            only_updates(&f),
            ["edited"],
            "never retried, not even at exit"
        );
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "edited again");
    }
}

#[tokio::test(start_paused = true)]
async fn a_rate_limited_update_backs_off_and_finalization_still_publishes_the_latest() {
    let mut f = Fixture::new(
        "command::draft_sync::a_rate_limited_update_backs_off_and_finalization_still_publishes_the_latest",
    );
    f.source
        .failures
        .lock()
        .unwrap()
        .push_back(Failure::RateLimited);
    let (running, mut events) = f.start(false);
    f.edit("first");
    assert!(matches!(
        event(&mut events).await,
        DraftSyncEvent::Warning { .. }
    ));
    // Inside the rate-limit back-off nothing more is sent.
    f.edit("during back-off");
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(only_updates(&f), ["first"]);
    f.edit("latest");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded);
    assert_eq!(only_updates(&f), ["first", "latest"]);
    assert!(!f.path.exists());
}

#[tokio::test(start_paused = true)]
async fn only_one_update_is_in_flight_and_finalization_publishes_after_it_settles() {
    let mut f = Fixture::new(
        "command::draft_sync::only_one_update_is_in_flight_and_finalization_publishes_after_it_settles",
    );
    *f.source.block_once.lock().unwrap() = true;
    let (running, _events) = f.start(false);
    f.edit("a");
    assert_eq!(f.mutation().await, Call::Update("a".into()));
    f.edit("b");
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(
        only_updates(&f),
        ["a"],
        "no second request while one is in flight"
    );
    let stopped = tokio::spawn(running.stop());
    tokio::task::yield_now().await;
    assert!(!stopped.is_finished(), "finalization waits for the writer");
    f.source.gate.release();
    let outcome = stopped
        .await
        .unwrap()
        .unwrap()
        .final_upload()
        .await
        .unwrap();
    assert!(outcome.uploaded);
    assert_eq!(only_updates(&f), ["a", "b"]);
    assert_eq!(f.source.remote.lock().unwrap().body, "b");
}

#[tokio::test(start_paused = true)]
async fn unsafe_draft_states_are_never_published_and_finalization_keeps_them() {
    use awman::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;
    for kind in ["directory", "invalid-utf8", "oversized"] {
        let mut f = Fixture::new(
            "command::draft_sync::unsafe_draft_states_are_never_published_and_finalization_keeps_them",
        );
        let (running, _events) = f.start(false);
        match kind {
            "directory" => {
                std::fs::remove_file(&f.path).unwrap();
                std::fs::create_dir(&f.path).unwrap();
            }
            "invalid-utf8" => std::fs::write(&f.path, [0xff, 0xfe, 0xfd]).unwrap(),
            _ => std::fs::write(&f.path, vec![b'x'; MAX_WORK_ITEM_BYTES as usize + 1]).unwrap(),
        }
        tokio::time::advance(Duration::from_secs(90)).await;
        let result = running.stop().await.unwrap().final_upload().await;
        assert!(
            matches!(result, Err(DraftSyncError::Read { .. })),
            "{kind}: {result:?}"
        );
        assert!(f.source.mutations().is_empty(), "{kind}");
        assert!(f.path.exists(), "{kind}: kept for recovery");
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_final_update_retains_the_draft_and_names_it() {
    for failure in [Failure::Transient, Failure::RateLimited, Failure::Auth] {
        let mut f = Fixture::new(
            "command::draft_sync::a_failed_final_update_retains_the_draft_and_names_it",
        );
        f.source.failures.lock().unwrap().push_back(failure);
        let (running, _events) = f.start(false);
        f.edit("edited");
        let error = running
            .stop()
            .await
            .unwrap()
            .final_upload()
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(&f.path.display().to_string()),
            "{error}"
        );
        assert!(!matches!(error, DraftSyncError::Read { .. }));
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "edited");
        assert_eq!(f.source.remote.lock().unwrap().body, "seed\n");
    }
}

#[tokio::test(start_paused = true)]
async fn an_update_reverted_to_the_original_body_after_a_periodic_upload_is_published() {
    let mut f = Fixture::new(
        "command::draft_sync::an_update_reverted_to_the_original_body_after_a_periodic_upload_is_published",
    );
    let (running, _events) = f.start(false);
    f.edit("interim");
    assert_eq!(f.mutation().await, Call::Update("interim".into()));
    tracker_holds(&f.source, "interim").await;
    // Restore the issue's original body (the seed) and exit.
    f.edit("seed\n");
    let outcome = running.stop().await.unwrap().final_upload().await.unwrap();
    assert!(outcome.uploaded, "the revert must reach the tracker");
    assert_eq!(outcome.issue.unwrap().body, "seed\n");
    assert_eq!(only_updates(&f), ["interim", "seed\n"]);
    assert_eq!(f.source.remote.lock().unwrap().body, "seed\n");
    assert!(
        !f.path.exists(),
        "deleted only after the acknowledged update"
    );
}

/// Wait (off the paused runtime) until the fake tracker has applied `body`:
/// `update` is recorded before the fake writes it.
async fn tracker_holds(source: &Arc<FakeSource>, body: &'static str) {
    let source = Arc::clone(source);
    tokio::task::spawn_blocking(move || {
        awman_test_support::wait_for(
            "the fake tracker to apply the update",
            Duration::from_secs(15),
            || (source.remote.lock().unwrap().body == body).then_some(()),
        )
    })
    .await
    .unwrap();
}
