//! Bounded retry of a lease push.
//!
//! Only [`GitFailureClass::Transport`] failures (no remote response) are
//! retried, with short exponential backoff, and only while the next attempt
//! can still start before `deadline` (lease expiry minus two heartbeats).
//! Authentication failures and remote refusals are never retried.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::auth_classify::GitFailureClass;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u8,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// No attempt starts after this instant.
    pub deadline: DateTime<Utc>,
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
    /// Transport failures until attempts or the lease window ran out:
    /// `failure.code: "push_unreachable"`.
    Unreachable,
    /// Cancelled during backoff.
    Cancelled,
    /// Not a transport, auth or rejection failure.
    Other,
}

impl PushRetryPolicy {
    /// The default lease policy: 4 attempts, 2 s doubling to at most 15 s,
    /// bounded by `lease_expires_at - 2 × heartbeat`.
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
        let full = chrono::Duration::from_std(heartbeat * 2).unwrap_or(chrono::Duration::zero());
        let left = (lease_expires_at - now).max(chrono::Duration::zero());
        let margin = if left > full { full } else { left / 2 };
        Self {
            max_attempts: 4,
            base_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(15),
            deadline: lease_expires_at - margin,
        }
    }

    /// Move the deadline when a heartbeat extends the lease.
    pub fn extend_to(&mut self, lease_expires_at: DateTime<Utc>, heartbeat: Duration) {
        self.deadline = Self::for_lease(lease_expires_at, heartbeat).deadline;
    }

    /// What to do after attempt `attempt` (1-based) failed with `class`.
    pub fn decide(
        &self,
        attempt: u8,
        class: GitFailureClass,
        now: DateTime<Utc>,
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
        let starts = now + chrono::Duration::from_std(after).unwrap_or(chrono::Duration::MAX);
        if starts >= self.deadline {
            return give_up(PushGiveUp::Unreachable);
        }
        PushRetryDecision::Retry {
            after,
            attempt: attempt + 1,
        }
    }
}

/// Wait `after` unless `cancel` fires first. `false` means cancelled.
pub async fn backoff(after: Duration, cancel: &tokio_util::sync::CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(after) => true,
    }
}
