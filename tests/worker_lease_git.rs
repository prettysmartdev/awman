//! Worker end to end (WI 0143): lease Git against a local bare repository
//! served over smart HTTP, with the lease's repository credentials.
//!
//! Covers the checklist items B3 (pushed sha in the result), B4/F2/F6
//! (lease credentials used for clone, fetch, `ls-remote` and push through
//! the one-shot helper only, no system/global Git config, no secret in
//! argv, logs, `.git/config` or files left behind), B5 (auth failure →
//! `git_auth`/`git_auth_failed`) and H5 (expiring clone credentials are
//! refreshed through `POST /v1/leases/{id}/credentials` before the push).
//!
//! Needs `git http-backend`: without it each test fails, unless
//! `AWMAN_TEST_ALLOW_SKIP_REAL_GIT=1` lets it return early (printing why).
//! Names carry `real_git` so `make test-fast` skips them.

#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::sync::Arc;
use std::time::Duration;

use awman::data::config::env::EnvSnapshot;
use awman::data::ids::LeaseId;
use awman::data::message::RecordingMessageSink;
use awman::engine::git::lease_transport::LeaseUrlPolicy;
use awman::engine::git::push_retry::PushRetryPolicy;
use awman::engine::git::GitEngine;
use awman::engine::worker::credential_helper::lease_transport;
use awman::engine::worker::lease_workspace::{LeaseCheckout, LeaseWorkspace};
use awman::engine::worker::push::{LeasePush, LeasePushOutcome, PushObserver, PushRetry};
use smart_http::{files_containing, HelperScript, SmartHttpRepo};
use tokio_util::sync::CancellationToken;
use wsp_lease::{bearer, claim, commit_file, item, labels, run_lease, RunOptions, Wsp};

const TOKEN: &str = "lease-repo-token-9f3c2a7e";

macro_rules! require_http_backend {
    () => {
        if !smart_http::require_http_backend() {
            return;
        }
    };
}

struct NoopObserver;
impl PushObserver for NoopObserver {
    fn retry(&self, _: &PushRetry) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn result_pushed_sha_matches_bare_repo_branch_head_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), Some(TOKEN)).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    let mut work = item("01JSHA", &repo.url);
    work.repo.credentials = Some(bearer(TOKEN, None));
    wsp.enqueue(work);
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let lease = claim(&worker.session, None).await.expect("a lease");

    let report = run_lease(
        worker.session.clone(),
        lease,
        commit_file("change.txt", "done\n"),
        RunOptions::new(&tmp.path().join("worker"), helper.program()),
    )
    .await;

    assert!(
        report.result.failure.is_none(),
        "{:?}",
        report.result.failure
    );
    let git_result = report.result.git.clone().expect("git result");
    let pushed = git_result.head_sha.expect("head sha");
    assert_eq!(
        repo.branch_sha("wsp/01JSHA").as_deref(),
        Some(pushed.as_str()),
        "the result's sha is the bare repository's branch head"
    );
    assert!(report.posted);
    let posted = wsp.mock.result_for("01JSHA").expect("result posted");
    let posted: awman::data::fs::api_db::CommandResult =
        serde_json::from_value(posted.result).unwrap();
    assert_eq!(
        posted.git.and_then(|g| g.head_sha).as_deref(),
        Some(pushed.as_str()),
        "the WSP result embeds the CommandResult with the pushed sha"
    );
    // The lease workspace is gone after the run.
    assert!(!report.clone_dir.exists());
}

/// A global and a system Git config that would redirect the lease URL and
/// run a credential helper writing `marker`, if Git read them.
fn hostile_env(dir: &std::path::Path, url: &str, marker: &std::path::Path) -> EnvSnapshot {
    let cfg = dir.join("hostile.gitconfig");
    std::fs::write(
        &cfg,
        format!(
            "[url \"http://127.0.0.1:1/\"]\n\tinsteadOf = {url}\n\
             [credential]\n\thelper = \"!f() {{ touch '{}'; echo password=hostile; }}; f\"\n",
            marker.display()
        ),
    )
    .unwrap();
    let cfg = cfg.to_string_lossy().into_owned();
    EnvSnapshot::with_overrides(std::env::vars().chain([
        ("GIT_CONFIG_GLOBAL".to_string(), cfg.clone()),
        ("GIT_CONFIG_SYSTEM".to_string(), cfg),
    ]))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_credentials_used_for_clone_fetch_ls_remote_and_push_via_helper_only_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), Some(TOKEN)).await;
    repo.publish_branch(tmp.path(), "wsp/dep", "dep.txt");
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let marker = tmp.path().join("hostile-helper-ran");
    let env = hostile_env(tmp.path(), &repo.url, &marker);

    let mut work = item("01JCRED", &repo.url);
    work.repo.merge_refs = vec!["wsp/dep".into()];
    work.repo.credentials = Some(bearer(TOKEN, None));
    let work_root = tmp.path().join("worker");
    let ws = LeaseWorkspace::prepare(&work_root, &LeaseId::new()).unwrap();
    let transport =
        lease_transport(&ws, &work, helper.program(), LeaseUrlPolicy::default()).unwrap();
    let credential_file = transport.helper().unwrap().file().to_path_buf();
    let clone = ws.clone_dir().to_path_buf();

    // Clone + checkout + merge ref, then a fetch and an ls-remote, all through
    // the lease transport with a hostile global/system config in the env.
    let repo_url = repo.url.clone();
    let phases = Arc::new(repo);
    let (ws, transport, messages) = {
        let phases = phases.clone();
        let env = env.clone();
        let work = work.clone();
        tokio::task::spawn_blocking(move || {
            let git_engine = GitEngine::with_env(env);
            let cancel = CancellationToken::new();
            let mut sink = RecordingMessageSink::new();
            phases.set_phase("clone");
            LeaseCheckout::new(&git_engine, &transport)
                .run(&ws, &work.repo, &mut sink, &cancel)
                .expect("lease checkout");
            phases.set_phase("fetch");
            git_engine
                .fetch_with_lease_transport(
                    ws.clone_dir(),
                    &["+refs/heads/main:refs/remotes/origin/main".to_string()],
                    &transport,
                    &mut sink,
                    &cancel,
                )
                .expect("lease fetch");
            phases.set_phase("ls-remote");
            let refs = git_engine
                .ls_remote_with_lease_transport(&work.repo.url, &transport, &mut sink, &cancel)
                .expect("lease ls-remote");
            assert!(refs.contains_key("refs/heads/wsp/dep"), "{refs:?}");
            let messages: Vec<String> = sink.queued().iter().map(|m| m.text.clone()).collect();
            (ws, transport, messages)
        })
        .await
        .unwrap()
    };
    assert!(clone.join("dep.txt").exists(), "merge ref merged");
    std::fs::write(clone.join("agent.txt"), "agent\n").unwrap();
    smart_http::commit_all(&clone, "agent");

    phases.set_phase("push");
    let push_git = GitEngine::with_env(env)
        .isolate_remote_push(&clone, &repo_url)
        .unwrap()
        .with_lease_transport(transport.clone());
    let pushed = LeasePush::new(
        Arc::new(push_git),
        "wsp/01JCRED",
        PushRetryPolicy::for_lease(
            chrono::Utc::now() + chrono::Duration::minutes(5),
            Duration::from_secs(1),
        ),
        Duration::from_secs(1),
    )
    .run(&CancellationToken::new(), &NoopObserver)
    .await
    .expect("lease push");
    assert!(matches!(pushed, LeasePushOutcome::Pushed { .. }));
    assert_eq!(
        phases.branch_sha("wsp/01JCRED").as_deref(),
        Some(pushed.sha())
    );

    // Every operation reached the server and authenticated with the lease
    // credential (Basic, x-access-token:<token>) after Git's 401 challenge.
    for phase in ["clone", "fetch", "ls-remote", "push"] {
        let seen = phases.requests_in(phase);
        assert!(!seen.is_empty(), "{phase}: no requests reached the server");
        assert!(
            seen.iter()
                .any(|r| r.authorized && r.auth_scheme == "basic"),
            "{phase}: never authenticated: {seen:?}"
        );
        assert!(
            seen.iter().all(|r| r.authorized || r.status == 401),
            "{phase}: {seen:?}"
        );
    }
    assert!(phases
        .requests_in("push")
        .iter()
        .any(|r| r.is_push() && r.authorized));
    assert!(phases.saw_credential(&format!("x-access-token:{TOKEN}")));

    // The hostile global/system config never applied.
    assert!(!marker.exists(), "a global/system credential helper ran");
    // The helper ran with Git config isolation and never saw the secret on argv.
    let log = helper.log();
    assert!(!log.is_empty(), "the lease helper was used");
    for line in log.lines() {
        assert!(line.contains("nosystem=1"), "{line}");
        assert!(line.contains("global=/dev/null"), "{line}");
        assert!(!line.contains(TOKEN), "secret on helper argv: {line}");
    }
    // No secret in logs, .git/config or anywhere in the clone.
    assert!(messages.iter().all(|m| !m.contains(TOKEN)), "{messages:?}");
    let config = std::fs::read_to_string(clone.join(".git/config")).unwrap();
    assert!(
        !config.contains(TOKEN) && !config.contains("x-access-token"),
        "{config}"
    );
    assert!(files_containing(&clone, TOKEN).is_empty());
    // The only file holding it is the helper's owner-only file outside the clone.
    let holders = files_containing(&work_root, TOKEN);
    assert_eq!(holders, vec![credential_file.clone()], "{holders:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&credential_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    drop(transport);
    drop(ws);
    assert!(
        files_containing(&work_root, TOKEN).is_empty(),
        "secret left behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_lease_credentials_yield_failure_stage_git_auth_code_git_auth_failed_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), Some(TOKEN)).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    let mut work = item("01JAUTH", &repo.url);
    work.repo.credentials = Some(bearer("not-the-token", None));
    wsp.enqueue(work);
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let lease = claim(&worker.session, None).await.unwrap();

    let report = run_lease(
        worker.session.clone(),
        lease,
        commit_file("x", "x"),
        RunOptions::new(&tmp.path().join("worker"), helper.program()),
    )
    .await;
    let failure = report.result.failure.clone().expect("failure");
    assert_eq!(
        (failure.stage.as_str(), failure.code.as_str()),
        ("git_auth", "git_auth_failed"),
        "{failure:?}"
    );
    assert!(!failure.message.contains("not-the-token"), "{failure:?}");
    assert!(report.posted, "an auth failure is still reported");
    let posted: awman::data::fs::api_db::CommandResult =
        serde_json::from_value(wsp.mock.result_for("01JAUTH").unwrap().result).unwrap();
    assert_eq!(posted.failure.unwrap().code, "git_auth_failed");
    assert!(repo.branch_sha("wsp/01JAUTH").is_none());

    // No credentials at all against a server that requires them: the same.
    let wsp2 = Wsp::start().await;
    wsp2.enqueue(item("01JANON", &repo.url));
    let worker = wsp2.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let lease = claim(&worker.session, None).await.unwrap();
    let report = run_lease(
        worker.session.clone(),
        lease,
        commit_file("x", "x"),
        RunOptions::new(&tmp.path().join("worker2"), helper.program()),
    )
    .await;
    let failure = report.result.failure.expect("failure");
    assert_eq!(failure.code, "git_auth_failed", "{failure:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_repo_credentials_refreshed_before_push_real_git() {
    require_http_backend!();
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), Some("clone-token-1")).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let wsp = Wsp::start().await;
    let mut work = item("01JREFRESH", &repo.url);
    // Expires inside the 120 s refresh margin: the push must refresh first.
    work.repo.credentials = Some(bearer("clone-token-1", Some(chrono::Duration::seconds(30))));
    wsp.enqueue(work);
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let lease = claim(&worker.session, None).await.unwrap();
    let lease_id = lease.id.clone();

    // The clone runs with token 1; then the server rotates to token 2 and
    // hands it out on POST /v1/leases/{id}/credentials only.
    let rotate = {
        let hold = CancellationToken::new();
        let mut options = RunOptions::new(&tmp.path().join("worker"), helper.program());
        options.hold_until = Some(hold.clone());
        (options, hold)
    };
    let (options, hold) = rotate;
    let session = worker.session.clone();
    let run = tokio::spawn(run_lease(session, lease, commit_file("r", "r"), options));
    wsp_lease::eventually(Duration::from_secs(30), "clone finished", || {
        !repo.requests().is_empty()
            && wsp
                .mock
                .heartbeat_for(&lease_id)
                .is_some_and(|h| h.state == awman_wsp::HeartbeatState::Running)
    })
    .await;
    repo.set_token(Some("push-token-2"));
    wsp.mock.set_credentials(
        "01JREFRESH",
        bearer("push-token-2", Some(chrono::Duration::hours(1))),
    );
    repo.set_phase("push");
    hold.cancel();
    let report = run.await.unwrap();

    assert!(
        report.result.failure.is_none(),
        "{:?}",
        report.result.failure
    );
    assert!(repo.branch_sha("wsp/01JREFRESH").is_some());
    assert!(repo.saw_credential("push-token-2"));
    assert!(repo
        .requests_in("push")
        .iter()
        .any(|r| r.is_push() && r.authorized));
}
