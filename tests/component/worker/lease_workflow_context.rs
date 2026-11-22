//! Bounded workflow artifacts attached to pulled lease results.
use crate::support;
use awman::{
    command::worker::lease_workflow_context as policy,
    data::{
        context::CommandInvocationPins,
        fs::{api_paths::ApiPaths, context_dirs::ContextDirResolver},
    },
    engine::worker::{LeaseRef, WspSession},
};
use awman_test_support::{fakes::FakeWorkServer, TestSandbox};
use awman_wsp::{
    conformance, features, ClaimOutcome, ItemWorkflowContext, LeaseResult, RegisterRequest,
    WorkItem,
};
use base64::Engine as _;
use std::{collections::BTreeMap, sync::atomic::AtomicBool, sync::Arc};

fn host(sandbox: &TestSandbox) -> ContextDirResolver {
    ContextDirResolver::at_home(sandbox.work().join("host/.awman"))
}

fn requested(id: &str, max_bytes: u64) -> WorkItem {
    let mut item = awman_wsp::conformance::work_item(id);
    item.workflow_context = Some(ItemWorkflowContext { max_bytes });
    item
}

async fn linked(server: &FakeWorkServer, name: &str, feature: Option<&str>) -> Arc<WspSession> {
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
            capacity: 1,
            features: feature.into_iter().map(str::to_owned).collect(),
        })
        .await
        .unwrap();
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_attachment_reaches_reference_server_and_marks_omitted_content() {
    let sandbox = support::sandbox(
        "worker::lease_workflow_context::bounded_attachment_reaches_reference_server_and_marks_omitted_content",
    );
    let resolver = host(&sandbox);
    let invocation = uuid::Uuid::new_v4();
    let workflow_dir = resolver.workflow_dir(invocation);
    std::fs::create_dir_all(&workflow_dir).unwrap();
    std::fs::write(workflow_dir.join("attached-note.md"), b"note").unwrap();
    std::fs::write(workflow_dir.join("second.md"), b"more").unwrap();
    let paths = ApiPaths::from_root(sandbox.work().join("api"));
    CommandInvocationPins::new(paths.clone())
        .pin("session", "job", 1, invocation)
        .unwrap();
    let mut item = requested("artifact", 4);
    let attachment = policy::collect(
        &paths,
        &resolver,
        &item,
        "session",
        "job",
        1,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        attachment.items.len(),
        2,
        "all safe file metadata is retained"
    );
    assert!(
        attachment.truncated,
        "the second file exceeds the inline cap"
    );
    let note = attachment
        .items
        .iter()
        .find(|file| file.path == "attached-note.md")
        .unwrap();
    assert_eq!(note.size_bytes, 4);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(note.content_base64.as_ref().unwrap())
            .unwrap(),
        b"note"
    );
    assert!(attachment
        .items
        .iter()
        .any(|file| file.content_base64.is_none()));

    let server = FakeWorkServer::start(&sandbox).await;
    let worker = linked(&server, "capable", Some(features::LEASE_WORKFLOW_CONTEXT)).await;
    server
        .server()
        .enqueue(item.clone(), chrono::Utc::now())
        .unwrap();
    let lease = match worker.claim(Some("artifact"), 0).await.unwrap() {
        ClaimOutcome::Leased(lease) => lease,
        ClaimOutcome::Empty => panic!("capable worker was not offered requested artifacts"),
    };
    worker
        .result(
            &LeaseRef::of(&lease),
            &LeaseResult {
                context_changes: None,
                workflow_context: Some(attachment.clone()),
                result: awman::data::fs::api_db::CommandResult {
                    outcome: Some(awman::data::execution_event::CommandStatusKind::Done),
                    exit_code: Some(0),
                    ..Default::default()
                },
                outputs: None,
                outputs_mounted: false,
                duration_ms: 1,
            },
        )
        .await
        .unwrap();
    let accepted = server
        .server()
        .workflow_context_result("artifact")
        .expect("reference server retained the workflow result");
    assert!(accepted.truncated);
    assert_eq!(accepted.items.len(), 2);
    assert_eq!(accepted.items[0].path, "attached-note.md");
    assert_eq!(
        accepted.items[0].content_base64.as_deref(),
        Some("bm90ZQ==")
    );
    assert!(accepted.items[1].content_base64.is_none());
    // Collection is strictly opt-in and remains distinct from repo memory.
    item.workflow_context = None;
    assert!(policy::collect(
        &paths,
        &resolver,
        &item,
        "session",
        "job",
        1,
        &AtomicBool::new(false),
    )
    .is_none());
    server.shutdown().await;
    sandbox.finish();
}

#[test]
fn incapable_worker_is_ineligible_before_work_and_lost_attempt_has_no_artifacts() {
    let mut item = requested("no-cap", 1024);
    let error = policy::admit(&item, &[]).unwrap_err();
    assert!(error.starts_with("ineligible:"));
    assert!(policy::admit(&item, &[features::LEASE_WORKFLOW_CONTEXT]).is_ok());

    let sandbox = support::sandbox(
        "worker::lease_workflow_context::incapable_worker_is_ineligible_before_work_and_lost_attempt_has_no_artifacts",
    );
    let resolver = host(&sandbox);
    let paths = ApiPaths::from_root(sandbox.work().join("api"));
    let invocation = uuid::Uuid::new_v4();
    let workflow_dir = resolver.workflow_dir(invocation);
    std::fs::create_dir_all(&workflow_dir).unwrap();
    std::fs::write(workflow_dir.join("note.md"), b"note").unwrap();
    CommandInvocationPins::new(paths.clone())
        .pin("session", "job", 1, invocation)
        .unwrap();
    // A newer attempt invalidates the old pin. Collection cannot leak attempt 1.
    CommandInvocationPins::new(paths.clone())
        .pin("session", "job", 2, uuid::Uuid::new_v4())
        .unwrap();
    let result = policy::collect(
        &paths,
        &resolver,
        &item,
        "session",
        "job",
        1,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(result.items.is_empty());
    assert!(!result.truncated);
    assert!(awman::command::daemon_api::capabilities::advertised(
        true,
        &[features::LEASE_WORKFLOW_CONTEXT]
    )
    .contains(&"lease_workflow_context".to_owned()));
    item.workflow_context = None;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_mismatch_releases_the_lease_as_ineligible() {
    let sandbox = support::sandbox(
        "worker::lease_workflow_context::capability_mismatch_releases_the_lease_as_ineligible",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let worker = linked(&server, "mismatch", Some(features::LEASE_WORKFLOW_CONTEXT)).await;
    server
        .server()
        .enqueue(requested("mismatch", 128), chrono::Utc::now())
        .unwrap();
    let lease = match worker.claim(Some("mismatch"), 0).await.unwrap() {
        ClaimOutcome::Leased(lease) => lease,
        ClaimOutcome::Empty => {
            panic!("server did not offer the requested item to its capable worker")
        }
    };
    let reason = policy::admit(&lease.item, &[]).unwrap_err();
    assert!(reason.starts_with("ineligible:"));
    worker
        .release(
            &LeaseRef::of(&lease),
            &awman_wsp::ReleaseRequest {
                context_changes: None,
                reason: awman_wsp::ReleaseReason::Ineligible,
                message: Some(reason),
                result: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        server.server().release_for(&lease.id).unwrap().reason,
        awman_wsp::ReleaseReason::Ineligible
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_context_conformance_passes_against_reference_mock() {
    let sandbox = support::sandbox(
        "worker::lease_workflow_context::workflow_context_conformance_passes_against_reference_mock",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let mock = server.server();
    let report = conformance::workflow_context::run(server.base_url(), mock.as_ref()).await;
    assert!(report.is_success(), "{:#?}", report.failed);
    server.shutdown().await;
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn unsafe_agent_names_signal_omission_and_server_cannot_raise_the_inline_cap() {
    use awman::engine::worker::lease_workflow_context::WorkflowArtifactCollector;
    let sandbox = support::sandbox("worker::lease_workflow_context::unsafe_agent_names_signal_omission_and_server_cannot_raise_the_inline_cap");
    let resolver = host(&sandbox);
    let invocation = uuid::Uuid::new_v4();
    let dir = resolver.workflow_dir(invocation);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("safe.md"), b"safe").unwrap();
    std::fs::write(dir.join("log-10:00.md"), b"hidden").unwrap();
    let collector =
        WorkflowArtifactCollector::for_invocation(&resolver, invocation, u64::MAX).unwrap();
    let result = collector.collect(&AtomicBool::new(false));
    assert!(result.truncated);
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].path, "safe.md");
    assert!(result.collection_error.is_none());
    let failed = collector.collect(&AtomicBool::new(true));
    assert!(failed.items.is_empty());
    assert!(failed.truncated);
    assert_eq!(
        failed.collection_error,
        Some(awman_wsp::WorkflowContextCollectionError::Unavailable)
    );
    let budget = WorkflowArtifactCollector::for_invocation(&resolver, invocation, 0)
        .unwrap()
        .collect(&AtomicBool::new(false));
    assert!(budget.truncated);
    assert_eq!(budget.items.len(), 1);
    assert!(budget.items[0].content_base64.is_none());
    assert!(
        budget.collection_error.is_none(),
        "content omitted by the budget is distinct from failed enumeration"
    );

    std::fs::remove_file(dir.join("log-10:00.md")).unwrap();
    // Two individually downloadable files exceed the worker's 16 MiB cap.
    for name in ["a", "b"] {
        std::fs::File::create(dir.join(name))
            .unwrap()
            .set_len(9 << 20)
            .unwrap();
    }
    let result = collector.collect(&AtomicBool::new(false));
    assert!(result.truncated);
    assert_eq!(result.items.len(), 3);
    let decoded: usize = result
        .items
        .iter()
        .filter_map(|i| i.content_base64.as_ref())
        .map(|s| {
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .unwrap()
                .len()
        })
        .sum();
    assert!(decoded <= 16 << 20);
    assert!(result.items.iter().any(|i| i.content_base64.is_none()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_conformance_detects_missing_capability_and_epoch_fences() {
    let sandbox = support::sandbox("worker::lease_workflow_context::artifact_conformance_detects_missing_capability_and_epoch_fences");
    for (case, fault) in [
        (
            "workflow_context_offered_only_to_capable_workers",
            awman_wsp::mock::MockFault::IgnoreWorkflowContextCapability,
        ),
        (
            "lost_epoch_artifacts_are_discarded",
            awman_wsp::mock::MockFault::IgnoreEpoch,
        ),
    ] {
        let mock = Arc::new(awman_wsp::mock::MockWorkServer::with_fault(fault));
        let server = FakeWorkServer::start_with(&sandbox, mock.clone()).await;
        assert!(
            conformance::workflow_context::run_case(server.base_url(), mock.as_ref(), case)
                .await
                .is_err(),
            "undetected defect: {fault:?}"
        );
        server.shutdown().await;
    }
}
