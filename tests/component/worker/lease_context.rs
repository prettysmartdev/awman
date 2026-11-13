//! Pulled-lease durable context (`lease_context`): the work server owns the
//! memory, a lease stages it privately, and changes travel back in the
//! result or release. Each "host" is a distinct awman home in the sandbox.
//!
//! The pull controller's own lease loop needs a container runtime, so these
//! tests drive the same production entry points it calls
//! (`command::worker::lease_context::{admit, stage, collect, remove_orphans}`)
//! and report through the production `WspSession` to the mock server.
use crate::support;
use awman::command::worker::lease_context;
use awman::data::{
    execution_event::CommandStatusKind, fs::api_db::CommandResult,
    fs::context_dirs::ContextDirResolver, ids::LeaseId,
};
use awman::engine::worker::{
    error::WorkerEngineError, lease_context::LeaseContextStage, LeaseRef, WspSession,
};
use awman_test_support::{fakes::FakeWorkServer, TestSandbox};
use awman_wsp::{
    codes, conformance::lease_context::LeaseContextAdmin, features, ClaimOutcome,
    ContextChangeKind, ContextChanges, ItemContext, ItemContextFile, LeaseResult, RegisterRequest,
    ReleaseReason, ReleaseRequest, WorkItem,
};
use base64::Engine as _;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

const REPO: &str = "https://example.invalid/memory.git";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn host(sandbox: &TestSandbox, name: &str) -> ContextDirResolver {
    let resolver = ContextDirResolver::at_home(sandbox.work().join(name).join(".awman"));
    std::fs::create_dir_all(resolver.awman_home().join("context")).unwrap();
    resolver
}

fn context(files: &[(&str, &[u8])], max_bytes: u64) -> ItemContext {
    ItemContext {
        files: files
            .iter()
            .map(|(path, bytes)| ItemContextFile {
                path: (*path).into(),
                content_base64: b64(bytes),
            })
            .collect(),
        max_bytes,
    }
}

fn item(id: &str, ctx: Option<ItemContext>) -> WorkItem {
    let mut item = awman_wsp::conformance::work_item(id);
    item.repo.url = REPO.into();
    item.context = ctx;
    item
}

fn done() -> CommandResult {
    CommandResult {
        outcome: Some(CommandStatusKind::Done),
        exit_code: Some(0),
        ..Default::default()
    }
}

fn lease_result(
    result: CommandResult,
    changes: Option<ContextChanges>,
) -> LeaseResult<CommandResult> {
    LeaseResult {
        context_changes: changes,
        workflow_context: None,
        result,
        outputs: None,
        outputs_mounted: false,
        duration_ms: 1,
    }
}

/// Files under a directory, path → content.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
        .collect()
}

fn code_of(error: WorkerEngineError) -> &'static str {
    match error {
        WorkerEngineError::Context { code, message } => {
            assert!(!message.contains(".awman"), "host path in {message}");
            code
        }
        other => panic!("expected a context error, got {other}"),
    }
}

/// The features this worker registers: what the daemon-hosted pull worker
/// advertises for context (see the inline capability test).
fn capable() -> Vec<&'static str> {
    vec![features::LEASE_CONTEXT]
}

async fn linked(server: &FakeWorkServer, name: &str, features: &[&str]) -> Arc<WspSession> {
    let link = server.server().link();
    server.server().approve_link(&link.link_id, BTreeMap::new());
    let client = awman_wsp::Client::new(server.base_url(), String::new()).unwrap();
    let token = client.poll_link(&link.link_id).await.unwrap().unwrap();
    let session =
        Arc::new(WspSession::connect(name, server.base_url(), token.refresh_token).unwrap());
    session
        .register(&RegisterRequest {
            name: name.into(),
            labels: BTreeMap::new(),
            capacity: 4,
            features: features.iter().map(|f| f.to_string()).collect(),
        })
        .await
        .unwrap();
    session
}

async fn claim(session: &WspSession, id: &str) -> awman_wsp::Lease {
    match session.claim(Some(id), 0).await.unwrap() {
        ClaimOutcome::Leased(lease) => lease,
        ClaimOutcome::Empty => panic!("{id} was not offered"),
    }
}

/// Claim on one host: admit with this worker's features and stage the
/// snapshot before any clone.
async fn begin(
    session: &WspSession,
    resolver: &ContextDirResolver,
    id: &str,
) -> (awman_wsp::Lease, LeaseContextStage) {
    let lease = claim(session, id).await;
    lease_context::admit(&lease.item, &capable()).unwrap();
    let stage = lease_context::stage(resolver, &LeaseId::new(), &lease.item)
        .unwrap()
        .expect("the item carries context");
    assert!(stage.dir().starts_with(resolver.lease_repo_root()));
    (lease, stage)
}

/// After the run: collect, report, tear down.
async fn finish(session: &WspSession, lease: &awman_wsp::Lease, stage: LeaseContextStage) {
    let (result, changes) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
    assert_eq!(result.outcome, Some(CommandStatusKind::Done));
    assert!(changes.as_ref().unwrap().next_cursor.is_none());
    session
        .result(&LeaseRef::of(lease), &lease_result(result, changes))
        .await
        .unwrap();
    let dir = stage.dir().to_path_buf();
    stage.remove().unwrap();
    assert!(!dir.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_hosts_share_server_memory_and_the_next_lease_sees_the_update() {
    let sandbox = support::sandbox(
        "worker::lease_context::two_hosts_share_server_memory_and_the_next_lease_sees_the_update",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let (host_a, host_b) = (host(&sandbox, "host-a"), host(&sandbox, "host-b"));
    let a = linked(&server, "a", &capable()).await;
    let b = linked(&server, "b", &capable()).await;
    let mock = server.server();
    let seed = context(&[("a.md", b"one"), ("dir/gone.md", b"bye")], 4096);
    for id in ["first", "second"] {
        mock.enqueue_with_context(item(id, Some(seed.clone())), chrono::Utc::now())
            .unwrap();
    }
    // Two workers on distinct hosts hold leases on the same server memory.
    let initial = files(&[("a.md", "one"), ("dir/gone.md", "bye")]);
    let (lease_a, stage_a) = begin(&a, &host_a, "first").await;
    let (lease_b, stage_b) = begin(&b, &host_b, "second").await;
    assert_eq!(tree(stage_a.dir()), initial);
    assert_eq!(tree(stage_b.dir()), initial);
    // Host A's agent edits its mounted directory; host B's changes nothing.
    std::fs::write(stage_a.dir().join("a.md"), "two").unwrap();
    std::fs::remove_file(stage_a.dir().join("dir/gone.md")).unwrap();
    std::fs::write(stage_a.dir().join("new.md"), "new").unwrap();
    finish(&a, &lease_a, stage_a).await;
    finish(&b, &lease_b, stage_b).await;
    let updated = files(&[("a.md", "two"), ("new.md", "new")]);
    assert_eq!(mock.context_for(REPO).unwrap().files, updated);
    // The next lease, on the other host, observes the update and returns its own.
    mock.enqueue_with_context(item("third", None), chrono::Utc::now())
        .unwrap();
    let (lease, stage) = begin(&b, &host_b, "third").await;
    assert_eq!(tree(stage.dir()), updated);
    std::fs::remove_file(stage.dir().join("new.md")).unwrap();
    finish(&b, &lease, stage).await;
    assert_eq!(
        mock.context_for(REPO).unwrap().files,
        files(&[("a.md", "two")])
    );
    // No host key was read or written on either host; nothing is left staged.
    for resolver in [&host_a, &host_b] {
        assert_no_host_keys(resolver);
        assert_eq!(
            std::fs::read_dir(resolver.lease_repo_root())
                .unwrap()
                .count(),
            0
        );
    }
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_never_share_a_directory_or_carry_state() {
    let sandbox =
        support::sandbox("worker::lease_context::leases_never_share_a_directory_or_carry_state");
    let resolver = host(&sandbox, "host");
    let it = item("x", Some(context(&[("a", b"1")], 64)));
    let first = lease_context::stage(&resolver, &LeaseId::new(), &it)
        .unwrap()
        .unwrap();
    std::fs::write(first.dir().join("leftover"), "agent one").unwrap();
    let second = lease_context::stage(&resolver, &LeaseId::new(), &it)
        .unwrap()
        .unwrap();
    assert_ne!(first.dir(), second.dir());
    assert_eq!(tree(second.dir()), files(&[("a", "1")]));
    // A directory that already exists for a lease id is never reused.
    let reused = LeaseId::new();
    std::fs::create_dir_all(resolver.lease_repo_dir(&reused)).unwrap();
    std::fs::write(resolver.lease_repo_dir(&reused).join("planted"), "x").unwrap();
    let error = lease_context::stage(&resolver, &reused, &it).err().unwrap();
    assert_eq!(code_of(error), codes::CONTEXT_INVALID);
    // An item without context stages nothing.
    assert!(
        lease_context::stage(&resolver, &LeaseId::new(), &item("y", None))
            .unwrap()
            .is_none()
    );
    first.remove().unwrap();
    second.remove().unwrap();
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_symlinked_and_oversized_snapshots_fail_before_anything_is_staged() {
    let sandbox = support::sandbox(
        "worker::lease_context::malformed_symlinked_and_oversized_snapshots_fail_before_anything_is_staged",
    );
    let resolver = host(&sandbox, "host");
    let raw = |path: &str, content: &str| ItemContextFile {
        path: path.into(),
        content_base64: content.into(),
    };
    let cases: Vec<(Vec<ItemContextFile>, u64, &str)> = vec![
        (vec![raw("../escape", "eA==")], 64, codes::CONTEXT_INVALID),
        (vec![raw("/abs", "eA==")], 64, codes::CONTEXT_INVALID),
        (vec![raw("a/./b", "eA==")], 64, codes::CONTEXT_INVALID),
        (vec![raw("a\\b", "eA==")], 64, codes::CONTEXT_INVALID),
        (vec![raw("c:x", "eA==")], 64, codes::CONTEXT_INVALID),
        (
            vec![raw("a", "eA=="), raw("a", "eQ==")],
            64,
            codes::CONTEXT_INVALID,
        ),
        (
            vec![raw("Notes", "eA=="), raw("notes", "eQ==")],
            64,
            codes::CONTEXT_INVALID,
        ),
        (
            vec![raw("a", "eA=="), raw("a/b", "eQ==")],
            64,
            codes::CONTEXT_INVALID,
        ),
        (vec![raw("bad", "!!!")], 64, codes::CONTEXT_INVALID),
        (
            vec![raw("big", &b64(&[0; 65]))],
            64,
            codes::CONTEXT_TOO_LARGE,
        ),
        (
            vec![raw("x", &b64(&[0; 40])), raw("y", &b64(&[0; 40]))],
            64,
            codes::CONTEXT_TOO_LARGE,
        ),
    ];
    for (files, max_bytes, code) in cases {
        let lease = LeaseId::new();
        let it = item("bad", Some(ItemContext { files, max_bytes }));
        let error = lease_context::stage(&resolver, &lease, &it).err().unwrap();
        assert_eq!(code_of(error), code);
        assert!(!resolver.lease_repo_dir(&lease).exists());
    }
    // `max_bytes` above the 16 MiB ceiling does not raise it.
    let huge = vec![raw("big", &b64(&vec![0; (16 << 20) + 1]))];
    let error = lease_context::stage(
        &resolver,
        &LeaseId::new(),
        &item(
            "huge",
            Some(ItemContext {
                files: huge,
                max_bytes: u64::MAX,
            }),
        ),
    )
    .err()
    .unwrap();
    assert_eq!(code_of(error), codes::CONTEXT_TOO_LARGE);
    // An unknown field in the snapshot fails to decode at all.
    let mut value = serde_json::to_value(item("strict", None)).unwrap();
    value["context"] = serde_json::json!({"files": [], "unexpected": true});
    assert!(serde_json::from_value::<WorkItem>(value).is_err());
    // A symlinked lease root is refused, and its target is untouched.
    #[cfg(unix)]
    {
        let outside = sandbox.work().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(resolver.awman_home().join("context/repo")).unwrap();
        std::os::unix::fs::symlink(&outside, resolver.lease_repo_root()).unwrap();
        let ok = item("ok", Some(context(&[("a", b"1")], 64)));
        let error = lease_context::stage(&resolver, &LeaseId::new(), &ok)
            .err()
            .unwrap();
        assert_eq!(code_of(error), codes::CONTEXT_INVALID);
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        std::fs::remove_file(resolver.lease_repo_root()).unwrap();
    }
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_changes_fail_explicitly_with_metadata_only() {
    let sandbox = support::sandbox(
        "worker::lease_context::oversized_changes_fail_explicitly_with_metadata_only",
    );
    let resolver = host(&sandbox, "host");
    let it = item("cap", Some(context(&[("a.md", b"one"), ("gone", b"x")], 8)));
    let stage = lease_context::stage(&resolver, &LeaseId::new(), &it)
        .unwrap()
        .unwrap();
    std::fs::write(stage.dir().join("big.md"), "0123456789").unwrap();
    std::fs::remove_file(stage.dir().join("gone")).unwrap();
    let (result, changes) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
    assert_eq!(result.outcome, Some(CommandStatusKind::Error));
    let failure = result.failure.unwrap();
    assert_eq!(
        (failure.stage.as_str(), failure.code.as_str()),
        ("context", codes::CONTEXT_TOO_LARGE)
    );
    let changes = changes.expect("metadata is still reported");
    assert!(changes.next_cursor.is_none());
    let summary: Vec<_> = changes
        .items
        .iter()
        .map(|i| {
            (
                i.path.as_str(),
                i.change,
                i.size_bytes,
                i.content_base64.is_some(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("big.md", ContextChangeKind::Added, Some(10), false),
            ("gone", ContextChangeKind::Deleted, None, false),
        ]
    );
    // A run that already failed keeps its own failure.
    let failed = CommandResult {
        outcome: Some(CommandStatusKind::Cancelled),
        exit_code: Some(130),
        ..Default::default()
    };
    let (kept, _) = lease_context::collect(&stage, failed.clone(), &AtomicBool::new(false));
    assert_eq!(kept, failed);
    // Under the cap: content is inlined, deletions carry none.
    std::fs::remove_file(stage.dir().join("big.md")).unwrap();
    std::fs::write(stage.dir().join("a.md"), "two").unwrap();
    let (result, changes) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
    assert_eq!(result, done());
    let changes = changes.unwrap();
    assert_eq!(changes.items.len(), 2);
    let modified = &changes.items[0];
    assert_eq!(
        modified.content_base64.as_deref(),
        Some(b64(b"two").as_str())
    );
    assert_eq!(modified.size_bytes, Some(3));
    assert_eq!(modified.sha256.as_ref().map(String::len), Some(64));
    assert!(changes.items[1].content_base64.is_none());
    // A symlink an agent leaves behind is never followed or reported.
    #[cfg(unix)]
    {
        let secret = sandbox.work().join("secret");
        std::fs::write(&secret, "host secret").unwrap();
        std::os::unix::fs::symlink(&secret, stage.dir().join("link")).unwrap();
        let (_, changes) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
        assert!(!changes.unwrap().items.iter().any(|i| i.path == "link"));
    }
    stage.remove().unwrap();
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_and_cancelled_leases_return_changes_and_a_lost_epoch_publishes_nothing() {
    let sandbox = support::sandbox(
        "worker::lease_context::failed_and_cancelled_leases_return_changes_and_a_lost_epoch_publishes_nothing",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let resolver = host(&sandbox, "host");
    let worker = linked(&server, "w", &capable()).await;
    let mock = server.server();
    mock.enqueue_with_context(
        item("fails", Some(context(&[("a", b"1")], 64))),
        chrono::Utc::now(),
    )
    .unwrap();
    // A cancelled run returns its safe changes with the partial result.
    let lease = claim(&worker, "fails").await;
    let stage = lease_context::stage(&resolver, &LeaseId::new(), &lease.item)
        .unwrap()
        .unwrap();
    std::fs::write(stage.dir().join("a"), "2").unwrap();
    let cancelled = CommandResult {
        outcome: Some(CommandStatusKind::Cancelled),
        exit_code: Some(130),
        ..Default::default()
    };
    let (result, changes) = lease_context::collect(&stage, cancelled, &AtomicBool::new(false));
    assert_eq!(result.outcome, Some(CommandStatusKind::Cancelled));
    worker
        .release(
            &LeaseRef::of(&lease),
            &ReleaseRequest {
                context_changes: changes,
                reason: ReleaseReason::Shutdown,
                message: None,
                result: Some(result),
            },
        )
        .await
        .unwrap();
    stage.remove().unwrap();
    assert_eq!(mock.context_for(REPO).unwrap().files, files(&[("a", "2")]));

    // A lost epoch: the old lease's report is refused and changes nothing.
    mock.enqueue_with_context(item("lost", None), chrono::Utc::now())
        .unwrap();
    let old = claim(&worker, "lost").await;
    let stage = lease_context::stage(&resolver, &LeaseId::new(), &old.item)
        .unwrap()
        .unwrap();
    std::fs::write(stage.dir().join("a"), "stale").unwrap();
    let (result, changes) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
    mock.expire_lease_reusing_id(&old.id);
    let current = claim(&worker, "lost").await;
    assert!(current.epoch > old.epoch);
    let refused = worker
        .result(&LeaseRef::of(&old), &lease_result(result, changes))
        .await
        .unwrap_err();
    assert!(matches!(refused, WorkerEngineError::LeaseLost), "{refused}");
    stage.remove().unwrap();
    assert_eq!(mock.context_for(REPO).unwrap().files, files(&[("a", "2")]));
    worker
        .result(&LeaseRef::of(&current), &lease_result(done(), None))
        .await
        .unwrap();
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_workers_fail_closed_and_capable_workers_advertise_the_flag() {
    let sandbox = support::sandbox(
        "worker::lease_context::unsupported_workers_fail_closed_and_capable_workers_advertise_the_flag",
    );
    const { assert!(lease_context::SUPPORTED) };
    let status = awman::command::daemon_api::capabilities::advertised(false, &capable());
    assert_eq!(status.iter().filter(|c| *c == "lease_context").count(), 1);
    assert!(
        !awman::command::daemon_api::capabilities::advertised(false, &[])
            .iter()
            .any(|c| c == "lease_context")
    );
    let it = item("ctx", Some(context(&[("a", b"1")], 64)));
    // The foreground host registers no context feature: refused in admission.
    assert!(lease_context::admit(&it, &[]).is_err());
    assert!(lease_context::admit(&it, &capable()).is_ok());
    assert!(lease_context::admit(&item("plain", None), &[]).is_ok());
    // The server never offers a context item to a worker without the feature.
    let server = FakeWorkServer::start(&sandbox).await;
    let plain = linked(&server, "plain", &[]).await;
    server
        .server()
        .enqueue_with_context(it, chrono::Utc::now())
        .unwrap();
    assert!(!plain
        .queue(50, None)
        .await
        .unwrap()
        .items
        .iter()
        .any(|i| i.id == "ctx"));
    let refused = plain.claim(Some("ctx"), 0).await.unwrap_err();
    assert!(
        matches!(refused, WorkerEngineError::Ineligible),
        "{refused}"
    );
    let capable_worker = linked(&server, "capable", &capable()).await;
    let lease = claim(&capable_worker, "ctx").await;
    capable_worker
        .result(&LeaseRef::of(&lease), &lease_result(done(), None))
        .await
        .unwrap();
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphaned_staged_directories_are_removed_without_following_links() {
    let sandbox = support::sandbox(
        "worker::lease_context::orphaned_staged_directories_are_removed_without_following_links",
    );
    let resolver = host(&sandbox, "host");
    let it = item("x", Some(context(&[("a", b"1")], 64)));
    let (live, dead) = (LeaseId::new(), LeaseId::new());
    let keep = lease_context::stage(&resolver, &live, &it)
        .unwrap()
        .unwrap();
    let gone = lease_context::stage(&resolver, &dead, &it)
        .unwrap()
        .unwrap();
    let gone_dir = gone.dir().to_path_buf();
    std::mem::forget(gone); // a crashed worker left it behind
    #[cfg(unix)]
    let target = {
        let target = sandbox.work().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&target, resolver.lease_repo_root().join("planted")).unwrap();
        target
    };
    let expected = if cfg!(unix) { 2 } else { 1 };
    assert_eq!(lease_context::remove_orphans(&resolver, &[live]), expected);
    assert!(!gone_dir.exists());
    assert!(keep.dir().exists());
    #[cfg(unix)]
    assert!(target.join("keep").exists());
    assert_eq!(lease_context::remove_orphans(&resolver, &[]), 1);
    assert!(!keep.dir().exists());
    sandbox.finish();
}

/// An admin that ignores server memory: every item keeps the context it was
/// built with, so updates never reach the next lease.
struct ForgetfulAdmin<'a>(&'a awman_wsp::mock::MockWorkServer, bool);

impl awman_wsp::conformance::ConformanceAdmin for ForgetfulAdmin<'_> {
    fn enqueue(&self, item: WorkItem) -> awman_wsp::conformance::AdminFuture<'_> {
        awman_wsp::conformance::ConformanceAdmin::enqueue(self.0, item)
    }
    fn enqueue_at(
        &self,
        item: WorkItem,
        at: chrono::DateTime<chrono::Utc>,
    ) -> awman_wsp::conformance::AdminFuture<'_> {
        self.0.enqueue_at(item, at)
    }
    fn expire_lease_reusing_id<'a>(
        &'a self,
        id: &'a str,
    ) -> awman_wsp::conformance::AdminFuture<'a> {
        awman_wsp::conformance::ConformanceAdmin::expire_lease_reusing_id(self.0, id)
    }
    fn approve_link<'a>(
        &'a self,
        id: &'a str,
        labels: BTreeMap<String, String>,
    ) -> awman_wsp::conformance::AdminFuture<'a> {
        awman_wsp::conformance::ConformanceAdmin::approve_link(self.0, id, labels)
    }
    fn expire_leases(&self) -> awman_wsp::conformance::AdminFuture<'_> {
        awman_wsp::conformance::ConformanceAdmin::expire_leases(self.0)
    }
    fn request_cancel<'a>(&'a self, id: &'a str) -> awman_wsp::conformance::AdminFuture<'a> {
        awman_wsp::conformance::ConformanceAdmin::request_cancel(self.0, id)
    }
}
impl LeaseContextAdmin for ForgetfulAdmin<'_> {
    fn enqueue_with_context(&self, mut item: WorkItem) -> awman_wsp::conformance::AdminFuture<'_> {
        // `true`: also drop the context, so the lease carries no memory.
        if self.1 {
            item.context = None;
        }
        Box::pin(async move { self.0.enqueue(item, chrono::Utc::now()).unwrap() })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_passes_against_the_mock_and_each_probe_can_fail() {
    let sandbox = support::sandbox(
        "worker::lease_context::conformance_passes_against_the_mock_and_each_probe_can_fail",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let report = awman_test_support::conformance::work_server_lease_context(
        server.base_url(),
        server.server().as_ref(),
    )
    .await;
    assert!(report.is_success(), "{:?}", report.failed);
    assert_eq!(
        report.passed.len(),
        awman_wsp::conformance::lease_context::cases().len()
    );
    let base = server.base_url().to_owned();
    let mock = server.server().clone();
    use awman_wsp::conformance::lease_context::run_case;
    for case in [
        "result_changes_reach_the_next_lease",
        "release_changes_reach_the_next_lease",
    ] {
        assert!(
            run_case(&base, &ForgetfulAdmin(&mock, false), case)
                .await
                .is_err(),
            "undetected: {case}"
        );
    }
    assert!(run_case(
        &base,
        &ForgetfulAdmin(&mock, true),
        "context_offered_only_to_capable_workers"
    )
    .await
    .is_err());
    server.shutdown().await;
    // A server that ignores the epoch applies stale changes; the probe sees it.
    let listener = sandbox.ports().tcp_listener();
    listener.set_nonblocking(true).unwrap();
    let faulty = Arc::new(awman_wsp::mock::MockWorkServer::with_fault(
        awman_wsp::mock::MockFault::IgnoreEpoch,
    ));
    let serving = faulty
        .clone()
        .serve_on(tokio::net::TcpListener::from_std(listener).unwrap())
        .await
        .unwrap();
    assert!(run_case(
        &serving.base_url,
        faulty.as_ref(),
        "lost_epoch_changes_are_discarded"
    )
    .await
    .is_err());
    let addr = serving.addr;
    drop(serving);
    awman_test_support::wait_for_async(
        "fault server closed",
        std::time::Duration::from_secs(5),
        || async move {
            tokio::net::TcpStream::connect(addr)
                .await
                .is_err()
                .then_some(())
        },
    )
    .await;
    sandbox.finish();
}

/// Only the store's private temporary directory may exist under `_keyed/`
/// (Layer 0 stages writes there), and it is left empty: no key is created.
fn assert_no_host_keys(resolver: &ContextDirResolver) {
    let keyed = resolver.awman_home().join("context/repo/_keyed");
    let entries: Vec<_> = std::fs::read_dir(&keyed)
        .map(|dir| dir.flatten().collect())
        .unwrap_or_default();
    for entry in entries {
        assert_eq!(entry.file_name(), ".tmp", "a host key was created");
        assert_eq!(std::fs::read_dir(entry.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[test]
fn complete_tree_must_be_stageable_and_staging_never_touches_keyed_memory() {
    let sandbox = support::sandbox("worker::lease_context::complete_tree_must_be_stageable_and_staging_never_touches_keyed_memory");
    let resolver = host(&sandbox, "isolated");
    let keyed = resolver.awman_home().join("context/repo/_keyed");
    let stage = LeaseContextStage::stage(
        &resolver,
        &LeaseId::new(),
        &context(&[("note", b"old")], 1024),
    )
    .unwrap();
    assert!(
        !keyed.exists(),
        "lease staging must not create the keyed namespace"
    );
    // Poison the unrelated keyed namespace; collection and cleanup still work.
    std::os::unix::fs::symlink(sandbox.work().join("absent"), &keyed).unwrap();
    std::fs::write(stage.dir().join("NOTE"), b"new").unwrap();
    let (result, _) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
    assert_eq!(result.failure.unwrap().code, codes::CONTEXT_INVALID);
    std::fs::remove_file(stage.dir().join("NOTE")).unwrap();
    for index in 0..1000 {
        std::fs::write(stage.dir().join(format!("file-{index}")), b"").unwrap();
    }
    let (result, _) = lease_context::collect(&stage, done(), &AtomicBool::new(false));
    assert_eq!(result.failure.unwrap().code, codes::CONTEXT_TOO_LARGE);
    stage.remove().unwrap();
    let next = LeaseContextStage::stage(
        &resolver,
        &LeaseId::new(),
        &context(&[("next", b"safe")], 1024),
    )
    .unwrap();
    assert_eq!(std::fs::read(next.dir().join("next")).unwrap(), b"safe");
    next.remove().unwrap();
}
