//! Reliable push of a lease's branch.
//!
//! [`LeasePush::run`] retries only failures before a remote response
//! ([`GitFailureClass::Transport`]) with the bounded backoff of
//! [`PushRetryPolicy`], whose window follows the lease expiry from the latest
//! heartbeat. Each retry is reported to the [`PushObserver`] (the caller
//! records it in the event stream). Before every attempt the remote branch
//! is checked: an already-published `head` is success. Authentication
//! failures (`failure.stage: "git_auth"`, `failure.code: "git_auth_failed"`)
//! and remote refusals are never retried; exhausting the window is the
//! distinct `push_unreachable` failure. Backoff is cancellable.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use crate::data::fs::api_db::{failure_codes, failure_stages};
use crate::data::message::{RecordingMessageSink, UserMessage};
use crate::engine::error::EngineError;
use crate::engine::git::auth_classify::GitFailureClass;
use crate::engine::git::lease_transport::{LeaseCredentialHelper, LeaseCredentials};
use crate::engine::git::push_retry::{backoff, PushGiveUp, PushRetryDecision, PushRetryPolicy};
use crate::engine::git::remote_push::{LeaseGitGuard, RemotePushGit};

/// Fresh repository credentials for a long run (`POST /v1/leases/{id}/credentials`).
#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn fresh(&self) -> Result<LeaseCredentials, WorkerEngineError>;
}

/// One scheduled retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRetry {
    /// The attempt about to be made (2-based: the first retry is attempt 2).
    pub attempt: u8,
    pub max_attempts: u8,
    pub after: Duration,
    pub reason: String,
}

impl PushRetry {
    /// `(phase, message)` of the `status_message` event recording it.
    pub fn status_message(&self) -> (String, String) {
        (
            "push".into(),
            format!(
                "push retry {}/{} in {} ms: {}",
                self.attempt,
                self.max_attempts,
                self.after.as_millis(),
                self.reason
            ),
        )
    }
}

/// Where push progress goes.
pub trait PushObserver: Send + Sync {
    fn retry(&self, retry: &PushRetry);
    fn message(&self, _message: UserMessage) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeasePushOutcome {
    Pushed {
        sha: String,
        attempts: u8,
    },
    /// The remote branch already pointed at `head` (an earlier attempt
    /// landed although its response was lost).
    AlreadyPublished {
        sha: String,
    },
}

impl LeasePushOutcome {
    pub fn sha(&self) -> &str {
        match self {
            Self::Pushed { sha, .. } | Self::AlreadyPublished { sha } => sha,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeasePushFailureKind {
    Auth,
    Rejected,
    Unreachable,
    Cancelled,
    /// Fresh credentials were refused because the lease was lost.
    LeaseLost,
    LinkRevoked,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasePushFailure {
    pub kind: LeasePushFailureKind,
    pub message: String,
    pub attempts: u8,
}

impl LeasePushFailure {
    /// `CommandFailure.stage`.
    pub fn stage(&self) -> &'static str {
        match self.kind {
            LeasePushFailureKind::Auth => failure_stages::GIT_AUTH,
            _ => failure_stages::PUSH,
        }
    }

    /// `CommandFailure.code`.
    pub fn code(&self) -> &'static str {
        match self.kind {
            LeasePushFailureKind::Auth => failure_codes::GIT_AUTH_FAILED,
            LeasePushFailureKind::Unreachable => failure_codes::PUSH_UNREACHABLE,
            LeasePushFailureKind::Cancelled => failure_codes::CANCELLED,
            _ => failure_codes::PUSH_FAILED,
        }
    }
}

impl From<PushGiveUp> for LeasePushFailureKind {
    fn from(why: PushGiveUp) -> Self {
        match why {
            PushGiveUp::Auth => Self::Auth,
            PushGiveUp::Rejected => Self::Rejected,
            PushGiveUp::Unreachable => Self::Unreachable,
            PushGiveUp::Cancelled => Self::Cancelled,
            PushGiveUp::Other => Self::Other,
        }
    }
}

pub struct LeasePush {
    git: Arc<RemotePushGit>,
    branch: String,
    policy: PushRetryPolicy,
    heartbeat: Duration,
    expiry: Option<watch::Receiver<DateTime<Utc>>>,
    credentials: Option<CredentialRefresh>,
}

/// The helper to rotate, where fresh credentials come from, and when the
/// current ones expire.
type CredentialRefresh = (
    Arc<LeaseCredentialHelper>,
    Arc<dyn CredentialSource>,
    Option<DateTime<Utc>>,
);

/// Refresh credentials expiring within this margin before an attempt.
pub const CREDENTIAL_REFRESH_MARGIN: Duration = Duration::from_secs(120);

async fn blocking<T: Send + 'static>(
    git: &Arc<RemotePushGit>,
    observer: &dyn PushObserver,
    f: impl FnOnce(&RemotePushGit, &mut RecordingMessageSink) -> Result<T, EngineError> + Send + 'static,
) -> Result<T, EngineError> {
    let git = git.clone();
    let (result, sink) = tokio::task::spawn_blocking(move || {
        let mut sink = RecordingMessageSink::new();
        let result = f(&git, &mut sink);
        (result, sink)
    })
    .await
    .map_err(|e| EngineError::Git(format!("push task failed: {e}")))?;
    for message in sink.queued() {
        observer.message(message.clone());
    }
    result
}

impl LeasePush {
    /// Push `HEAD` of `git` to `branch` under `policy`. `heartbeat` is the
    /// lease's heartbeat interval (the window is expiry − 2 × heartbeat).
    pub fn new(
        git: Arc<RemotePushGit>,
        branch: impl Into<String>,
        policy: PushRetryPolicy,
        heartbeat: Duration,
    ) -> Self {
        Self {
            git,
            branch: branch.into(),
            policy,
            heartbeat,
            expiry: None,
            credentials: None,
        }
    }

    /// Follow the lease expiry from heartbeat answers.
    pub fn with_expiry(mut self, expiry: watch::Receiver<DateTime<Utc>>) -> Self {
        self.expiry = Some(expiry);
        self
    }

    /// Refresh the helper's credentials from `source` when they expire soon.
    pub fn with_credentials(
        mut self,
        helper: Arc<LeaseCredentialHelper>,
        source: Arc<dyn CredentialSource>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.credentials = Some((helper, source, expires_at));
        self
    }

    async fn refresh_credentials(&mut self) -> Result<(), WorkerEngineError> {
        let Some((helper, source, expires_at)) = &mut self.credentials else {
            return Ok(());
        };
        let margin = chrono::Duration::from_std(CREDENTIAL_REFRESH_MARGIN)
            .unwrap_or(chrono::Duration::zero());
        if expires_at.is_none_or(|at| at > Utc::now() + margin) {
            return Ok(());
        }
        let fresh = source.fresh().await?;
        if fresh.expires_at().is_some_and(|at| at <= Utc::now()) {
            return Err(WorkerEngineError::Invalid(
                "refreshed repository credentials already expired".into(),
            ));
        }
        helper
            .rotate(&fresh)
            .map_err(|e| WorkerEngineError::Io(e.to_string()))?;
        *expires_at = fresh.expires_at();
        Ok(())
    }

    pub async fn run(
        mut self,
        cancel: &CancellationToken,
        observer: &dyn PushObserver,
    ) -> Result<LeasePushOutcome, LeasePushFailure> {
        let operation_cancel = cancel.child_token();
        let _cancel_on_drop = operation_cancel.clone().drop_guard();
        let cancel = &operation_cancel;
        let mut attempt: u8 = 1;
        let fail = |kind, message: String, attempts| LeasePushFailure {
            kind,
            message,
            attempts,
        };
        loop {
            if cancel.is_cancelled() {
                return Err(fail(
                    LeasePushFailureKind::Cancelled,
                    "push cancelled".into(),
                    attempt - 1,
                ));
            }
            if let Some(expiry) = &self.expiry {
                self.policy.extend_to(*expiry.borrow(), self.heartbeat);
            }
            if Utc::now() >= self.policy.deadline {
                return Err(fail(
                    LeasePushFailureKind::Unreachable,
                    "lease push window expired".into(),
                    attempt - 1,
                ));
            }
            let refresh_guard = LeaseGitGuard {
                cancel: cancel.clone(),
                expiry: self.expiry.clone(),
                deadline: self.policy.deadline,
            };
            let refresh = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(fail(LeasePushFailureKind::Cancelled, "push cancelled".into(), attempt - 1)),
                _ = async { while refresh_guard.check().is_ok() { tokio::time::sleep(Duration::from_millis(10)).await; } } => return Err(fail(LeasePushFailureKind::Cancelled, "credential refresh cancelled or expired".into(), attempt - 1)),
                result = self.refresh_credentials() => result,
            };
            match refresh {
                Ok(()) => {}
                Err(WorkerEngineError::LeaseLost) => {
                    return Err(fail(
                        LeasePushFailureKind::LeaseLost,
                        "the lease was lost".into(),
                        attempt - 1,
                    ));
                }
                Err(WorkerEngineError::LinkRevoked) => {
                    cancel.cancel();
                    return Err(fail(
                        LeasePushFailureKind::LinkRevoked,
                        "the work server link was revoked".into(),
                        attempt - 1,
                    ));
                }
                Err(e) => {
                    // Failed refresh is never permission to keep using possibly
                    // invalid credentials. No Git request follows this failure.
                    return Err(fail(LeasePushFailureKind::Auth, e.to_string(), attempt - 1));
                }
            }
            let guard = LeaseGitGuard {
                cancel: cancel.clone(),
                expiry: self.expiry.clone(),
                deadline: self.policy.deadline,
            };
            if guard.check().is_err() {
                return Err(fail(
                    LeasePushFailureKind::Cancelled,
                    "push cancelled or expired".into(),
                    attempt - 1,
                ));
            }
            let operation_guard = guard.clone();
            let branch = self.branch.clone();
            let error = match blocking(&self.git, observer, move |git, sink| {
                let head = git.head(sink)?;
                if git
                    .origin_sha_guarded(&branch, sink, &operation_guard)?
                    .as_deref()
                    == Some(head.as_str())
                {
                    return Ok(LeasePushOutcome::AlreadyPublished { sha: head });
                }
                let sha = git.push_guarded(&branch, sink, &operation_guard)?;
                Ok(LeasePushOutcome::Pushed { sha, attempts: 0 })
            })
            .await
            {
                Ok(LeasePushOutcome::Pushed { sha, .. }) => {
                    if guard.check().is_err() {
                        return Err(fail(
                            LeasePushFailureKind::Cancelled,
                            "push fenced".into(),
                            attempt,
                        ));
                    }
                    return Ok(LeasePushOutcome::Pushed {
                        sha,
                        attempts: attempt,
                    });
                }
                Ok(done) => {
                    if guard.check().is_err() {
                        return Err(fail(
                            LeasePushFailureKind::Cancelled,
                            "push fenced".into(),
                            attempt,
                        ));
                    }
                    return Ok(done);
                }
                Err(error) => error,
            };
            if guard.check().is_err() {
                return Err(fail(
                    LeasePushFailureKind::Cancelled,
                    "push fenced".into(),
                    attempt,
                ));
            }
            let class = GitFailureClass::of(&error);
            match self.policy.decide(attempt, class, Utc::now()) {
                PushRetryDecision::Retry {
                    after,
                    attempt: next,
                } => {
                    observer.retry(&PushRetry {
                        attempt: next,
                        max_attempts: self.policy.max_attempts,
                        after,
                        reason: error.to_string(),
                    });
                    if !backoff(after, cancel).await {
                        return Err(fail(
                            LeasePushFailureKind::Cancelled,
                            "push cancelled during retry backoff".into(),
                            attempt,
                        ));
                    }
                    attempt = next;
                }
                PushRetryDecision::GiveUp(why) => {
                    return Err(fail(why.into(), error.to_string(), attempt));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::git::lease_transport::{LeaseTransport, LeaseUrlPolicy};
    use crate::engine::git::GitEngine;
    use std::path::Path;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        retries: Mutex<Vec<PushRetry>>,
    }

    impl PushObserver for Recorder {
        fn retry(&self, retry: &PushRetry) {
            self.retries.lock().unwrap().push(retry.clone());
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare origin and a clone with one unpublished commit.
    fn setup(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let bare = root.join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-q", "-b", "main"]);
        let clone = root.join("leases").join("lease_1");
        std::fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        std::fs::write(clone.join("a"), "a").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "a"]);
        git(&clone, &["push", "-q", bare.to_str().unwrap(), "main"]);
        git(&clone, &["remote", "add", "origin", bare.to_str().unwrap()]);
        std::fs::write(clone.join("b"), "b").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "b"]);
        (bare, clone)
    }

    fn pusher(clone: &Path, url: &str, policy: PushRetryPolicy) -> LeasePush {
        let transport = LeaseTransport::anonymous(LeaseUrlPolicy { allow_local: true });
        let git = GitEngine::new()
            .isolate_remote_push(clone, url)
            .unwrap()
            .with_lease_transport(transport);
        LeasePush::new(Arc::new(git), "wsp/item", policy, Duration::from_millis(10))
    }

    fn fast(window: Duration) -> PushRetryPolicy {
        PushRetryPolicy {
            max_attempts: 3,
            base_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(20),
            deadline: Utc::now() + chrono::Duration::from_std(window).unwrap(),
        }
    }

    #[tokio::test]
    async fn already_published_head_counts_as_success() {
        let tmp = tempfile::tempdir().unwrap();
        let (bare, clone) = setup(tmp.path());
        let url = bare.to_str().unwrap();
        let recorder = Recorder::default();
        let first = pusher(&clone, url, fast(Duration::from_secs(60)))
            .run(&CancellationToken::new(), &recorder)
            .await
            .unwrap();
        let head = git(&clone, &["rev-parse", "HEAD"]);
        assert_eq!(
            first,
            LeasePushOutcome::Pushed {
                sha: head.clone(),
                attempts: 1
            }
        );
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/wsp/item"]), head);
        let second = pusher(&clone, url, fast(Duration::from_secs(60)))
            .run(&CancellationToken::new(), &recorder)
            .await
            .unwrap();
        assert_eq!(second, LeasePushOutcome::AlreadyPublished { sha: head });
        assert!(recorder.retries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unreachable_remote_retries_with_events_then_fails_push_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let (_bare, clone) = setup(tmp.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // nothing listens: connection refused, no response
        let recorder = Recorder::default();
        let err = pusher(
            &clone,
            &format!("http://127.0.0.1:{port}/r.git"),
            fast(Duration::from_secs(60)),
        )
        .run(&CancellationToken::new(), &recorder)
        .await
        .unwrap_err();
        assert_eq!(err.kind, LeasePushFailureKind::Unreachable, "{err:?}");
        assert_eq!((err.stage(), err.code()), ("push", "push_unreachable"));
        assert_eq!(err.attempts, 3);
        let retries = recorder.retries.lock().unwrap().clone();
        assert_eq!(
            retries.iter().map(|r| r.attempt).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(retries[0].status_message().1.starts_with("push retry 2/3"));
    }

    #[tokio::test]
    async fn cancel_during_backoff_stops_retrying() {
        let tmp = tempfile::tempdir().unwrap();
        let (_bare, clone) = setup(tmp.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut policy = fast(Duration::from_secs(600));
        policy.base_backoff = Duration::from_secs(300);
        policy.max_backoff = Duration::from_secs(300);
        let cancel = CancellationToken::new();
        let recorder = Recorder::default();
        let run = pusher(&clone, &format!("http://127.0.0.1:{port}/r.git"), policy)
            .run(&cancel, &recorder);
        let canceller = async {
            while recorder.retries.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel();
        };
        let started = std::time::Instant::now();
        let (result, ()) = tokio::join!(run, canceller);
        assert_eq!(result.unwrap_err().kind, LeasePushFailureKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(60));
    }

    #[tokio::test]
    async fn rejected_push_is_not_retried_and_window_bounds_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let (bare, clone) = setup(tmp.path());
        // Someone else published a different wsp/item: non-fast-forward.
        let other = tmp.path().join("other");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        std::fs::write(other.join("z"), "z").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-qm", "z"]);
        git(
            &other,
            &["push", "-q", "origin", "HEAD:refs/heads/wsp/item"],
        );
        let recorder = Recorder::default();
        let err = pusher(
            &clone,
            bare.to_str().unwrap(),
            fast(Duration::from_secs(60)),
        )
        .run(&CancellationToken::new(), &recorder)
        .await
        .unwrap_err();
        assert_eq!(err.kind, LeasePushFailureKind::Rejected, "{err:?}");
        assert_eq!(err.attempts, 1);
        assert!(recorder.retries.lock().unwrap().is_empty());
        // A lease about to expire leaves no room for a retry.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = pusher(
            &clone,
            &format!("http://127.0.0.1:{port}/r.git"),
            fast(Duration::ZERO),
        )
        .run(&CancellationToken::new(), &recorder)
        .await
        .unwrap_err();
        assert_eq!(
            (err.kind, err.attempts),
            (LeasePushFailureKind::Unreachable, 0)
        );
    }

    #[test]
    fn auth_failure_maps_to_git_auth_stage_and_code() {
        let failure = LeasePushFailure {
            kind: PushGiveUp::Auth.into(),
            message: String::new(),
            attempts: 1,
        };
        assert_eq!(
            (failure.stage(), failure.code()),
            ("git_auth", "git_auth_failed")
        );
    }
}

#[cfg(all(test, unix))]
mod fencing_review_tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use crate::engine::git::{
        lease_transport::{HelperProgram, LeaseTransport, LeaseUrlPolicy},
        GitEngine,
    };
    use std::{path::Path, sync::Mutex};
    struct Silent;
    impl PushObserver for Silent {
        fn retry(&self, _: &PushRetry) {}
    }
    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("/usr/bin/git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn fixture(stage: &str) -> (tempfile::TempDir, Arc<RemotePushGit>) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=test@test",
                "commit",
                "--allow-empty",
                "-qm",
                "seed",
            ],
        );
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("git");
        std::fs::write(&fake, format!("#!/bin/sh\nfor arg in \"$@\"; do\n case \"$arg\" in\n ls-remote|push)\n  echo \"$arg\" >> '{}/requests'\n  if [ \"$arg\" = '{stage}' ]; then\n   touch '{}/started'\n   sleep 10\n   touch '{}/stale-success'\n  fi\n  exit 0;;\n esac\ndone\nexec /usr/bin/git \"$@\"\n", root.path().display(), root.path().display(), root.path().display())).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env =
            EnvSnapshot::with_overrides([("PATH", format!("{}:/usr/bin:/bin", bin.display()))]);
        let remote = GitEngine::with_env(env)
            .isolate_remote_push(root.path(), "https://git.example/r")
            .unwrap()
            .with_lease_transport(LeaseTransport::default());
        (root, Arc::new(remote))
    }
    fn policy() -> PushRetryPolicy {
        PushRetryPolicy::for_lease(
            Utc::now() + chrono::Duration::seconds(60),
            Duration::from_secs(1),
        )
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fencing_during_ls_remote_or_upload_kills_git_and_never_reports_success() {
        for stage in ["ls-remote", "push"] {
            let (root, remote) = fixture(stage);
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            let path = root.path().join("started");
            let signal = tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while !path.exists() {
                    assert!(tokio::time::Instant::now() < deadline);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                token.cancel();
            });
            let result = tokio::time::timeout(
                Duration::from_secs(6),
                LeasePush::new(remote, "work", policy(), Duration::from_secs(1))
                    .run(&cancel, &Silent),
            )
            .await
            .unwrap()
            .unwrap_err();
            signal.await.unwrap();
            assert_eq!(result.kind, LeasePushFailureKind::Cancelled);
            let requests = std::fs::read_to_string(root.path().join("requests")).unwrap();
            if stage == "ls-remote" {
                assert!(!requests.lines().any(|line| line == "push"));
            }
            assert!(!root.path().join("stale-success").exists());
        }
    }
    #[tokio::test]
    async fn expired_initial_window_makes_zero_remote_requests() {
        let (root, remote) = fixture("push");
        let mut policy = policy();
        policy.deadline = Utc::now() - chrono::Duration::seconds(1);
        let error = LeasePush::new(remote, "work", policy, Duration::from_secs(1))
            .run(&CancellationToken::new(), &Silent)
            .await
            .unwrap_err();
        assert_eq!(error.attempts, 0);
        assert!(!root.path().join("requests").exists());
    }
    struct Revoked(Mutex<usize>);
    #[async_trait]
    impl CredentialSource for Revoked {
        async fn fresh(&self) -> Result<LeaseCredentials, WorkerEngineError> {
            *self.0.lock().unwrap() += 1;
            Err(WorkerEngineError::LinkRevoked)
        }
    }
    #[tokio::test]
    async fn revoked_refresh_makes_zero_subsequent_git_requests() {
        let (root, remote) = fixture("push");
        let creds = LeaseCredentials::from_wsp(Some(&awman_wsp::Credentials {
            kind: awman_wsp::CredentialKind::Bearer,
            token: Some("old-token".into()),
            username: None,
            expires_at: None,
        }))
        .unwrap();
        let helper = Arc::new(
            LeaseCredentialHelper::install(
                &root.path().join("private"),
                "https://git.example/r",
                &creds,
                HelperProgram::awman("/unused".into()),
                LeaseUrlPolicy::default(),
            )
            .unwrap(),
        );
        let source = Arc::new(Revoked(Mutex::new(0)));
        let error = LeasePush::new(remote, "work", policy(), Duration::from_secs(1))
            .with_credentials(helper, source.clone(), Some(Utc::now()))
            .run(&CancellationToken::new(), &Silent)
            .await
            .unwrap_err();
        assert_eq!(error.kind, LeasePushFailureKind::LinkRevoked);
        assert_eq!(*source.0.lock().unwrap(), 1);
        assert!(!root.path().join("requests").exists());
    }
}
