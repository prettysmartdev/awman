//! Reliable push of a lease's branch.
//!
//! [`LeasePush`] runs the shared [`PushPublisher`] (the same head check,
//! classification and bounded retry as push sessions) with a [`LeaseGate`]:
//! the retry window follows the lease expiry from the latest heartbeat and
//! ends two heartbeats before it, credentials are refreshed before an
//! attempt when they expire soon, and an expired or lost lease stops every
//! Git process and is reported as cancelled, never as a push. Each retry is
//! reported to the [`PushObserver`] (the caller records the typed
//! `push_retry` event).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use crate::engine::git::lease_transport::{LeaseCredentialHelper, LeaseCredentials};
use crate::engine::git::push_publish::{PushGate, PushPublisher};
use crate::engine::git::push_retry::PushRetryPolicy;
use crate::engine::git::remote_push::{LeaseGitGuard, RemotePushGit};

pub use crate::engine::git::push_publish::{
    PushFailure as LeasePushFailure, PushFailureKind as LeasePushFailureKind, PushObserver,
    PushOutcome as LeasePushOutcome, PushRetry,
};

/// Fresh repository credentials for a long run (`POST /v1/leases/{id}/credentials`).
#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn fresh(&self) -> Result<LeaseCredentials, WorkerEngineError>;
}

/// Refresh credentials expiring within this margin before an attempt.
pub const CREDENTIAL_REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// The helper to rotate, where fresh credentials come from, and when the
/// current ones expire.
type CredentialRefresh = (
    Arc<LeaseCredentialHelper>,
    Arc<dyn CredentialSource>,
    Option<DateTime<Utc>>,
);

pub struct LeasePush {
    git: Arc<RemotePushGit>,
    branch: String,
    policy: PushRetryPolicy,
    gate: LeaseGate,
}

/// The lease's [`PushGate`]: heartbeat expiry, credential refresh, fencing.
pub struct LeaseGate {
    heartbeat: Duration,
    expiry: Option<watch::Receiver<DateTime<Utc>>>,
    credentials: Option<CredentialRefresh>,
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
            gate: LeaseGate {
                heartbeat,
                expiry: None,
                credentials: None,
            },
        }
    }

    /// Follow the lease expiry from heartbeat answers.
    pub fn with_expiry(mut self, expiry: watch::Receiver<DateTime<Utc>>) -> Self {
        self.gate.expiry = Some(expiry);
        self
    }

    /// Refresh the helper's credentials from `source` when they expire soon.
    pub fn with_credentials(
        mut self,
        helper: Arc<LeaseCredentialHelper>,
        source: Arc<dyn CredentialSource>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.gate.credentials = Some((helper, source, expires_at));
        self
    }

    pub async fn run(
        mut self,
        cancel: &CancellationToken,
        observer: &dyn PushObserver,
    ) -> Result<LeasePushOutcome, LeasePushFailure> {
        PushPublisher::new(self.git, self.branch, self.policy)
            .run(cancel, &mut self.gate, observer)
            .await
    }
}

impl LeaseGate {
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
}

#[async_trait]
impl PushGate for LeaseGate {
    fn update_policy(&self, policy: &mut PushRetryPolicy) {
        if let Some(expiry) = &self.expiry {
            policy.extend_to(*expiry.borrow(), self.heartbeat);
        }
    }

    async fn before_attempt(
        &mut self,
        cancel: &CancellationToken,
        deadline: DateTime<Utc>,
    ) -> Result<(), LeasePushFailure> {
        use crate::engine::git::push_publish::PushFailure;
        let guard = LeaseGitGuard {
            cancel: cancel.clone(),
            expiry: self.expiry.clone(),
            deadline,
        };
        let refresh = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(PushFailure::new(LeasePushFailureKind::Cancelled, "push cancelled"));
            }
            _ = async { while guard.check().is_ok() { tokio::time::sleep(Duration::from_millis(10)).await; } } => {
                return Err(PushFailure::new(
                    LeasePushFailureKind::Cancelled,
                    "credential refresh cancelled or expired",
                ));
            }
            result = self.refresh_credentials() => result,
        };
        match refresh {
            Ok(()) => Ok(()),
            Err(WorkerEngineError::LeaseLost) => Err(PushFailure::new(
                LeasePushFailureKind::LeaseLost,
                "the lease was lost",
            )),
            Err(WorkerEngineError::LinkRevoked) => {
                cancel.cancel();
                Err(PushFailure::new(
                    LeasePushFailureKind::LinkRevoked,
                    "the work server link was revoked",
                ))
            }
            // Failed refresh is never permission to keep using possibly
            // invalid credentials. No Git request follows this failure.
            Err(e) => Err(PushFailure::new(LeasePushFailureKind::Auth, e.to_string())),
        }
    }

    fn expiry(&self) -> Option<watch::Receiver<DateTime<Utc>>> {
        self.expiry.clone()
    }

    fn still_valid(&self) -> bool {
        self.expiry
            .as_ref()
            .is_none_or(|expiry| Utc::now() < *expiry.borrow())
    }
}
