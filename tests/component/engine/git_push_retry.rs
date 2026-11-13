//! The shared push loop (`PushPublisher`) with a push-session transport
//! (`HostGitTransport::user`, allowed global configuration) against a
//! Smart-HTTP server whose requests fail at the transport level on demand.
use crate::support;
use awman::data::config::env::EnvSnapshot;
use awman::data::git_transport::{GitTransportOp, SafeRemote};
use awman::data::message::UserMessage;
use awman::engine::git::push_publish::{
    NoGate, PushFailure, PushFailureKind, PushObserver, PushOutcome, PushPublisher, PushRetry,
};
use awman::engine::git::push_retry::PushRetryPolicy;
use awman::engine::git::transport::{CredentialSource, HostGitTransport};
use awman::engine::git::GitEngine;
use awman_test_support::capability::{LocalGit, Loopback};
use awman_test_support::fakes::{FakeGitHttp, GitHttpBuilder, GitHttpFault, GitHttpRequest};
use awman_test_support::fixtures::{Repo, RepoBuilder};
use awman_test_support::TestSandbox;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PASSWORD: &str = "test-secret-push-retry-password";

#[derive(Default)]
struct Observer {
    retries: Mutex<Vec<PushRetry>>,
    messages: Mutex<Vec<String>>,
    signal: tokio::sync::Notify,
}
impl PushObserver for Observer {
    fn retry(&self, retry: &PushRetry) {
        self.retries.lock().unwrap().push(retry.clone());
        self.signal.notify_one();
    }
    fn message(&self, message: UserMessage) {
        self.messages.lock().unwrap().push(message.text);
    }
}
impl Observer {
    fn retries(&self) -> Vec<PushRetry> {
        self.retries.lock().unwrap().clone()
    }
    fn operations(&self) -> Vec<GitTransportOp> {
        self.retries().iter().map(|r| r.operation).collect()
    }
}

/// Millisecond backoff, a generous window and per-call bound.
fn policy() -> PushRetryPolicy {
    PushRetryPolicy {
        max_attempts: 3,
        base_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(10),
        first_attempt_timeout: Duration::from_secs(20),
        retry_attempt_timeout: Duration::from_secs(20),
        ..PushRetryPolicy::for_command(None)
    }
}

struct World<'a> {
    s: &'a TestSandbox,
    repo: Repo<'a>,
    bare: PathBuf,
}

fn sandbox(id: &'static str) -> TestSandbox {
    let s = support::sandbox(id);
    s.require::<LocalGit>();
    s.require::<Loopback>();
    s.redact(PASSWORD);
    s
}

/// A local `main` one commit ahead of its bare remote `origin.git`.
fn world(s: &TestSandbox) -> World<'_> {
    let repo = RepoBuilder::new(s, "repo").file("base", "base").init();
    let bare = repo.bare_remote("origin");
    repo.write("change", "published by awman").commit("change");
    World { s, repo, bare }
}

impl<'a> World<'a> {
    fn server(
        &self,
        configure: impl FnOnce(GitHttpBuilder<'a>) -> GitHttpBuilder<'a>,
    ) -> FakeGitHttp {
        configure(FakeGitHttp::builder(self.s, self.bare.parent().unwrap())).start()
    }
    fn engine(&self) -> GitEngine {
        GitEngine::with_env(support::env_snapshot(self.s))
    }
    fn publisher_with(
        &self,
        engine: GitEngine,
        url: &str,
        policy: PushRetryPolicy,
    ) -> PushPublisher {
        let git = engine
            .isolate_remote_push_with_transport(
                self.repo.path(),
                url,
                HostGitTransport::user(CredentialSource::Ambient),
            )
            .unwrap();
        PushPublisher::new(Arc::new(git), "main", policy)
    }
    fn publisher(&self, url: &str, policy: PushRetryPolicy) -> PushPublisher {
        self.publisher_with(self.engine(), url, policy)
    }
    /// Where `main` points on the bare remote.
    fn remote_main(&self) -> String {
        self.repo
            .git(&["ls-remote", self.bare.to_str().unwrap(), "refs/heads/main"])
            .split('\t')
            .next()
            .unwrap()
            .trim()
            .to_string()
    }
}

async fn run(
    publisher: PushPublisher,
    cancel: &CancellationToken,
    observer: &Observer,
) -> Result<PushOutcome, PushFailure> {
    tokio::time::timeout(
        Duration::from_secs(60),
        publisher.run(cancel, &mut NoGate, observer),
    )
    .await
    .expect("the push loop is bounded")
}

/// Drop the first request of `service`; every other request is served.
fn drop_first(
    service: &'static str,
) -> impl Fn(&GitHttpRequest) -> Option<GitHttpFault> + Send + Sync + 'static {
    let seen = AtomicUsize::new(0);
    move |r| {
        (r.service() == Some(service) && seen.fetch_add(1, Ordering::SeqCst) == 0)
            .then_some(GitHttpFault::DropBeforeResponse)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_pre_response_failure_then_recovery_emits_one_retry_and_pushes() {
    let s = sandbox(
        "engine::git_push_retry::one_pre_response_failure_then_recovery_emits_one_retry_and_pushes",
    );
    let w = world(&s);
    let server = w.server(|b| b.fault(drop_first("git-receive-pack")));
    let url = server.url("origin.git");
    let observer = Observer::default();
    let outcome = run(
        w.publisher(&url, policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            sha: w.repo.head(),
            attempts: 2
        }
    );
    assert_eq!(
        observer.retries(),
        vec![PushRetry {
            attempt: 2,
            max_attempts: 3,
            operation: GitTransportOp::Push,
            delay: Duration::from_millis(5),
            remote: SafeRemote::from_url(&url),
        }]
    );
    assert_eq!(server.receive_packs(), 1);
    assert_eq!(w.remote_main(), w.repo.head());
    drop(server);
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_pre_response_failure_exhausts_into_unreachable_within_the_window() {
    let s = sandbox("engine::git_push_retry::permanent_pre_response_failure_exhausts_into_unreachable_within_the_window");
    let w = world(&s);
    let before = w.remote_main();
    let server = w.server(|b| b.fault(|_| Some(GitHttpFault::DropBeforeResponse)));
    let url = server.url("origin.git");
    // Attempts run out.
    let observer = Observer::default();
    let failure = run(
        w.publisher(&url, policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (PushFailureKind::Unreachable, 3)
    );
    assert_eq!(
        (failure.stage(), failure.code()),
        ("push", "push_unreachable")
    );
    assert!(failure
        .message
        .contains(&SafeRemote::from_url(&url).to_string()));
    assert!(!failure.message.to_ascii_lowercase().contains("empty reply"));
    assert_eq!(
        observer
            .retries()
            .iter()
            .map(|r| r.attempt)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    // The window, counted from the first failure, runs out before the
    // attempts do: 40 ms, 80 ms, 160 ms... never start after 100 ms.
    let windowed = PushRetryPolicy {
        max_attempts: 20,
        base_backoff: Duration::from_millis(40),
        max_backoff: Duration::from_secs(1),
        window: Duration::from_millis(100),
        ..policy()
    };
    let observer = Observer::default();
    let failure = run(
        w.publisher(&url, windowed),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(failure.kind, PushFailureKind::Unreachable);
    assert!((2..=3).contains(&failure.attempts), "{failure:?}");
    let waited: Duration = observer.retries().iter().map(|r| r.delay).sum();
    assert!(waited < Duration::from_millis(100), "{waited:?}");
    // A command deadline that has already passed makes no request.
    let expired =
        PushRetryPolicy::for_command(Some(chrono::Utc::now() - chrono::Duration::seconds(1)));
    let requests = server.requests().len();
    let failure = run(
        w.publisher(&url, expired),
        &CancellationToken::new(),
        &Observer::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (PushFailureKind::Unreachable, 0)
    );
    assert_eq!(server.requests().len(), requests);
    assert_eq!(server.receive_packs(), 0);
    assert_eq!(w.remote_main(), before);
    drop(server);
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_push_with_dropped_response_is_resolved_by_the_head_check_without_a_second_push() {
    let s = sandbox("engine::git_push_retry::accepted_push_with_dropped_response_is_resolved_by_the_head_check_without_a_second_push");
    let w = world(&s);
    for close_connections in [false, true] {
        if close_connections {
            w.repo.write("fresh-connection", "next").commit("next push");
        }
        let server = w.server(|builder| {
            let builder = if close_connections {
                builder.close_connections()
            } else {
                builder
            };
            builder.drop_after_receive_pack()
        });
        let url = server.url("origin.git");
        let observer = Observer::default();
        let outcome = run(
            w.publisher(&url, policy()),
            &CancellationToken::new(),
            &observer,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            PushOutcome::AlreadyPublished { sha: w.repo.head() }
        );
        assert_eq!(server.receive_packs(), 1, "the push is never repeated");
        assert_eq!(observer.operations(), vec![GitTransportOp::Push]);
        assert_eq!(w.remote_main(), w.repo.head());
        drop(server);
    }
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_hook_and_non_fast_forward_replies_are_never_retried() {
    let s =
        sandbox("engine::git_push_retry::auth_hook_and_non_fast_forward_replies_are_never_retried");
    let w = world(&s);
    let check = |failure: PushFailure, kind, code: &str, observer: &Observer| {
        assert_eq!((failure.kind, failure.attempts), (kind, 1), "{failure:?}");
        assert_eq!(failure.code(), code);
        assert!(observer.retries().is_empty());
        assert!(!failure.message.contains(PASSWORD));
    };
    // Authentication: the remote requires a password nobody supplies.
    let server = w.server(|b| b.basic_auth("user", PASSWORD));
    let observer = Observer::default();
    let failure = run(
        w.publisher(&server.url("origin.git"), policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(failure.stage(), "git_auth");
    check(failure, PushFailureKind::Auth, "git_auth_failed", &observer);
    drop(server);

    // A pre-receive hook declines the push (in-band, after a response).
    // The sandbox disables hooks globally; the bare remote opts back in.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hooks = w.bare.join("hooks");
        let hook = hooks.join("pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho 'policy says no' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        s.command("git")
            .args(["config", "core.hooksPath", hooks.to_str().unwrap()])
            .current_dir(&w.bare)
            .run()
            .assert_success();
        let server = w.server(|b| b);
        let observer = Observer::default();
        let failure = run(
            w.publisher(&server.url("origin.git"), policy()),
            &CancellationToken::new(),
            &observer,
        )
        .await
        .unwrap_err();
        check(failure, PushFailureKind::Rejected, "push_failed", &observer);
        assert_eq!(server.receive_packs(), 1);
        std::fs::remove_file(&hook).unwrap();
        drop(server);
    }

    // Non-fast-forward: the remote branch has a commit the local one lacks.
    w.repo.write("remote-only", "remote").commit("remote only");
    w.repo.push("origin", "main");
    w.repo.git(&["reset", "--hard", "HEAD~1"]);
    w.repo.write("diverged", "local").commit("diverged");
    let server = w.server(|b| b);
    let observer = Observer::default();
    let failure = run(
        w.publisher(&server.url("origin.git"), policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    check(failure, PushFailureKind::Rejected, "push_failed", &observer);
    drop(server);
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_advanced_to_another_commit_during_retry_is_a_rejection_not_success() {
    let s = sandbox("engine::git_push_retry::branch_advanced_to_another_commit_during_retry_is_a_rejection_not_success");
    let w = world(&s);
    // Another writer's commit, already in the remote's object store.
    w.repo.git(&["checkout", "-q", "-b", "other", "HEAD~1"]);
    let other = w.repo.write("other", "someone else").commit("other writer");
    w.repo.push("origin", "other");
    w.repo.checkout("main");
    let main_ref = w.bare.join("refs/heads/main");
    let advance = other.clone();
    let server = w.server(move |b| {
        let dropped = AtomicUsize::new(0);
        b.fault(move |r| {
            if r.service() == Some("git-receive-pack")
                && dropped.fetch_add(1, Ordering::SeqCst) == 0
            {
                // The branch moves while this push gets no response.
                std::fs::write(&main_ref, format!("{advance}\n")).unwrap();
                return Some(GitHttpFault::DropBeforeResponse);
            }
            None
        })
    });
    let observer = Observer::default();
    let failure = run(
        w.publisher(&server.url("origin.git"), policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (PushFailureKind::Rejected, 2),
        "{failure:?}"
    );
    assert_eq!(failure.code(), "push_failed");
    assert_eq!(observer.operations(), vec![GitTransportOp::Push]);
    assert_eq!(w.remote_main(), other);
    drop(server);
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_check_failing_with_the_same_unreachable_remote_stays_retryable() {
    let s = sandbox("engine::git_push_retry::head_check_failing_with_the_same_unreachable_remote_stays_retryable");
    let w = world(&s);
    // The push gets no response; then the next head check gets none either.
    let server = w.server(|b| {
        let phase = AtomicUsize::new(0);
        b.fault(move |r| {
            let fail = matches!(
                (phase.load(Ordering::SeqCst), r.service()),
                (0, Some("git-receive-pack")) | (1, Some("git-upload-pack"))
            );
            if fail {
                phase.fetch_add(1, Ordering::SeqCst);
            }
            fail.then_some(GitHttpFault::DropBeforeResponse)
        })
    });
    let observer = Observer::default();
    let outcome = run(
        w.publisher(&server.url("origin.git"), policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            sha: w.repo.head(),
            attempts: 3
        }
    );
    assert_eq!(
        observer.operations(),
        vec![GitTransportOp::Push, GitTransportOp::LsRemote]
    );
    assert_eq!(server.receive_packs(), 1);
    drop(server);
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_backoff_returns_cancelled() {
    let s = sandbox("engine::git_push_retry::cancellation_during_backoff_returns_cancelled");
    let w = world(&s);
    let server = w.server(|b| b.fault(|_| Some(GitHttpFault::DropBeforeResponse)));
    let long = PushRetryPolicy {
        base_backoff: Duration::from_secs(60),
        max_backoff: Duration::from_secs(60),
        window: Duration::from_secs(600),
        ..policy()
    };
    let observer = Observer::default();
    let cancel = CancellationToken::new();
    let publisher = w.publisher(&server.url("origin.git"), long);
    let cancelling = async {
        observer.signal.notified().await;
        cancel.cancel();
    };
    let (failure, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut gate = NoGate;
        tokio::join!(publisher.run(&cancel, &mut gate, &observer), cancelling)
    })
    .await
    .expect("cancellation interrupts the 60 s backoff");
    let failure = failure.unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (PushFailureKind::Cancelled, 1)
    );
    assert_eq!((failure.stage(), failure.code()), ("push", "cancelled"));
    assert_eq!(observer.retries().len(), 1);
    assert_eq!(server.receive_packs(), 0);
    drop(server);
    drop(w);
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_before_any_response_is_transport_and_after_response_is_not() {
    let s = sandbox("engine::git_push_retry::timeout_before_any_response_is_transport_and_after_response_is_not");
    let w = world(&s);
    let short = PushRetryPolicy {
        first_attempt_timeout: Duration::from_secs(1),
        ..policy()
    };
    // No byte arrives before awman's bound: retried, then published.
    let server = w.server(|b| b.fault(first_request(GitHttpFault::StallBeforeResponse)));
    let observer = Observer::default();
    let outcome = run(
        w.publisher(&server.url("origin.git"), short.clone()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            sha: w.repo.head(),
            attempts: 2
        }
    );
    assert_eq!(observer.operations(), vec![GitTransportOp::LsRemote]);
    drop(server);
    // Reset the remote so there is something to push again.
    let base = w.repo.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    std::fs::write(w.bare.join("refs/heads/main"), format!("{base}\n")).unwrap();
    // The remote started answering, then stalled: terminal on attempt 1.
    let server = w.server(|b| b.fault(first_request(GitHttpFault::StallAfterHeaders)));
    let observer = Observer::default();
    let failure = run(
        w.publisher(&server.url("origin.git"), short),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (PushFailureKind::Other, 1),
        "{failure:?}"
    );
    assert_eq!(failure.code(), "push_failed");
    assert!(observer.retries().is_empty());
    assert_eq!(w.remote_main(), base);
    drop(server);
    drop(w);
    s.finish();
}

/// A global credential helper reading a private file: the password is never
/// in argv, and the server requires it.
fn engine_with_password(w: &World) -> GitEngine {
    let credential = w.s.root().join("push.credential");
    std::fs::write(&credential, format!("username=user\npassword={PASSWORD}\n")).unwrap();
    let config = w.s.root().join("push.gitconfig");
    w.s.command("git")
        .args(["config", "--file"])
        .arg(&config)
        .args([
            "credential.helper",
            &format!("!cat '{}'", credential.display()),
        ])
        .run()
        .assert_success();
    let mut values: std::collections::BTreeMap<String, String> =
        w.s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
    values.insert("GIT_CONFIG_GLOBAL".into(), config.display().to_string());
    GitEngine::with_env(EnvSnapshot::from_full_env(values))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_event_carries_attempt_operation_delay_ms_and_safe_remote_only() {
    let s = sandbox("engine::git_push_retry::retry_event_carries_attempt_operation_delay_ms_and_safe_remote_only");
    let w = world(&s);
    let server = w.server(|b| {
        b.basic_auth("user", PASSWORD)
            .fault(drop_first("git-receive-pack"))
    });
    let url = server.url("origin.git");
    let observer = Observer::default();
    let outcome = run(
        w.publisher_with(engine_with_password(&w), &url, policy()),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PushOutcome::Pushed { attempts: 2, .. }));
    let retries = observer.retries();
    assert_eq!(retries.len(), 1);
    let event = retries[0].event();
    assert_eq!(event.sse_event_type(), "push_retry");
    let data = serde_json::to_value(&event).unwrap()["data"].clone();
    let mut keys: Vec<_> = data.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["attempt", "delay_ms", "max_attempts", "operation", "remote"]
    );
    assert_eq!(data["attempt"], 2);
    assert_eq!(data["operation"], "push");
    assert_eq!(data["delay_ms"], 5);
    assert_eq!(data["remote"], SafeRemote::from_url(&url).to_string());
    let everything = format!(
        "{} {} {:?}",
        data,
        event.to_plain_text().unwrap(),
        observer.messages.lock().unwrap()
    );
    for leaked in [PASSWORD, "Empty reply", "curl", "user:"] {
        assert!(!everything.contains(leaked), "{leaked} in {everything}");
    }
    drop(server);
    drop(w);
    s.finish();
}

/// `fault` on the very first request only.
fn first_request(
    fault: GitHttpFault,
) -> impl Fn(&GitHttpRequest) -> Option<GitHttpFault> + Send + Sync + 'static {
    let seen = AtomicUsize::new(0);
    move |_| (seen.fetch_add(1, Ordering::SeqCst) == 0).then_some(fault)
}

#[tokio::test(start_paused = true)]
async fn delayed_observer_cannot_start_a_retry_after_the_window() {
    let s =
        sandbox("engine::git_push_retry::delayed_observer_cannot_start_a_retry_after_the_window");
    let w = world(&s);
    let http = w.server(|b| b.fail_before_response(1));
    struct Slow(AtomicUsize);
    impl PushObserver for Slow {
        fn retry(&self, _: &PushRetry) {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async {
                tokio::time::advance(Duration::from_millis(250)).await;
            });
        }
    }
    let observer = Slow(AtomicUsize::new(0));
    let mut p = policy();
    p.window = Duration::from_millis(100);
    let error = w
        .publisher(&http.url("origin.git"), p)
        .run(&CancellationToken::new(), &mut NoGate, &observer)
        .await
        .unwrap_err();
    assert_eq!(error.kind, PushFailureKind::Unreachable);
    assert_eq!(error.attempts, 1);
    assert_eq!(observer.0.load(Ordering::SeqCst), 1);
    assert_eq!(http.requests().len(), 1);
    assert_eq!(http.receive_packs(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_head_check_and_push_share_one_process_budget() {
    let s = sandbox("engine::git_push_retry::retry_head_check_and_push_share_one_process_budget");
    let w = world(&s);
    let upload = AtomicUsize::new(0);
    let http = w.server(|b| {
        b.fault(move |r| {
            if r.service() == Some("git-upload-pack") && upload.fetch_add(1, Ordering::SeqCst) == 1
            {
                let (_held, release) = std::sync::mpsc::channel::<()>();
                assert_eq!(
                    release.recv_timeout(Duration::from_millis(500)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                );
            }
            if r.service() == Some("git-receive-pack") {
                return Some(GitHttpFault::StallBeforeResponse);
            }
            None
        })
    });
    struct Timed(Mutex<Option<std::time::Instant>>);
    impl PushObserver for Timed {
        fn retry(&self, _: &PushRetry) {
            *self.0.lock().unwrap() = Some(std::time::Instant::now());
        }
    }
    let observer = Timed(Mutex::new(None));
    let mut p = policy();
    p.max_attempts = 2;
    p.first_attempt_timeout = Duration::from_secs(2);
    p.retry_attempt_timeout = Duration::from_millis(900);
    let error = w
        .publisher(&http.url("origin.git"), p)
        .run(&CancellationToken::new(), &mut NoGate, &observer)
        .await
        .unwrap_err();
    assert_eq!(error.kind, PushFailureKind::Unreachable);
    let elapsed = observer.0.lock().unwrap().unwrap().elapsed();
    assert!(
        elapsed < Duration::from_millis(1250),
        "shared 900ms budget took {elapsed:?}"
    );
    assert_eq!(error.attempts, 2);
    assert_eq!(http.receive_packs(), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_kills_local_head_and_config_probe_process_groups() {
    use std::os::unix::fs::PermissionsExt;
    let s = sandbox(
        "engine::git_push_retry::cancellation_kills_local_head_and_config_probe_process_groups",
    );
    let w = world(&s);
    let http = w.server(|b| b);
    let bin = s.root().join("wrappers");
    std::fs::create_dir(&bin).unwrap();
    let armed = s.root().join("armed");
    let started = s.root().join("started");
    let wrapper = bin.join("git");
    let real = s
        .command("sh")
        .args(["-c", "command -v git"])
        .run()
        .stdout_text();
    std::fs::write(&wrapper, format!("#!/bin/sh\nif [ -f '{}' ]; then\n case \" $* \" in\n *\"$(cat '{}')\"*) touch '{}'; sleep 30; exit 1;;\n esac\nfi\nexec '{}' \"$@\"\n", armed.display(), armed.display(), started.display(), real.trim())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut values: std::collections::BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    values.insert(
        "PATH".into(),
        format!("{}:{}", bin.display(), values.get("PATH").unwrap()),
    );
    for target in ["rev-parse --verify HEAD", " config "] {
        let publisher = w.publisher_with(
            GitEngine::with_env(EnvSnapshot::from_full_env(values.clone())),
            &http.url("origin.git"),
            policy(),
        );
        std::fs::write(&armed, target).unwrap();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let marker = started.clone();
        let interrupter = tokio::spawn(async move {
            awman_test_support::wait_for_async(
                "local Git started",
                Duration::from_secs(5),
                || async { marker.exists().then_some(()) },
            )
            .await;
            stop.cancel();
        });
        let began = std::time::Instant::now();
        let error = run(publisher, &cancel, &Observer::default())
            .await
            .unwrap_err();
        interrupter.await.unwrap();
        assert_eq!(error.kind, PushFailureKind::Cancelled);
        assert!(began.elapsed() < Duration::from_secs(2));
        std::fs::remove_file(&armed).unwrap();
        std::fs::remove_file(&started).unwrap();
    }
    assert_eq!(http.receive_packs(), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_advertisement_then_stall_is_terminal_without_a_retry() {
    use std::os::unix::fs::PermissionsExt;
    let s =
        sandbox("engine::git_push_retry::ssh_advertisement_then_stall_is_terminal_without_a_retry");
    let w = world(&s);
    let ssh = s.tmp().join("ssh-response");
    let base = w.repo.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let packet = format!("{base} refs/heads/main\n");
    let advertisement = format!("{:04x}{packet}0000", packet.len() + 4);
    std::fs::write(
        &ssh,
        format!("#!/bin/sh\nprintf '%s' '{advertisement}'\nsleep 30\n"),
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut env: std::collections::BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    env.insert(
        "GIT_SSH_COMMAND".into(),
        format!("{} -o BatchMode=yes", ssh.display()),
    );
    env.insert("GIT_SSH_VARIANT".into(), "ssh".into());
    let observer = Observer::default();
    let mut p = policy();
    p.first_attempt_timeout = Duration::from_secs(1);
    let error = run(
        w.publisher_with(
            GitEngine::with_env(EnvSnapshot::from_full_env(env.clone())),
            "ssh://git@example.invalid/o/r.git",
            p,
        ),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, PushFailureKind::Other);
    assert_eq!(error.code(), "push_failed");
    assert_eq!(error.attempts, 1);
    assert!(observer.retries().is_empty());
    // The head check answers normally; receive-pack advertises then stalls
    // after the client starts its upload. Incoming packet evidence must
    // also prevent retrying this uncertain SSH upload.
    std::fs::write(&ssh, format!("#!/bin/sh\nprintf '%s' '{advertisement}'\ncase \" $* \" in *git-receive-pack*) sleep 30;; *) dd bs=1 count=4 of=/dev/null 2>/dev/null;; esac\n")).unwrap();
    let observer = Observer::default();
    let mut p = policy();
    p.first_attempt_timeout = Duration::from_secs(1);
    let error = run(
        w.publisher_with(
            GitEngine::with_env(EnvSnapshot::from_full_env(env)),
            "ssh://git@example.invalid/o/r.git",
            p,
        ),
        &CancellationToken::new(),
        &observer,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, PushFailureKind::Other);
    assert!(
        error.message.contains("git push") && error.message.contains("timed out"),
        "{}",
        error.message
    );
    assert_eq!(error.attempts, 1);
    assert!(observer.retries().is_empty());
}
