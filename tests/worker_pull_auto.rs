//! Worker end to end (WI 0143): pulled leases against the mock work server
//! (served over HTTP, linked through the device flow) and a local bare
//! repository over smart HTTP.
//!
//! Checklist items: B1 (three items run in queue order), B2 (two leases at
//! once under `maxConcurrentJobs: 2`, through the real
//! `WorkerExecutorShare`), B9/H4/H6 (an item the runtime cannot honour is
//! released `ineligible` before any Git or container work), B12 (heartbeat
//! `cancel: true` stops the run and posts the `cancelled` result), H2
//! (`409 lease_lost` stops the run and discards its result; the old lease is
//! fenced) and H3 (a refused token refresh cancels the running lease and
//! stops claiming).
//!
//! These tests compose Layer 1 primitives; tests/worker_daemon.rs exercises
//! the production Layer 2 controller and Dispatch over the real daemon.
//! Needs git http-backend; missing support is an error unless the explicit
//! AWMAN_TEST_ALLOW_SKIP_REAL_GIT=1 opt-out is set.

#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use awman::data::daemon_state::FeatureName;
use awman::data::execution_event::CommandStatusKind;
use awman::data::worker::JobClass;
use awman::engine::agent_runtime::{Capabilities, ImageAcquisition};
use awman::engine::daemon::executor::SharedExecutor;
use awman::engine::worker::executor_share::WorkerExecutorShare;
use awman::engine::worker::heartbeat::HeartbeatEnd;
use awman::engine::worker::lease_workspace::LeaseRuntimeSupport;
use awman::engine::worker::{LeaseRef, WorkerEngineError};
use awman_wsp::{ReleaseReason, ReleaseRequest};
use chrono::Utc;
use smart_http::{HelperScript, SmartHttpRepo};
use tokio_util::sync::CancellationToken;
use wsp_lease::{claim, commit_file, eventually, item, labels, run_lease, RunOptions, Wsp};

macro_rules! require_http_backend {
    () => {
        if !smart_http::http_backend_available() {
            assert_eq!(std::env::var("AWMAN_TEST_ALLOW_SKIP_REAL_GIT").as_deref(), Ok("1"), "git http-backend is required; install Git or explicitly set AWMAN_TEST_ALLOW_SKIP_REAL_GIT=1");
            eprintln!("skipping: git http-backend is not installed (explicit opt-out)");
            return;
        }
    };
}

/// The capabilities of the sandbox paradigm (`docker-sbx`): no arbitrary
/// host mounts, so no read-only workspace and no `/awman/output`.
fn sandbox_capabilities() -> Capabilities {
    Capabilities {
        arbitrary_host_mounts: false,
        kit_declarative: true,
        has_image_store: false,
        image_acquisition: ImageAcquisition::Kit,
        per_launch_vm_settings: false,
        reports_resource_stats: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_worker_runs_three_items_in_queue_order_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    // Enqueued A, B, C; C has the highest priority, so the queue is C, A, B.
    let now = Utc::now();
    let mut c = item("01JC", &repo.url);
    c.priority = 5;
    wsp.mock
        .enqueue(item("01JA", &repo.url), now - chrono::Duration::seconds(30))
        .unwrap();
    wsp.mock
        .enqueue(item("01JB", &repo.url), now - chrono::Duration::seconds(20))
        .unwrap();
    wsp.mock
        .enqueue(c, now - chrono::Duration::seconds(10))
        .unwrap();
    let worker = wsp.worker("auto", 1, &[], labels(&[]), labels(&[])).await;
    let listed: Vec<String> = worker
        .session
        .queue(50, None)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(listed, ["01JC", "01JA", "01JB"]);

    // `auto`: claim the next item as soon as a slot is free, until empty.
    let mut ran = Vec::new();
    while let Some(lease) = claim(&worker.session, None).await {
        let id = lease.item.id.clone();
        let report = run_lease(
            worker.session.clone(),
            lease,
            commit_file("out.txt", "out\n"),
            RunOptions::new(&tmp.path().join("worker"), helper.program()),
        )
        .await;
        assert!(
            report.result.failure.is_none(),
            "{id}: {:?}",
            report.result.failure
        );
        assert!(report.posted, "{id}: result posted");
        ran.push(id);
    }
    assert_eq!(ran, ["01JC", "01JA", "01JB"], "run in queue order");
    for id in &ran {
        assert!(wsp.mock.result_for(id).is_some(), "{id}");
        assert!(
            repo.branch_sha(&format!("wsp/{id}")).is_some(),
            "{id} pushed"
        );
    }
    assert!(worker
        .session
        .queue(50, None)
        .await
        .unwrap()
        .items
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn max_concurrent_jobs_two_runs_two_leases_concurrently_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let helper = Arc::new(HelperScript::install(&tmp.path().join("helper")));
    let wsp = Wsp::start().await;
    for id in ["01J1", "01J2", "01J3"] {
        wsp.enqueue(item(id, &repo.url));
    }
    // daemon.maxConcurrentJobs 4, daemon.features.worker.maxConcurrentJobs 2.
    let executor = SharedExecutor::new(4);
    executor.set_share(FeatureName::Worker, 2);
    let share = WorkerExecutorShare::new(executor);
    let worker = Arc::new(wsp.worker("pair", 2, &[], labels(&[]), labels(&[])).await);

    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut jobs = Vec::new();
    for n in 0..3 {
        let (share, worker, helper) = (share.clone(), worker.clone(), helper.clone());
        let (running, peak) = (running.clone(), peak.clone());
        let work_root = tmp.path().join(format!("worker-{n}"));
        jobs.push(tokio::spawn(async move {
            // A slot first, then the claim: never hold a lease without one.
            let permit = share.acquire(JobClass::Pull).await;
            let lease = claim(&worker.session, None).await.expect("a lease");
            let agent: wsp_lease::Agent = Box::new(move |clone| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(700));
                running.fetch_sub(1, Ordering::SeqCst);
                std::fs::write(clone.join("x"), "x").unwrap();
                smart_http::commit_all(clone, "x");
                Ok(())
            });
            let report = run_lease(
                worker.session.clone(),
                lease,
                agent,
                RunOptions::new(&work_root, helper.program()),
            )
            .await;
            drop(permit);
            report
        }));
    }
    // While the first two run, the third waits for a slot.
    eventually(Duration::from_secs(30), "two running", || {
        running.load(Ordering::SeqCst) == 2
    })
    .await;
    let snapshot = share.snapshot();
    assert_eq!(
        (snapshot.running_pull, snapshot.waiting_pull),
        (2, 1),
        "{snapshot:?}"
    );
    for job in jobs {
        let report = job.await.unwrap();
        assert!(
            report.result.failure.is_none(),
            "{:?}",
            report.result.failure
        );
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2, "exactly two at once");
    for id in ["01J1", "01J2", "01J3"] {
        assert!(wsp.mock.result_for(id).is_some(), "{id}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_requiring_unsupported_capability_is_released_ineligible_before_run_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let wsp = Wsp::start().await;
    let mut read_only = item("01JRO", &repo.url);
    read_only.exec.read_only = true;
    let mut outputs = item("01JOUT", &repo.url);
    outputs.outputs = Some(awman_wsp::Outputs { max_bytes: 1024 });
    wsp.enqueue(read_only);
    wsp.enqueue(outputs);

    // A sandbox-runtime worker: registration omits what it can never offer.
    let sandbox = LeaseRuntimeSupport::from_capabilities(&sandbox_capabilities());
    assert!(sandbox.features().is_empty(), "{:?}", sandbox.features());
    let worker = wsp
        .worker("sbx", 1, &sandbox.features(), labels(&[]), labels(&[]))
        .await;
    let mut released = Vec::new();
    while let Some(lease) = claim(&worker.session, None).await {
        let refusal = sandbox
            .check(&lease.item, "docker-sbx")
            .expect_err("the sandbox cannot honour this lease");
        worker
            .session
            .release(
                &LeaseRef::of(&lease),
                &ReleaseRequest {
                    reason: ReleaseReason::Ineligible,
                    message: Some(refusal.0.clone()),
                    result: None,
                },
            )
            .await
            .unwrap();
        let recorded = wsp.mock.release_for(&lease.id).expect("release recorded");
        assert_eq!(recorded.reason, ReleaseReason::Ineligible);
        assert!(recorded.result.is_none(), "nothing ran, no partial result");
        released.push((lease.item.id.clone(), refusal.0));
    }
    assert_eq!(released.len(), 2, "{released:?}");
    assert!(released
        .iter()
        .any(|(id, why)| id == "01JRO" && why.contains("read-only")));
    assert!(released
        .iter()
        .any(|(id, why)| id == "01JOUT" && why.contains("/awman/output")));
    assert!(
        repo.requests().is_empty(),
        "nothing was cloned for an ineligible lease"
    );
    // The server excludes this worker for those items from now on…
    assert!(worker
        .session
        .queue(50, None)
        .await
        .unwrap()
        .items
        .is_empty());
    // …while a Docker worker, which supports both, still gets them.
    let docker_runtime = awman::engine::container::ContainerRuntime::docker();
    let docker = LeaseRuntimeSupport::from_capabilities(docker_runtime.capabilities());
    let other = wsp
        .worker("docker", 1, &docker.features(), labels(&[]), labels(&[]))
        .await;
    let lease = claim(&other.session, None)
        .await
        .expect("claimable elsewhere");
    assert!(docker.check(&lease.item, "docker").is_ok());
}

/// Start `id`'s lease held after checkout; returns the run, the hold token
/// and the server lease (for fencing checks).
async fn start_held(
    wsp: &Wsp,
    worker: &wsp_lease::Worker,
    tmp: &std::path::Path,
    helper: &HelperScript,
) -> (
    tokio::task::JoinHandle<wsp_lease::RunReport>,
    CancellationToken,
    awman_wsp::Lease,
) {
    let lease = claim(&worker.session, None).await.expect("a lease");
    let hold = CancellationToken::new();
    let mut options = RunOptions::new(&tmp.join("worker"), helper.program());
    options.hold_until = Some(hold.clone());
    let run = tokio::spawn(run_lease(
        worker.session.clone(),
        lease.clone(),
        commit_file("late.txt", "late"),
        options,
    ));
    let (mock, id) = (wsp.mock.clone(), lease.id.clone());
    eventually(Duration::from_secs(30), "lease running", move || {
        mock.heartbeat_for(&id)
            .is_some_and(|h| h.state == awman_wsp::HeartbeatState::Running)
    })
    .await;
    (run, hold, lease)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_cancel_true_stops_run_and_posts_cancelled_result_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    wsp.enqueue(item("01JHBC", &repo.url));
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    // `hold` is never released: only the heartbeat's cancel ends the run.
    let (run, _hold, lease) = start_held(&wsp, &worker, tmp.path(), &helper).await;
    wsp.mock.request_cancel(&lease.id, "the item was withdrawn");
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("the cancel stops the run within a heartbeat")
        .unwrap();
    assert_eq!(
        report.heartbeat_end,
        Some(HeartbeatEnd::Cancelled {
            reason: Some("the item was withdrawn".into())
        })
    );
    assert_eq!(report.result.outcome, Some(CommandStatusKind::Cancelled));
    assert!(
        report.posted,
        "a cancelled lease reports its partial result"
    );
    let posted: awman::data::fs::api_db::CommandResult =
        serde_json::from_value(wsp.mock.result_for("01JHBC").unwrap().result).unwrap();
    assert_eq!(posted.outcome, Some(CommandStatusKind::Cancelled));
    assert!(
        repo.branch_sha("wsp/01JHBC").is_none(),
        "the agent never ran, nothing pushed"
    );
    assert!(!report.clone_dir.exists(), "lease workspace removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_lost_on_heartbeat_stops_run_and_posts_nothing_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    wsp.enqueue(item("01JLOST", &repo.url));
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let (run, _hold, old) = start_held(&wsp, &worker, tmp.path(), &helper).await;
    // The worker "went offline": the server expired the lease.
    wsp.mock.expire_leases();
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("409 lease_lost stops the run")
        .unwrap();
    assert_eq!(report.heartbeat_end, Some(HeartbeatEnd::LeaseLost));
    assert!(!report.posted, "a lost lease's result is discarded");
    assert!(wsp.mock.result_for("01JLOST").is_none());
    assert!(repo.branch_sha("wsp/01JLOST").is_none());
    // The item was re-queued; the old lease stays fenced.
    let again = claim(&worker.session, None).await.expect("re-queued");
    assert_eq!(again.item.id, "01JLOST");
    assert!(again.epoch > old.epoch);
    let stale = worker
        .session
        .result(
            &LeaseRef::of(&old),
            &awman_wsp::LeaseResult {
                result: awman::data::fs::api_db::CommandResult::default(),
                outputs: None,
                outputs_mounted: false,
                duration_ms: 0,
            },
        )
        .await;
    assert_eq!(stale.unwrap_err(), WorkerEngineError::LeaseLost);
    assert!(
        wsp.mock.result_for("01JLOST").is_none(),
        "never accepted twice"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_revocation_mid_run_cancels_lease_and_stops_claiming_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    wsp.enqueue(item("01JREV", &repo.url));
    wsp.enqueue(item("01JNEXT", &repo.url));
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let revoked = worker.session.revoked();
    let (run, _hold, _lease) = start_held(&wsp, &worker, tmp.path(), &helper).await;
    // The owner revoked the link: the access token stops working and the
    // refresh is refused.
    wsp.mock.revoke(&worker.refresh_token);
    wsp.mock.invalidate_access_tokens();
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("revocation stops the run")
        .unwrap();
    assert_eq!(report.heartbeat_end, Some(HeartbeatEnd::LinkRevoked));
    assert!(
        revoked.is_cancelled(),
        "the session's revocation signal fired"
    );
    assert!(worker.session.is_revoked());
    assert_eq!(report.result.outcome, Some(CommandStatusKind::Cancelled));
    assert!(!report.posted);
    // No further claims from that server.
    let next = worker.session.claim(None, 0).await;
    assert_eq!(next.unwrap_err(), WorkerEngineError::LinkRevoked);
    assert_eq!(
        worker.session.queue(50, None).await.unwrap_err(),
        WorkerEngineError::LinkRevoked
    );
}
