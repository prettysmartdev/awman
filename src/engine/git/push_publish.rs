//! The one bounded, head-checked push loop for push sessions and leases.
//!
//! [`PushPublisher::run`] publishes `HEAD` of an isolated
//! [`RemotePushGit`] to `refs/heads/<branch>`:
//!
//! 1. Before every attempt it checks (`ls-remote`, through the same
//!    transport builder) where the remote branch points. Already at `HEAD`
//!    means the publication happened, possibly by an earlier attempt whose
//!    response was lost: the outcome is [`PushOutcome::AlreadyPublished`] and
//!    nothing is pushed again. A different or absent head continues to the
//!    push; a branch that advanced to another commit is refused by the remote
//!    as non-fast-forward, which is a rejection, never an "unreachable
//!    success".
//! 2. It pushes once, never forced.
//! 3. Only a failure before any remote response (see
//!    [`GitFailureClass::Transport`]) of the check or the push is retried,
//!    under [`PushRetryPolicy`]. Every retry is reported to the
//!    [`PushObserver`] as a typed [`PushRetry`] (attempt, operation, delay and
//!    sanitized remote; never Git output or credentials). Backoff observes
//!    cancellation.
//!
//! What differs between push sessions and leases is the [`PushGate`]: a
//! push session uses [`NoGate`]; a lease follows heartbeat expiry, refreshes
//! credentials and fences a lost epoch. Retry state lives in local variables
//! only: nothing is persisted outside the caller's command/lease record.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::auth_classify::GitFailureClass;
use super::push_retry::{backoff, PushGiveUp, PushRetryDecision, PushRetryPolicy};
use super::remote_push::RemotePushGit;
use super::transport::TransportGuard;
use crate::data::execution_event::EventPayload;
use crate::data::fs::api_db::{failure_codes, failure_stages};
use crate::data::git_transport::{GitTransportOp, SafeRemote};
use crate::data::message::{RecordingMessageSink, UserMessage};
use crate::engine::error::EngineError;

/// One scheduled retry. Carries no Git output and no reason text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRetry {
    /// The attempt about to be made (the first retry is attempt 2).
    pub attempt: u8,
    pub max_attempts: u8,
    /// The operation that got no response (`push` or the `ls-remote` head check).
    pub operation: GitTransportOp,
    /// Backoff before this attempt.
    pub delay: Duration,
    pub remote: SafeRemote,
}

impl PushRetry {
    /// The `push_retry` execution event recording this retry.
    pub fn event(&self) -> EventPayload {
        EventPayload::PushRetry {
            attempt: self.attempt,
            max_attempts: self.max_attempts,
            operation: self.operation.as_str().into(),
            delay_ms: u64::try_from(self.delay.as_millis()).unwrap_or(u64::MAX),
            remote: self.remote.to_string(),
        }
    }
}

/// Where push progress goes.
pub trait PushObserver: Send + Sync {
    fn retry(&self, retry: &PushRetry);
    fn message(&self, _message: UserMessage) {}
}

/// Caller hooks around every attempt. The defaults ([`NoGate`]) suit a push
/// session; a lease supplies expiry, credential refresh and fencing.
#[async_trait]
pub trait PushGate: Send {
    /// Before the deadline check of every attempt (a lease moves the
    /// deadline to follow its latest heartbeat).
    fn update_policy(&self, _policy: &mut PushRetryPolicy) {}

    /// Before the Git requests of every attempt, after the deadline check.
    /// `Err` ends the loop with that failure and no Git request.
    async fn before_attempt(
        &mut self,
        _cancel: &CancellationToken,
        _deadline: DateTime<Utc>,
    ) -> Result<(), PushFailure> {
        Ok(())
    }

    /// An expiry every Git process stops at (a lease's heartbeat expiry).
    fn expiry(&self) -> Option<watch::Receiver<DateTime<Utc>>> {
        None
    }

    /// Checked after every Git call. `false` (an expired or lost lease)
    /// ends the loop as cancelled: a landed push is never claimed by a lost
    /// epoch.
    fn still_valid(&self) -> bool {
        true
    }
}

/// The push-session gate: no expiry, no credential refresh, no fencing.
pub struct NoGate;
impl PushGate for NoGate {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed {
        sha: String,
        attempts: u8,
    },
    /// The remote branch already pointed at `HEAD` (an earlier attempt or
    /// the command itself published it).
    AlreadyPublished {
        sha: String,
    },
    /// [`PushPublisher::verify_only`]: the remote branch is elsewhere and
    /// nothing was pushed.
    NotPushed {
        remote_sha: Option<String>,
    },
}

impl PushOutcome {
    /// The published head, when the remote branch points at `HEAD`.
    pub fn sha(&self) -> Option<&str> {
        match self {
            Self::Pushed { sha, .. } | Self::AlreadyPublished { sha } => Some(sha),
            Self::NotPushed { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushFailureKind {
    Auth,
    Rejected,
    Unreachable,
    Cancelled,
    /// Fresh credentials were refused because the lease was lost.
    LeaseLost,
    LinkRevoked,
    Other,
}

impl From<PushGiveUp> for PushFailureKind {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushFailure {
    pub kind: PushFailureKind,
    /// Stable and credential-free: names the operation and the safe remote.
    pub message: String,
    /// Attempts that made a Git request.
    pub attempts: u8,
}

impl PushFailure {
    pub fn new(kind: PushFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            attempts: 0,
        }
    }

    /// `CommandFailure.stage`.
    pub fn stage(&self) -> &'static str {
        match self.kind {
            PushFailureKind::Auth => failure_stages::GIT_AUTH,
            _ => failure_stages::PUSH,
        }
    }

    /// `CommandFailure.code`.
    pub fn code(&self) -> &'static str {
        match self.kind {
            PushFailureKind::Auth => failure_codes::GIT_AUTH_FAILED,
            PushFailureKind::Unreachable => failure_codes::PUSH_UNREACHABLE,
            PushFailureKind::Cancelled => failure_codes::CANCELLED,
            _ => failure_codes::PUSH_FAILED,
        }
    }
}

pub struct PushPublisher {
    git: Arc<RemotePushGit>,
    branch: String,
    policy: PushRetryPolicy,
    publish: bool,
}

/// Run `f` on a blocking thread and forward its messages to `observer`.
async fn blocking<T: Send + 'static>(
    git: &Arc<RemotePushGit>,
    observer: &dyn PushObserver,
    f: impl FnOnce(&RemotePushGit, &mut RecordingMessageSink) -> T + Send + 'static,
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
    Ok(result)
}

type Attempt = Result<PushOutcome, (GitTransportOp, EngineError)>;

impl PushPublisher {
    /// Publish `HEAD` of `git` to `branch` under `policy`.
    pub fn new(
        git: Arc<RemotePushGit>,
        branch: impl Into<String>,
        policy: PushRetryPolicy,
    ) -> Self {
        Self {
            git,
            branch: branch.into(),
            policy,
            publish: true,
        }
    }

    /// Only run the (retried) head check: never push. A remote branch that
    /// is not at `HEAD` yields [`PushOutcome::NotPushed`].
    pub fn verify_only(mut self) -> Self {
        self.publish = false;
        self
    }

    pub async fn run(
        mut self,
        cancel: &CancellationToken,
        gate: &mut dyn PushGate,
        observer: &dyn PushObserver,
    ) -> Result<PushOutcome, PushFailure> {
        // Git processes and backoff stop with the caller's token, and a
        // dropped run stops its Git processes.
        let operation_cancel = cancel.child_token();
        let _cancel_on_drop = operation_cancel.clone().drop_guard();
        let cancel = &operation_cancel;
        let remote = self.git.remote().clone();
        let fail = |kind, message: String, attempts| PushFailure {
            kind,
            message,
            attempts,
        };
        let mut attempt: u8 = 1;
        let mut first_failure: Option<tokio::time::Instant> = None;
        let mut policy_deadline = self.policy.deadline;
        let monotonic_deadline = |deadline: DateTime<Utc>| {
            deadline
                .signed_duration_since(Utc::now())
                .to_std()
                .ok()
                .and_then(|left| tokio::time::Instant::now().checked_add(left))
        };
        let mut deadline = monotonic_deadline(policy_deadline);
        loop {
            let made = attempt - 1;
            if cancel.is_cancelled() {
                return Err(fail(
                    PushFailureKind::Cancelled,
                    "push cancelled".into(),
                    made,
                ));
            }
            gate.update_policy(&mut self.policy);
            if self.policy.deadline != policy_deadline {
                policy_deadline = self.policy.deadline;
                deadline = monotonic_deadline(policy_deadline);
            }
            let window_expired = |first: Option<tokio::time::Instant>| {
                first.is_some_and(|first| first.elapsed() >= self.policy.window)
            };
            if deadline.is_some_and(|end| tokio::time::Instant::now() >= end)
                || Utc::now() >= self.policy.deadline
                || window_expired(first_failure)
            {
                return Err(fail(
                    PushFailureKind::Unreachable,
                    format!("git push to {remote}: the push window expired"),
                    made,
                ));
            }
            let refresh_end = first_failure
                .map(|first| first + self.policy.window)
                .into_iter()
                .chain(deadline)
                .min();
            let refresh = async {
                match refresh_end {
                    Some(end) => tokio::time::timeout_at(
                        end,
                        gate.before_attempt(cancel, self.policy.deadline),
                    )
                    .await
                    .map_err(|_| {
                        fail(
                            PushFailureKind::Unreachable,
                            format!("git push to {remote}: the push window expired"),
                            made,
                        )
                    })?,
                    None => gate.before_attempt(cancel, self.policy.deadline).await,
                }
            };
            let refreshed = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(fail(PushFailureKind::Cancelled, "push cancelled during credential refresh".into(), made)),
                result = refresh => result,
            };
            if let Err(mut failure) = refreshed {
                failure.attempts = made;
                return Err(failure);
            }
            // Observers, backoff scheduling and credential refresh all consume
            // the original retry-start window.
            if window_expired(first_failure)
                || deadline.is_some_and(|end| tokio::time::Instant::now() >= end)
            {
                return Err(fail(
                    PushFailureKind::Unreachable,
                    format!("git push to {remote}: the push window expired"),
                    made,
                ));
            }
            let expiry = gate.expiry();
            let guard = TransportGuard {
                cancel: cancel.clone(),
                deadline: if expiry.is_none() {
                    self.policy.bounded_deadline()
                } else {
                    None
                },
                expiry,
                timeout: self.policy.attempt_timeout(attempt),
                attempt_end: Some(
                    std::time::Instant::now()
                        + deadline.map_or(self.policy.attempt_timeout(attempt), |end| {
                            end.saturating_duration_since(tokio::time::Instant::now())
                                .min(self.policy.attempt_timeout(attempt))
                        }),
                ),
                inactivity: false,
                start_deadline: first_failure.map(|first| {
                    (
                        std::time::Instant::now()
                            + self.policy.window.saturating_sub(first.elapsed()),
                        Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    )
                }),
            };
            if guard.check().is_err() || !gate.still_valid() {
                return Err(self.stopped(cancel, &*gate, &remote, made));
            }
            let operation_guard = guard.clone();
            let branch = self.branch.clone();
            let publish = self.publish;
            let result: Attempt = blocking(&self.git, observer, move |git, sink| {
                let head = git
                    .head_guarded(&operation_guard)
                    .map_err(|e| (GitTransportOp::Push, e))?;
                let published = git
                    .remote_head(&branch, sink, &operation_guard)
                    .map_err(|e| (GitTransportOp::LsRemote, e))?;
                if published.as_deref() == Some(head.as_str()) {
                    return Ok(PushOutcome::AlreadyPublished { sha: head });
                }
                if !publish {
                    return Ok(PushOutcome::NotPushed {
                        remote_sha: published,
                    });
                }
                git.push_head(&branch, sink, &operation_guard)
                    .map_err(|e| (GitTransportOp::Push, e))?;
                Ok(PushOutcome::Pushed {
                    sha: head,
                    attempts: attempt,
                })
            })
            .await
            .unwrap_or_else(|e| Err((GitTransportOp::Push, e)));
            // A lost epoch, cancellation or deadline after any Git call
            // wins over its result: a landed push is never claimed then.
            if guard.check().is_err() || !gate.still_valid() {
                return Err(self.stopped(cancel, &*gate, &remote, attempt));
            }
            let (operation, error) = match result {
                Ok(outcome) => return Ok(outcome),
                Err(failed) => failed,
            };
            let now = Utc::now();
            let class = GitFailureClass::of(&error);
            // The attempt budget ends at the push deadline. A lease's guard
            // follows expiry rather than that deadline, so a local Git call
            // the budget killed surfaces as a plain error: the push window
            // ran out, exactly as when the deadline stops the next attempt.
            // An auth failure or remote refusal stays what the remote said.
            if matches!(class, GitFailureClass::Other | GitFailureClass::Transport)
                && (deadline.is_some_and(|end| tokio::time::Instant::now() >= end)
                    || now >= self.policy.deadline)
            {
                return Err(fail(
                    PushFailureKind::Unreachable,
                    format!("git push to {remote}: the push window expired"),
                    attempt,
                ));
            }
            let first = *first_failure.get_or_insert_with(tokio::time::Instant::now);
            let first =
                now - chrono::Duration::from_std(first.elapsed()).unwrap_or(chrono::Duration::MAX);
            match self.policy.decide(attempt, class, now, first) {
                PushRetryDecision::Retry {
                    after,
                    attempt: next,
                } => {
                    observer.retry(&PushRetry {
                        attempt: next,
                        max_attempts: self.policy.max_attempts,
                        operation,
                        delay: after,
                        remote: remote.clone(),
                    });
                    if !backoff(after, cancel).await {
                        return Err(fail(
                            PushFailureKind::Cancelled,
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

    /// Why a guard stopped the loop: cancellation and a lost/expired lease
    /// are `cancelled`; a passed command deadline is `push_unreachable`.
    fn stopped(
        &self,
        cancel: &CancellationToken,
        gate: &dyn PushGate,
        remote: &SafeRemote,
        attempts: u8,
    ) -> PushFailure {
        let (kind, message) = if cancel.is_cancelled() {
            (PushFailureKind::Cancelled, "push cancelled".to_string())
        } else if !gate.still_valid() || gate.expiry().is_some() {
            (PushFailureKind::Cancelled, "push fenced".to_string())
        } else {
            (
                PushFailureKind::Unreachable,
                format!("git push to {remote}: the push deadline passed"),
            )
        };
        PushFailure {
            kind,
            message,
            attempts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_codes_follow_the_shared_table() {
        for (kind, stage, code) in [
            (PushFailureKind::Auth, "git_auth", "git_auth_failed"),
            (PushFailureKind::Unreachable, "push", "push_unreachable"),
            (PushFailureKind::Rejected, "push", "push_failed"),
            (PushFailureKind::Other, "push", "push_failed"),
            (PushFailureKind::Cancelled, "push", "cancelled"),
            (PushFailureKind::LeaseLost, "push", "push_failed"),
            (PushFailureKind::LinkRevoked, "push", "push_failed"),
        ] {
            let failure = PushFailure::new(kind, "m");
            assert_eq!((failure.stage(), failure.code()), (stage, code), "{kind:?}");
        }
    }

    #[test]
    fn retry_event_has_only_attempt_operation_delay_and_safe_remote() {
        let retry = PushRetry {
            attempt: 2,
            max_attempts: 4,
            operation: GitTransportOp::LsRemote,
            delay: Duration::from_millis(2000),
            remote: SafeRemote::from_url("https://user@example.invalid/o/r.git?token=x"),
        };
        let event = retry.event();
        assert_eq!(event.sse_event_type(), "push_retry");
        // The envelope publishes `type` as `sse_event_type()`; the data is
        // exactly these five fields.
        let wire = serde_json::json!({"type":"push_retry","data":{
            "attempt":2,"max_attempts":4,"operation":"ls-remote",
            "delay_ms":2000,"remote":"example.invalid/o/r.git"}});
        assert_eq!(serde_json::to_value(&event).unwrap()["data"], wire["data"]);
        let parsed: EventPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            serde_json::to_value(&event).unwrap()
        );
        assert_eq!(
            event.to_plain_text().unwrap(),
            "push retry 2/4 in 2000 ms (ls-remote to example.invalid/o/r.git)"
        );
    }
}
