//! Worker end to end (WI 0143), checklist G3: a pushed job and a pulled job
//! run at the same time inside `daemon.features.worker.maxConcurrentJobs`
//! (one `JobClass::Push` and one `JobClass::Pull` slot of the real
//! `WorkerExecutorShare`), with one event stream shape and one
//! `CommandResult` shape.
//!
//! Both jobs are command rows on the real push API (one row per lease, plan
//! D1). The pulled job's persisted log is also forwarded to the mock work
//! server with the real `EventForwarder`, and its `CommandResult` is posted
//! as the WSP result. The test then compares the push SSE stream with the
//! pulled job's SSE and WSP streams, and the two results.

#[path = "helpers/push_api.rs"]
mod push_api;
#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use awman::data::daemon_state::FeatureName;
use awman::data::execution_event::{CommandStatusKind, EventPayload};
use awman::data::fs::api_db::{CommandResult, GitResult};
use awman::data::worker::JobClass;
use awman::engine::daemon::executor::SharedExecutor;
use awman::engine::worker::events::{ChannelEventSource, EventForwarder};
use awman::engine::worker::executor_share::WorkerExecutorShare;
use awman::engine::worker::{LeaseApi, LeaseRef};
use serde_json::Value;
use tokio::sync::{mpsc, Barrier};
use tokio_util::sync::CancellationToken;
use wsp_lease::{claim, item, labels, Wsp};

const PUSHED: &str = "cmd_11111111111111111111111111111111";
const PULLED: &str = "cmd_22222222222222222222222222222222";
const PULL_SESSION: &str = "ses_22222222222222222222222222222222";

/// The structural shape of an event: its keys and its payload's keys.
fn shape(event: &Value) -> (BTreeSet<String>, String, BTreeSet<String>) {
    let keys = |v: &Value| {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let payload = &event["payload"];
    (
        keys(event),
        payload["type"].as_str().unwrap_or_default().to_string(),
        keys(&payload["data"]),
    )
}

fn result(branch: &str, sha: &str) -> CommandResult {
    CommandResult {
        exit_code: Some(0),
        outcome: Some(CommandStatusKind::Done),
        git: Some(GitResult {
            branch: Some(branch.into()),
            head_sha: Some(sha.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The same work, as both kinds of job emit it.
fn emit_run(attempt: &push_api::Attempt, label: &str) {
    attempt.emit(EventPayload::StatusMessage {
        phase: "setup".into(),
        message: format!("preparing {label}"),
    });
    attempt.emit(EventPayload::WorkflowState(serde_json::json!({
        "steps": [{ "name": "implement", "status": "running" }]
    })));
    attempt.emit(EventPayload::StdoutLine(format!("{label} working")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushed_and_pulled_jobs_run_concurrently_with_identical_event_and_result_shapes() {
    let tmp = tempfile::tempdir().unwrap();
    let state = push_api::make_state(&tmp.path().join("api"));
    let base = push_api::serve(state.clone()).await;
    let wsp = Wsp::start().await;
    wsp.enqueue(item("01JMIX", "https://example.invalid/repo.git"));
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;

    // daemon.features.worker.maxConcurrentJobs = 2 shared by push and pull.
    let executor = SharedExecutor::new(4);
    executor.set_share(FeatureName::Worker, 2);
    let share = WorkerExecutorShare::new(executor);
    let both_running = Arc::new(Barrier::new(3));

    push_api::enqueue(&state, PUSHED);
    // The lease runs in its own clone: its own session.
    push_api::add_session(&state, PULL_SESSION);
    push_api::enqueue_in(&state, PULL_SESSION, PULLED);

    let pushed = {
        let (share, state, barrier) = (share.clone(), state.clone(), both_running.clone());
        tokio::spawn(async move {
            let permit = share.acquire(JobClass::Push).await;
            let attempt = push_api::start_attempt(&state, PUSHED, None).await;
            emit_run(&attempt, "pushed");
            barrier.wait().await;
            let json =
                serde_json::to_string(&result("feature/pushed", "a".repeat(40).as_str())).unwrap();
            attempt.finish(&state, PUSHED, Some(&json)).await;
            drop(permit);
        })
    };
    // Claim order matters for the store: the push row is claimed first.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pulled = {
        let (share, state, barrier) = (share.clone(), state.clone(), both_running.clone());
        let session = worker.session.clone();
        tokio::spawn(async move {
            let permit = share.acquire(JobClass::Pull).await;
            let lease = claim(&session, None).await.expect("lease");
            let (tee, rx) = mpsc::channel(64);
            let attempt = push_api::start_attempt(&state, PULLED, Some(tee)).await;
            let api: Arc<dyn LeaseApi> = session.clone();
            let forwarder = tokio::spawn(EventForwarder::run(
                api,
                LeaseRef::of(&lease),
                Box::new(ChannelEventSource::new(rx).with_window(Duration::from_millis(20))),
                CancellationToken::new(),
            ));
            emit_run(&attempt, "pulled");
            barrier.wait().await;
            let res = result("wsp/01JMIX", "b".repeat(40).as_str());
            let json = serde_json::to_string(&res).unwrap();
            attempt.finish(&state, PULLED, Some(&json)).await;
            forwarder.await.unwrap().unwrap();
            session
                .result(
                    &LeaseRef::of(&lease),
                    &awman_wsp::LeaseResult {
                        result: res,
                        outputs: None,
                        outputs_mounted: false,
                        duration_ms: 1,
                    },
                )
                .await
                .unwrap();
            drop(permit);
            lease.id
        })
    };
    // Both are inside the share at the same moment.
    tokio::time::timeout(Duration::from_secs(20), both_running.wait())
        .await
        .expect("both jobs running at once");
    let snapshot = share.snapshot();
    assert_eq!(
        (snapshot.running_push, snapshot.running_pull),
        (1, 1),
        "{snapshot:?}"
    );
    pushed.await.unwrap();
    let lease_id = pulled.await.unwrap();

    // One event stream shape.
    let push_sse: Vec<Value> = push_api::Sse::open(&base, PUSHED)
        .await
        .until_done()
        .await
        .iter()
        .map(push_api::Frame::json)
        .collect();
    let pull_sse: Vec<Value> = push_api::Sse::open(&base, PULLED)
        .await
        .until_done()
        .await
        .iter()
        .map(push_api::Frame::json)
        .collect();
    let pull_wsp = wsp.mock.events_for(&lease_id);
    assert_eq!(
        pull_sse, pull_wsp,
        "a pulled job's SSE and WSP streams are the same events"
    );
    assert_eq!(push_sse.len(), pull_wsp.len());
    let push_shapes: Vec<_> = push_sse.iter().map(shape).collect();
    let pull_shapes: Vec<_> = pull_wsp.iter().map(shape).collect();
    assert_eq!(
        push_shapes, pull_shapes,
        "pushed and pulled events have one shape"
    );

    // One CommandResult shape: the WSP result embeds the CommandResult
    // unchanged, and the pushed row's stored result is the same type.
    let wsp_result = wsp.mock.result_for("01JMIX").unwrap().result;
    let pulled_result: CommandResult = serde_json::from_value(wsp_result.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&pulled_result).unwrap(),
        wsp_result,
        "lossless"
    );
    let pushed_row: Value = reqwest::get(format!("{base}/v1/worker/commands/{PUSHED}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pulled_row: Value = reqwest::get(format!("{base}/v1/worker/commands/{PULLED}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let keys = |v: &Value| -> BTreeSet<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    assert_eq!(
        keys(&pushed_row["result"]),
        keys(&pulled_row["result"]),
        "{pushed_row}\n{pulled_row}"
    );
    let pulled_projection: Value = pulled_row["result"].clone();
    for key in keys(&wsp_result) {
        assert_eq!(
            pulled_projection.get(&key),
            wsp_result.get(&key),
            "the WSP result and the command's result agree on `{key}`"
        );
    }
}
