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
