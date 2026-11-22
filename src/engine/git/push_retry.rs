//! Bounded retry policy of a push, shared by push sessions and leases.
//!
//! Only [`GitFailureClass::Transport`] failures (no remote response:
//! connection refused, DNS, connect/read timeout before a response, reset
//! before a response) are retried, with capped exponential backoff, at most
//! [`PUSH_MAX_ATTEMPTS`] attempts, and only while the next attempt starts
//! within [`PUSH_RETRY_WINDOW`] of the first failure and before `deadline`
//! (a command deadline, or lease expiry minus two heartbeats).
//! Authentication failures, remote refusals and anything else are terminal
//! on the first attempt. The loop itself is
//! [`super::push_publish::PushPublisher`].

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::auth_classify::GitFailureClass;

/// Total attempts, including the first.
pub const PUSH_MAX_ATTEMPTS: u8 = 4;
/// Backoff before the first retry; it doubles (2 s, 4 s, 8 s) up to
/// [`PUSH_MAX_BACKOFF`]. No jitter: one client publishes one branch.
pub const PUSH_BASE_BACKOFF: Duration = Duration::from_secs(2);
pub const PUSH_MAX_BACKOFF: Duration = Duration::from_secs(15);
/// No retry attempt starts later than this after the first transport failure.
pub const PUSH_RETRY_WINDOW: Duration = Duration::from_secs(30);
/// Process bound of the first attempt's head check and push.
pub const PUSH_FIRST_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Process bound of a retry attempt's head check and push.
pub const PUSH_RETRY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u8,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// No attempt starts after this instant (`DateTime::<Utc>::MAX_UTC`
    /// when the command has no deadline).
    pub deadline: DateTime<Utc>,
    /// No retry starts later than this after the first transport failure.
    pub window: Duration,
    /// Shared process budget of the first attempt (head resolution, check, push).
    pub first_attempt_timeout: Duration,
    /// Shared process budget of a retry attempt (head resolution, check, push).
    pub retry_attempt_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRetryDecision {
    /// Wait `after`, then make attempt number `attempt` (1-based).
    Retry {
        after: Duration,
        attempt: u8,
    },
    GiveUp(PushGiveUp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushGiveUp {
    /// `failure.stage: "git_auth"`, `failure.code: "git_auth_failed"`.
    Auth,
    /// The remote answered and refused.
    Rejected,
    /// Transport failures until attempts, the window or the deadline ran
    /// out: `failure.code: "push_unreachable"`.
    Unreachable,
    /// Cancelled during backoff.
    Cancelled,
    /// Not a transport, auth or rejection failure.
    Other,
}

impl PushRetryPolicy {
    /// The push-session policy: the constants above, bounded by the
    /// command's deadline when it has one.
    pub fn for_command(command_deadline: Option<DateTime<Utc>>) -> Self {
        Self {
            max_attempts: PUSH_MAX_ATTEMPTS,
            base_backoff: PUSH_BASE_BACKOFF,
            max_backoff: PUSH_MAX_BACKOFF,
            deadline: command_deadline.unwrap_or(DateTime::<Utc>::MAX_UTC),
            window: PUSH_RETRY_WINDOW,
            first_attempt_timeout: PUSH_FIRST_ATTEMPT_TIMEOUT,
            retry_attempt_timeout: PUSH_RETRY_ATTEMPT_TIMEOUT,
        }
    }

    /// The lease policy: the same constants, bounded by
    /// `lease_expires_at - 2 × heartbeat`.
    ///
    /// A server may renew leases for no longer than two heartbeats. The
    /// margin is then half of what is left, so a live lease always has a
    /// window to push in.
    pub fn for_lease(lease_expires_at: DateTime<Utc>, heartbeat: Duration) -> Self {
        Self::for_lease_at(Utc::now(), lease_expires_at, heartbeat)
    }

    fn for_lease_at(
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
        heartbeat: Duration,
    ) -> Self {
        Self {
            deadline: lease_deadline(now, lease_expires_at, heartbeat),
            ..Self::for_command(None)
        }
    }

    /// Move the deadline when a heartbeat extends the lease.
    pub fn extend_to(&mut self, lease_expires_at: DateTime<Utc>, heartbeat: Duration) {
        self.deadline = lease_deadline(Utc::now(), lease_expires_at, heartbeat);
    }

    /// The deadline, when one bounds the push (it is not the far future).
    pub fn bounded_deadline(&self) -> Option<DateTime<Utc>> {
        (self.deadline != DateTime::<Utc>::MAX_UTC).then_some(self.deadline)
    }

    /// Process bound of each Git call of `attempt` (1-based).
    pub fn attempt_timeout(&self, attempt: u8) -> Duration {
        if attempt <= 1 {
            self.first_attempt_timeout
        } else {
            self.retry_attempt_timeout
        }
    }

    /// What to do after attempt `attempt` (1-based) failed with `class` at
    /// `now`; the first transport failure of this push happened at
    /// `first_failure`.
    pub fn decide(
        &self,
        attempt: u8,
        class: GitFailureClass,
        now: DateTime<Utc>,
        first_failure: DateTime<Utc>,
    ) -> PushRetryDecision {
        let give_up = |why| PushRetryDecision::GiveUp(why);
        match class {
            GitFailureClass::Auth => return give_up(PushGiveUp::Auth),
            GitFailureClass::RemoteRejected => return give_up(PushGiveUp::Rejected),
            GitFailureClass::Other => return give_up(PushGiveUp::Other),
            GitFailureClass::Transport => {}
        }
        if attempt >= self.max_attempts {
            return give_up(PushGiveUp::Unreachable);
        }
        let shift = u32::from(attempt.saturating_sub(1)).min(16);
        let after = self
            .base_backoff
            .saturating_mul(1u32 << shift)
            .min(self.max_backoff);
        let chrono = |d: Duration| chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX);
        let Some(starts) = now.checked_add_signed(chrono(after)) else {
            return give_up(PushGiveUp::Unreachable);
        };
        let window_ends = first_failure
            .checked_add_signed(chrono(self.window))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        if starts >= window_ends || starts >= self.deadline {
            return give_up(PushGiveUp::Unreachable);
        }
        PushRetryDecision::Retry {
            after,
            attempt: attempt + 1,
        }
    }
}

fn lease_deadline(
    now: DateTime<Utc>,
    lease_expires_at: DateTime<Utc>,
    heartbeat: Duration,
) -> DateTime<Utc> {
    let full = chrono::Duration::from_std(heartbeat * 2).unwrap_or(chrono::Duration::zero());
    let left = (lease_expires_at - now).max(chrono::Duration::zero());
    let margin = if left > full { full } else { left / 2 };
    lease_expires_at - margin
}

/// Wait `after` unless `cancel` fires first. `false` means cancelled.
pub async fn backoff(after: Duration, cancel: &tokio_util::sync::CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(after) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }

    #[test]
    fn decide_retries_only_transport_with_capped_doubling_within_attempts_window_and_deadline() {
        let policy = PushRetryPolicy::for_command(None);
        let retry = |after: u64, attempt| PushRetryDecision::Retry {
            after: Duration::from_secs(after),
            attempt,
        };
        let unreachable = PushRetryDecision::GiveUp(PushGiveUp::Unreachable);
        for (class, why) in [
            (GitFailureClass::Auth, PushGiveUp::Auth),
            (GitFailureClass::RemoteRejected, PushGiveUp::Rejected),
            (GitFailureClass::Other, PushGiveUp::Other),
        ] {
            assert_eq!(
                policy.decide(1, class, at(0), at(0)),
                PushRetryDecision::GiveUp(why)
            );
        }
        let t = GitFailureClass::Transport;
        assert_eq!(policy.decide(1, t, at(0), at(0)), retry(2, 2));
        assert_eq!(policy.decide(2, t, at(3), at(0)), retry(4, 3));
        assert_eq!(policy.decide(3, t, at(8), at(0)), retry(8, 4));
        // Attempts are exhausted.
        assert_eq!(policy.decide(4, t, at(17), at(0)), unreachable);
        // The window counts from the first failure: a retry starting at 30 s
        // (or later) never starts.
        assert_eq!(policy.decide(3, t, at(22), at(0)), unreachable);
        assert_eq!(policy.decide(3, t, at(21), at(0)), retry(8, 4));
        // Capped backoff.
        let mut capped = policy.clone();
        capped.max_attempts = 10;
        capped.window = Duration::from_secs(3600);
        assert_eq!(capped.decide(5, t, at(0), at(0)), retry(15, 6));
        // A deadline bounds the next start.
        let bounded = PushRetryPolicy::for_command(Some(at(5)));
        assert_eq!(bounded.decide(1, t, at(2), at(2)), retry(2, 2));
        assert_eq!(bounded.decide(1, t, at(3), at(3)), unreachable);
        // A policy without a deadline has none to report.
        assert_eq!(policy.bounded_deadline(), None);
        assert_eq!(bounded.bounded_deadline(), Some(at(5)));
    }

    #[test]
    fn worst_case_wait_is_seconds_and_attempt_timeouts_differ() {
        let policy = PushRetryPolicy::for_command(None);
        let total: Duration = (1..policy.max_attempts)
            .map(|attempt| {
                policy
                    .base_backoff
                    .saturating_mul(1 << (attempt - 1))
                    .min(policy.max_backoff)
            })
            .sum();
        assert!(total < policy.window, "{total:?}");
        assert_eq!(policy.attempt_timeout(1), PUSH_FIRST_ATTEMPT_TIMEOUT);
        assert_eq!(policy.attempt_timeout(2), PUSH_RETRY_ATTEMPT_TIMEOUT);
    }

    #[test]
    fn lease_deadline_keeps_two_heartbeats_or_half_of_what_is_left() {
        let heartbeat = Duration::from_secs(10);
        let policy = PushRetryPolicy::for_lease_at(at(0), at(100), heartbeat);
        assert_eq!(policy.deadline, at(80));
        assert_eq!(policy.max_attempts, PUSH_MAX_ATTEMPTS);
        assert_eq!(policy.window, PUSH_RETRY_WINDOW);
        let short = PushRetryPolicy::for_lease_at(at(0), at(10), heartbeat);
        assert_eq!(short.deadline, at(5));
        let expired = PushRetryPolicy::for_lease_at(at(20), at(10), heartbeat);
        assert_eq!(expired.deadline, at(10));
    }
}
