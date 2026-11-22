use crate::support;
use awman::engine::git::{
    lease_transport::{LeaseTransport, LeaseUrlPolicy},
    push_retry::PushRetryPolicy,
    GitEngine,
};
use awman::engine::worker::push::{
    LeasePush, LeasePushFailureKind, LeasePushOutcome, PushObserver, PushRetry,
};
use awman_test_support::{fixtures::RepoBuilder, TestSandbox};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Observer {
    retries: Mutex<Vec<PushRetry>>,
    signal: tokio::sync::Notify,
}
impl PushObserver for Observer {
    fn retry(&self, event: &PushRetry) {
        self.retries.lock().unwrap().push(event.clone());
        self.signal.notify_one();
    }
}
fn policy() -> PushRetryPolicy {
    PushRetryPolicy {
        max_attempts: 3,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        deadline: chrono::Utc::now() + chrono::Duration::seconds(60),
        ..PushRetryPolicy::for_command(None)
    }
}
fn pusher(
    sandbox: &TestSandbox,
    repo: &std::path::Path,
    url: &str,
    policy: PushRetryPolicy,
) -> LeasePush {
    let git = GitEngine::with_env(support::env_snapshot(sandbox))
        .isolate_remote_push(repo, url)
        .unwrap()
        .with_lease_transport(LeaseTransport::anonymous(LeaseUrlPolicy {
            allow_local: true,
        }));
    LeasePush::new(Arc::new(git), "wsp/item", policy, Duration::from_millis(1))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_http_auth_and_server_refusals_never_enter_transport_retry() {
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let sandbox = support::sandbox(
        "worker::push::remote_http_auth_and_server_refusals_never_enter_transport_retry",
    );
    let repo = RepoBuilder::new(&sandbox, "refused-http")
        .file("base", "base")
        .init();
    for (status, kind) in [
        (401, LeasePushFailureKind::Auth),
        (403, LeasePushFailureKind::Auth),
        (503, LeasePushFailureKind::Rejected),
    ] {
        let server = MockServer::builder()
            .listener(sandbox.ports().tcp_listener())
            .start()
            .await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        let observer = Observer::default();
        let failure = pusher(&sandbox, repo.path(), &server.uri(), policy())
            .run(&CancellationToken::new(), &observer)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, kind, "HTTP {status}: {failure:?}");
        assert_eq!(failure.attempts, 1);
        assert!(observer.retries.lock().unwrap().is_empty());
        drop(server);
    }
    sandbox.finish();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_is_idempotent_and_rejection_never_retries() {
    let sandbox =
        support::sandbox("worker::push::publish_is_idempotent_and_rejection_never_retries");
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("initial", "base")
        .init();
    let bare = repo.bare_remote("origin");
    repo.write("change", "worker").commit("worker");
    let observer = Observer::default();
    let url = bare.to_str().unwrap();
    let push = pusher(&sandbox, repo.path(), url, policy());
    let first = push
        .run(&CancellationToken::new(), &observer)
        .await
        .unwrap();
    assert_eq!(
        first,
        LeasePushOutcome::Pushed {
            sha: repo.head(),
            attempts: 1
        }
    );
    assert_eq!(
        pusher(&sandbox, repo.path(), url, policy())
            .run(&CancellationToken::new(), &observer)
            .await
            .unwrap(),
        LeasePushOutcome::AlreadyPublished { sha: repo.head() }
    );
    // A divergent local branch cannot overwrite the worker branch.
    repo.checkout("main");
    repo.git(&["reset", "--hard", "HEAD~1"]);
    repo.write("other", "divergent").commit("divergent");
    let failed = pusher(&sandbox, repo.path(), url, policy())
        .run(&CancellationToken::new(), &observer)
        .await
        .unwrap_err();
    assert_eq!(
        (failed.kind, failed.attempts),
        (LeasePushFailureKind::Rejected, 1)
    );
    assert!(observer.retries.lock().unwrap().is_empty());
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_http_push_failure_retries_same_commit_then_becomes_idempotent() {
    use axum::{
        body::{to_bytes, Body},
        extract::Request,
        response::Response,
        routing::any,
        Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sandbox = support::sandbox(
        "worker::push::transient_http_push_failure_retries_same_commit_then_becomes_idempotent",
    );
    let repo = RepoBuilder::new(&sandbox, "http-push")
        .file("base", "base")
        .init();
    let bare = repo.bare_remote("origin");
    repo.write("changed", "one commit").commit("publish once");
    let upstream = awman_test_support::fakes::FakeGitHttp::start(&sandbox, bare.parent().unwrap());
    let target = upstream.url(bare.file_name().unwrap().to_str().unwrap());
    let uploads = Arc::new(AtomicUsize::new(0));
    let count = uploads.clone();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let router = Router::new().fallback(any(move |request: Request| {
        let count = count.clone();
        let target = target.clone();
        let http = http.clone();
        async move {
            let (parts, body) = request.into_parts();
            if parts.method == axum::http::Method::POST
                && parts.uri.path().ends_with("git-receive-pack")
            {
                count.fetch_add(1, Ordering::SeqCst);
            }
            let url = format!("{target}{}", parts.uri.path_and_query().unwrap());
            let response = http
                .request(parts.method, url)
                .headers(parts.headers)
                .body(to_bytes(body, 32 << 20).await.unwrap())
                .send()
                .await
                .unwrap();
            let result = Response::builder().status(response.status()).header(
                "content-type",
                response
                    .headers()
                    .get("content-type")
                    .expect("Git CGI content type"),
            );
            result
                .body(Body::from(response.bytes().await.unwrap()))
                .unwrap()
        }
    }));
    let listener = sandbox.ports().tcp_listener();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(async move {
        // Fail before any remote response, which is the retryable contract.
        // Reading the request establishes that the client reached this server.
        use tokio::io::AsyncReadExt;
        let (mut first, _) = listener.accept().await.unwrap();
        let mut bytes = [0u8; 4096];
        assert!(first.read(&mut bytes).await.unwrap() > 0);
        drop(first);
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
    });
    let observer = Observer::default();
    let url = format!("http://{addr}");
    let result = pusher(&sandbox, repo.path(), &url, policy())
        .run(&CancellationToken::new(), &observer)
        .await
        .unwrap();
    assert_eq!(
        result,
        LeasePushOutcome::Pushed {
            sha: repo.head(),
            attempts: 2
        }
    );
    assert_eq!(uploads.load(Ordering::SeqCst), 1);
    assert_eq!(observer.retries.lock().unwrap().len(), 1);
    assert_eq!(
        pusher(&sandbox, repo.path(), &url, policy())
            .run(&CancellationToken::new(), &observer)
            .await
            .unwrap(),
        LeasePushOutcome::AlreadyPublished { sha: repo.head() }
    );
    assert_eq!(uploads.load(Ordering::SeqCst), 1);
    stop.cancel();
    server.await.unwrap();
    drop(upstream);
    sandbox.finish();
}

/// Guards: 3e8d7e6c. Transport retries are bounded and cancellation interrupts
/// a long backoff. Keep ownership of the rejecting socket for the entire run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_3e8d7e6c_transport_retry_and_cancellation_are_bounded() {
    let sandbox = support::sandbox(
        "worker::push::regression_3e8d7e6c_transport_retry_and_cancellation_are_bounded",
    );
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("initial", "base")
        .init();
    let listener = sandbox.ports().tcp_listener();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(async move {
        loop {
            tokio::select! { _=stopped.cancelled()=>break, accepted=listener.accept()=> { drop(accepted.unwrap()); } }
        }
    });
    let observer = Observer::default();
    let failure = pusher(&sandbox, repo.path(), &url, policy())
        .run(&CancellationToken::new(), &observer)
        .await
        .unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (LeasePushFailureKind::Unreachable, 3),
        "{failure:?}"
    );
    assert_eq!(
        observer
            .retries
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.attempt)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let observer = Observer::default();
    let mut long = policy();
    long.deadline = chrono::Utc::now() + chrono::Duration::minutes(10);
    long.base_backoff = Duration::from_secs(60);
    long.max_backoff = Duration::from_secs(60);
    long.window = Duration::from_secs(600);
    let cancel = CancellationToken::new();
    let push = pusher(&sandbox, repo.path(), &url, long);
    let run = push.run(&cancel, &observer);
    let cancelling = async {
        observer.signal.notified().await;
        cancel.cancel();
    };
    let (failure, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run, cancelling)
    })
    .await
    .unwrap();
    assert_eq!(failure.unwrap_err().kind, LeasePushFailureKind::Cancelled);
    assert_eq!(observer.retries.lock().unwrap().len(), 1);
    stop.cancel();
    server.await.unwrap();
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_and_revoked_credentials_make_no_transport_request() {
    use awman::engine::git::lease_transport::{
        HelperProgram, LeaseCredentialHelper, LeaseCredentials,
    };
    use awman::engine::worker::{error::WorkerEngineError, push::CredentialSource};
    struct Revoked;
    #[async_trait::async_trait]
    impl CredentialSource for Revoked {
        async fn fresh(&self) -> Result<LeaseCredentials, WorkerEngineError> {
            Err(WorkerEngineError::LinkRevoked)
        }
    }
    let sandbox =
        support::sandbox("worker::push::expiry_and_revoked_credentials_make_no_transport_request");
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("initial", "base")
        .init();
    let listener = sandbox.ports().tcp_listener();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
    let observer = Observer::default();
    let mut expired = policy();
    expired.deadline = chrono::Utc::now() - chrono::Duration::seconds(1);
    let failure = pusher(&sandbox, repo.path(), &url, expired)
        .run(&CancellationToken::new(), &observer)
        .await
        .unwrap_err();
    assert_eq!(
        (failure.kind, failure.attempts),
        (LeasePushFailureKind::Unreachable, 0)
    );
    let creds = LeaseCredentials::from_wsp(Some(&awman_wsp::Credentials {
        kind: awman_wsp::CredentialKind::Bearer,
        token: Some("expired-test-token".into()),
        username: None,
        expires_at: None,
    }))
    .unwrap();
    let helper = Arc::new(
        LeaseCredentialHelper::install(
            &sandbox.tmp().join("credential"),
            &url,
            &creds,
            HelperProgram::awman(support::awman_exe().to_path_buf()),
            LeaseUrlPolicy { allow_local: true },
        )
        .unwrap(),
    );
    let failure = pusher(&sandbox, repo.path(), &url, policy())
        .with_credentials(helper, Arc::new(Revoked), Some(chrono::Utc::now()))
        .run(&CancellationToken::new(), &observer)
        .await
        .unwrap_err();
    assert_eq!(failure.kind, LeasePushFailureKind::LinkRevoked);
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    drop(listener);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fencing_during_remote_request_kills_git_and_never_reports_success() {
    let sandbox = support::sandbox(
        "worker::push::fencing_during_remote_request_kills_git_and_never_reports_success",
    );
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("initial", "base")
        .init();
    let listener = sandbox.ports().tcp_listener();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let stopped = CancellationToken::new();
    let stop = stopped.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        signal.cancel();
        stop.cancelled().await;
        drop(stream);
    });
    let observer = Observer::default();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        pusher(&sandbox, repo.path(), &url, policy()).run(&cancel, &observer),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(result.kind, LeasePushFailureKind::Cancelled);
    assert!(observer.retries.lock().unwrap().is_empty());
    stopped.cancel();
    server.await.unwrap();
    sandbox.finish();
}

/// Guards: 3e8d7e6c. Cancellation during upload kills the Git process group;
/// completing a blocked command later cannot publish a stale success.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fencing_during_upload_kills_git_before_it_can_publish() {
    let sandbox =
        support::sandbox("worker::push::fencing_during_upload_kills_git_before_it_can_publish");
    let git = sandbox.require::<awman_test_support::capability::LocalGit>();
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("initial", "base")
        .init();
    let bare = repo.bare_remote("origin");
    repo.write("change", "worker").commit("worker");
    let push = pusher(&sandbox, repo.path(), bare.to_str().unwrap(), policy());
    let fifo = sandbox.tmp().join("upload-release");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let started = sandbox.tmp().join("upload-started");
    let stale = sandbox.tmp().join("stale-success");
    let source = sandbox.tmp().join("git-shim");
    let quote = |p: &std::path::Path| shell_words::quote(p.to_str().unwrap()).to_string();
    std::fs::write(&source,format!("#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = push ]; then\n  : > {}\n  read -r ignored < {}\n  : > {}\n fi\ndone\nexec {} \"$@\"\n",quote(&started),quote(&fifo),quote(&stale),quote(&git.exe))).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    sandbox.install_exe(&source, "git");
    let cancel = CancellationToken::new();
    let cancelled = cancel.clone();
    let signal = async {
        awman_test_support::wait_for_async("upload entered", Duration::from_secs(5), || async {
            started.exists().then_some(())
        })
        .await;
        cancelled.cancel();
    };
    let observer = Observer::default();
    let (failure, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(push.run(&cancel, &observer), signal)
    })
    .await
    .unwrap();
    assert_eq!(failure.unwrap_err().kind, LeasePushFailureKind::Cancelled);
    assert!(!stale.exists());
    assert!(observer.retries.lock().unwrap().is_empty());
    sandbox.finish();
}

// WI 0145: the lease runs the shared push loop (`PushPublisher`) through its
// `LeaseGate`. Smart-HTTP faults are injected by `FakeGitHttp`.

struct LeaseWorld<'a> {
    repo: awman_test_support::fixtures::Repo<'a>,
    bare: std::path::PathBuf,
}

fn lease_world(sandbox: &TestSandbox) -> LeaseWorld<'_> {
    sandbox.require::<awman_test_support::capability::LocalGit>();
    sandbox.require::<awman_test_support::capability::Loopback>();
    let repo = RepoBuilder::new(sandbox, "repo")
        .file("initial", "base")
        .init();
    let bare = repo.bare_remote("origin");
    repo.write("change", "worker").commit("worker");
    LeaseWorld { repo, bare }
}

impl LeaseWorld<'_> {
    fn remote_main(&self) -> String {
        self.repo
            .git(&[
                "ls-remote",
                self.bare.to_str().unwrap(),
                "refs/heads/wsp/item",
            ])
            .split('\t')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

fn lease_pusher(
    sandbox: &TestSandbox,
    repo: &std::path::Path,
    url: &str,
    policy: PushRetryPolicy,
    heartbeat: Duration,
    expiry: tokio::sync::watch::Receiver<chrono::DateTime<chrono::Utc>>,
) -> LeasePush {
    let git = GitEngine::with_env(support::env_snapshot(sandbox))
        .isolate_remote_push(repo, url)
        .unwrap()
        .with_lease_transport(LeaseTransport::anonymous(LeaseUrlPolicy {
            allow_local: true,
        }));
    LeasePush::new(Arc::new(git), "wsp/item", policy, heartbeat).with_expiry(expiry)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_lease_expiry_stops_retries_before_fencing_and_never_claims_a_push() {
    use awman_test_support::fakes::{FakeGitHttp, GitHttpFault};
    let sandbox = support::sandbox(
        "worker::push::short_lease_expiry_stops_retries_before_fencing_and_never_claims_a_push",
    );
    let world = lease_world(&sandbox);
    let root = world.bare.parent().unwrap().to_path_buf();

    // An unreachable remote: retries stop on their own before the lease
    // expires (expiry − 2 × heartbeat, then half of what is left).
    let server = FakeGitHttp::builder(&sandbox, &root)
        .fault(|_| Some(GitHttpFault::DropBeforeResponse))
        .start();
    let heartbeat = Duration::from_secs(1);
    let expires = chrono::Utc::now() + chrono::Duration::seconds(4);
    let (_renewals, expiry) = tokio::sync::watch::channel(expires);
    let policy = PushRetryPolicy {
        max_attempts: 50,
        base_backoff: Duration::from_millis(400),
        max_backoff: Duration::from_millis(400),
        window: Duration::from_secs(60),
        ..PushRetryPolicy::for_lease(expires, heartbeat)
    };
    let observer = Observer::default();
    let failure = tokio::time::timeout(
        Duration::from_secs(10),
        lease_pusher(
            &sandbox,
            world.repo.path(),
            &server.url("origin.git"),
            policy,
            heartbeat,
            expiry,
        )
        .run(&CancellationToken::new(), &observer),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(chrono::Utc::now() < expires, "retries outlived the lease");
    assert_eq!(
        failure.kind,
        LeasePushFailureKind::Unreachable,
        "{failure:?}"
    );
    assert_eq!(failure.code(), "push_unreachable");
    assert!(
        failure.attempts >= 2 && failure.attempts < 50,
        "{failure:?}"
    );
    assert_eq!(
        observer.retries.lock().unwrap().len(),
        usize::from(failure.attempts - 1)
    );
    assert_eq!(server.receive_packs(), 0);
    drop(server);

    // The push lands but its response is lost; the heartbeat cannot renew
    // and the lease is gone before the retry. The landed push is never
    // claimed and nothing is pushed again.
    let server = FakeGitHttp::builder(&sandbox, &root)
        .drop_after_receive_pack()
        .start();
    let expires = chrono::Utc::now() + chrono::Duration::seconds(60);
    let (renewals, expiry) = tokio::sync::watch::channel(expires);
    let policy = PushRetryPolicy {
        base_backoff: Duration::from_millis(500),
        max_backoff: Duration::from_millis(500),
        ..PushRetryPolicy::for_lease(expires, heartbeat)
    };
    let observer = Observer::default();
    let push = lease_pusher(
        &sandbox,
        world.repo.path(),
        &server.url("origin.git"),
        policy,
        heartbeat,
        expiry,
    );
    let cancel = CancellationToken::new();
    let lose_lease = async {
        observer.signal.notified().await;
        renewals
            .send(chrono::Utc::now() - chrono::Duration::seconds(1))
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(push.run(&cancel, &observer), lose_lease)
    })
    .await
    .unwrap();
    let failure = result.expect_err("a lost epoch never claims the push");
    assert!(
        matches!(
            failure.kind,
            LeasePushFailureKind::Unreachable | LeasePushFailureKind::Cancelled
        ),
        "{failure:?}"
    );
    assert_eq!(failure.attempts, 1);
    assert_eq!(server.receive_packs(), 1, "never pushed again");
    assert_eq!(world.remote_main(), world.repo.head());
    drop(server);
    drop(world);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_retry_emits_the_typed_push_retry_event() {
    use awman::data::git_transport::{GitTransportOp, SafeRemote};
    use awman_test_support::fakes::FakeGitHttp;
    let sandbox = support::sandbox("worker::push::lease_retry_emits_the_typed_push_retry_event");
    let world = lease_world(&sandbox);
    let server = FakeGitHttp::builder(&sandbox, world.bare.parent().unwrap())
        .fail_before_response(1)
        .start();
    let url = server.url("origin.git");
    let expires = chrono::Utc::now() + chrono::Duration::seconds(60);
    let (_renewals, expiry) = tokio::sync::watch::channel(expires);
    let observer = Observer::default();
    let outcome = lease_pusher(
        &sandbox,
        world.repo.path(),
        &url,
        policy(),
        Duration::from_secs(1),
        expiry,
    )
    .run(&CancellationToken::new(), &observer)
    .await
    .unwrap();
    assert_eq!(
        outcome,
        LeasePushOutcome::Pushed {
            sha: world.repo.head(),
            attempts: 2
        }
    );
    let retries = observer.retries.lock().unwrap().clone();
    assert_eq!(
        retries,
        vec![PushRetry {
            attempt: 2,
            max_attempts: 3,
            operation: GitTransportOp::LsRemote,
            delay: Duration::from_millis(1),
            remote: SafeRemote::from_url(&url),
        }]
    );
    let event = retries[0].event();
    assert_eq!(event.sse_event_type(), "push_retry");
    assert_eq!(
        event.to_plain_text().unwrap(),
        format!(
            "push retry 2/3 in 1 ms (ls-remote to {})",
            SafeRemote::from_url(&url)
        )
    );
    assert_eq!(server.receive_packs(), 1);
    drop(server);
    drop(world);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_cancel_during_backoff_is_cancelled() {
    use awman_test_support::fakes::{FakeGitHttp, GitHttpFault};
    let sandbox = support::sandbox("worker::push::lease_cancel_during_backoff_is_cancelled");
    let world = lease_world(&sandbox);
    let server = FakeGitHttp::builder(&sandbox, world.bare.parent().unwrap())
        .fault(|_| Some(GitHttpFault::DropBeforeResponse))
        .start();
    let expires = chrono::Utc::now() + chrono::Duration::minutes(10);
    let (_renewals, expiry) = tokio::sync::watch::channel(expires);
    let mut long = policy();
    long.base_backoff = Duration::from_secs(60);
    long.max_backoff = Duration::from_secs(60);
    long.window = Duration::from_secs(600);
    long.deadline = expires;
    let push = lease_pusher(
        &sandbox,
        world.repo.path(),
        &server.url("origin.git"),
        long,
        Duration::from_secs(1),
        expiry,
    );
    let observer = Observer::default();
    let cancel = CancellationToken::new();
    let cancelling = async {
        observer.signal.notified().await;
        cancel.cancel();
    };
    let (failure, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(push.run(&cancel, &observer), cancelling)
    })
    .await
    .unwrap();
    let failure = failure.unwrap_err();
    assert_eq!(failure.kind, LeasePushFailureKind::Cancelled);
    assert_eq!((failure.stage(), failure.code()), ("push", "cancelled"));
    assert_eq!(observer.retries.lock().unwrap().len(), 1);
    assert_eq!(server.receive_packs(), 0);
    drop(server);
    drop(world);
    sandbox.finish();
}
