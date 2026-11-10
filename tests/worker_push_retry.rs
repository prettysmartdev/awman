//! Worker end to end (WI 0143), checklist B11: a lease push retries only
//! failures before a remote response, within the lease's expiry, records
//! each retry in the log, honours cancellation and never retries an auth
//! failure or a remote rejection. Against the mock work server and a local
//! bare repository over smart HTTP whose listener can reset connections.
//!
//! Needs `git http-backend` (tests fail without it unless
//! `AWMAN_TEST_ALLOW_SKIP_REAL_GIT=1`, then return early); names carry
//! `real_git` so `make test-fast` skips them.

#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::time::{Duration, Instant};

use awman::data::execution_event::CommandStatusKind;
use awman::engine::git::push_retry::PushRetryPolicy;
use smart_http::{HelperScript, SmartHttpRepo};
use tokio_util::sync::CancellationToken;
use wsp_lease::{bearer, claim, commit_file, eventually, item, labels, run_lease, RunOptions, Wsp};

const TOKEN: &str = "push-retry-token-31d7";

macro_rules! require_http_backend {
    () => {
        if !smart_http::require_http_backend() {
            return;
        }
    };
}

fn fast_policy() -> PushRetryPolicy {
    PushRetryPolicy {
        max_attempts: 3,
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(100),
        deadline: chrono::Utc::now() + chrono::Duration::seconds(60),
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: SmartHttpRepo,
    helper: HelperScript,
    wsp: Wsp,
}

async fn fixture(id: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let repo = SmartHttpRepo::start(&root, Some(TOKEN)).await;
    let helper = HelperScript::install(&root.join("helper"));
    let wsp = Wsp::start().await;
    let mut work = item(id, &repo.url);
    work.repo.credentials = Some(bearer(TOKEN, None));
    wsp.enqueue(work);
    Fixture {
        _tmp: tmp,
        root,
        repo,
        helper,
        wsp,
    }
}

/// Start the lease held after checkout; returns the run, the release
/// token and the server lease ID once the checkout finished.
async fn start_held(
    f: &Fixture,
    agent: wsp_lease::Agent,
    policy: Option<PushRetryPolicy>,
) -> (
    tokio::task::JoinHandle<wsp_lease::RunReport>,
    CancellationToken,
    String,
) {
    let worker = f.wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let lease = claim(&worker.session, None).await.expect("a lease");
    let lease_id = lease.id.clone();
    let hold = CancellationToken::new();
    let mut options = RunOptions::new(&f.root.join("worker"), f.helper.program());
    options.hold_until = Some(hold.clone());
    options.push_policy = policy;
    let run = tokio::spawn(run_lease(worker.session.clone(), lease, agent, options));
    let mock = f.wsp.mock.clone();
    let id = lease_id.clone();
    eventually(Duration::from_secs(30), "checkout finished", move || {
        mock.heartbeat_for(&id)
            .is_some_and(|h| h.state == awman_wsp::HeartbeatState::Running)
    })
    .await;
    f.repo.set_phase("push");
    (run, hold, lease_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_connection_reset_then_succeeds_and_logs_push_retry_events_real_git() {
    require_http_backend!();
    let f = fixture("01JRESET").await;
    let (run, hold, _) = start_held(&f, commit_file("a", "a"), Some(fast_policy())).await;
    f.repo.reset_next_connections(1);
    hold.cancel();
    let report = run.await.unwrap();

    assert!(
        report.result.failure.is_none(),
        "{:?}",
        report.result.failure
    );
    assert_eq!(f.repo.resets(), 1, "the first push connection was reset");
    assert_eq!(report.retries.len(), 1, "{:?}", report.retries);
    assert_eq!(report.retries[0].attempt, 2);
    assert!(
        report.messages.iter().any(|m| {
            let message = m.to_ascii_lowercase();
            m.starts_with("push retry 2/3")
                && (message.contains("reset") || message.contains("could not connect to server"))
        }),
        "retry logged with its pre-response transport failure: {:?}",
        report.messages
    );
    let sha = report.result.git.unwrap().head_sha.unwrap();
    assert_eq!(f.repo.branch_sha("wsp/01JRESET"), Some(sha));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_retry_401_or_rejected_push_real_git() {
    require_http_backend!();
    // 401: the server stops accepting the lease token after the clone.
    let f = fixture("01JNO401").await;
    let (run, hold, _) = start_held(&f, commit_file("a", "a"), Some(fast_policy())).await;
    f.repo.set_token(Some("rotated-elsewhere"));
    hold.cancel();
    let report = run.await.unwrap();
    let failure = report.result.failure.clone().expect("failure");
    assert_eq!(
        (failure.stage.as_str(), failure.code.as_str()),
        ("git_auth", "git_auth_failed"),
        "{failure:?}"
    );
    assert!(
        report.retries.is_empty(),
        "auth is never retried: {:?}",
        report.retries
    );
    assert!(f.repo.branch_sha("wsp/01JNO401").is_none());

    // Remote rejection: a pre-receive hook declines the push.
    let f = fixture("01JREJECT").await;
    let (run, hold, _) = start_held(&f, commit_file("a", "a"), Some(fast_policy())).await;
    f.repo.reject_pushes();
    hold.cancel();
    let report = run.await.unwrap();
    let failure = report.result.failure.clone().expect("failure");
    assert_eq!(
        (failure.stage.as_str(), failure.code.as_str()),
        ("push", "push_failed"),
        "{failure:?}"
    );
    assert!(
        report.retries.is_empty(),
        "rejection is never retried: {:?}",
        report.retries
    );
    let pushes = f
        .repo
        .requests_in("push")
        .into_iter()
        .filter(|r| r.is_push() && r.method == "POST" && r.authorized)
        .count();
    assert_eq!(pushes, 1, "exactly one push attempt reached the remote");
    assert!(f.repo.branch_sha("wsp/01JREJECT").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_backoff_stops_retrying_real_git() {
    require_http_backend!();
    let f = fixture("01JCANCEL").await;
    let mut policy = fast_policy();
    // The backoff must fit inside the mock lease window (3 s TTL minus two
    // heartbeats), or the policy gives up instead of waiting.
    policy.base_backoff = Duration::from_millis(1500);
    policy.max_backoff = Duration::from_millis(1500);
    let (run, hold, lease_id) = start_held(&f, commit_file("a", "a"), Some(policy)).await;
    f.repo.reset_next_connections(1000);
    hold.cancel();
    // Wait for the first failed attempt, then cancel through the heartbeat.
    eventually(Duration::from_secs(30), "first push attempt", || {
        f.repo.resets() > 0
    })
    .await;
    let cancelled_at = Instant::now();
    f.wsp.mock.request_cancel(&lease_id, "operator cancelled");
    let report = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("cancel ends the backoff")
        .unwrap();
    assert!(cancelled_at.elapsed() < Duration::from_secs(20));
    // Without the cancel, attempt 2 would follow the backoff and be reset too.
    assert_eq!(report.retries.len(), 1, "{:?}", report.retries);
    assert_eq!(report.result.outcome, Some(CommandStatusKind::Cancelled));
    assert_eq!(report.result.failure.as_ref().unwrap().code, "cancelled");
    assert_eq!(f.repo.resets(), 1, "no attempt after the cancel");
    assert!(
        report.posted,
        "a cancelled lease still reports its partial result"
    );
    let posted: awman::data::fs::api_db::CommandResult =
        serde_json::from_value(f.wsp.mock.result_for("01JCANCEL").unwrap().result).unwrap();
    assert_eq!(posted.outcome, Some(CommandStatusKind::Cancelled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn already_published_head_counts_as_success_real_git() {
    require_http_backend!();
    let f = fixture("01JLANDED").await;
    let bare = f.repo.bare.clone();
    // The agent commits, and "an earlier attempt" already landed that head.
    let agent: wsp_lease::Agent = Box::new(move |clone| {
        std::fs::write(clone.join("a"), "a").unwrap();
        smart_http::commit_all(clone, "a");
        smart_http::git(
            clone,
            &[
                "push",
                "-q",
                bare.to_str().unwrap(),
                "HEAD:refs/heads/wsp/01JLANDED",
            ],
        );
        Ok(())
    });
    let (run, hold, _) = start_held(&f, agent, Some(fast_policy())).await;
    hold.cancel();
    let report = run.await.unwrap();
    assert!(
        report.result.failure.is_none(),
        "{:?}",
        report.result.failure
    );
    let sha = report.result.git.unwrap().head_sha.unwrap();
    assert_eq!(f.repo.branch_sha("wsp/01JLANDED"), Some(sha));
    assert!(
        !f.repo
            .requests_in("push")
            .iter()
            .any(|r| r.is_push() && r.method == "POST"),
        "no second push of an already-published head"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_window_bounded_by_lease_expiry_yields_push_unreachable_real_git() {
    require_http_backend!();
    // The production policy (4 attempts, 2 s doubling) against the mock's
    // 3 s lease TTL: the window (expiry − 2 × heartbeat) closes before the
    // attempts run out.
    let f = fixture("01JWINDOW").await;
    let (run, hold, _) = start_held(&f, commit_file("a", "a"), None).await;
    f.repo.reset_next_connections(1000);
    let started = Instant::now();
    hold.cancel();
    let report = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the window ends the push")
        .unwrap();
    let failure = report.result.failure.clone().expect("failure");
    assert_eq!(
        (failure.stage.as_str(), failure.code.as_str()),
        ("push", "push_unreachable"),
        "{failure:?}"
    );
    assert!(
        report.retries.len() < 3,
        "bounded by the lease expiry, not the attempt limit: {:?}",
        report.retries
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(f.repo.branch_sha("wsp/01JWINDOW").is_none());
}
