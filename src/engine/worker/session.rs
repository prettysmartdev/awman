//! `WspSession`: the one place that talks Work Server Protocol v1.
//!
//! Wraps `awman_wsp::Client` (bearer tokens refreshed from the stored
//! refresh token, refresh-once on `401`), maps every error to
//! [`WorkerEngineError`] and latches revocation: once a token refresh is
//! refused, [`WspSession::revoked`] fires so the caller cancels running
//! leases of this server and stops claiming.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use awman_wsp::{
    ClaimOutcome, Credentials, HeartbeatRequest, HeartbeatResponse, LeaseResult, List, QueueItem,
    RegisterRequest, RegisterResponse, ReleaseRequest,
};
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use crate::data::execution_event::ExecutionEvent;
use crate::data::fs::api_db::CommandResult;

type Result<T> = std::result::Result<T, WorkerEngineError>;

/// The work server's lease ID and the epoch awman holds it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRef {
    pub server_lease_id: String,
    pub epoch: u64,
}

impl LeaseRef {
    pub fn of(lease: &awman_wsp::Lease) -> Self {
        Self {
            server_lease_id: lease.id.clone(),
            epoch: lease.epoch,
        }
    }
}

/// The per-lease calls the heartbeat, event and push primitives make.
/// [`WspSession`] implements it; tests substitute fakes.
#[async_trait]
pub trait LeaseApi: Send + Sync {
    async fn heartbeat(
        &self,
        lease: &LeaseRef,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse>;
    /// Post one ordered batch; returns how many events the server newly accepted.
    async fn post_events(&self, lease: &LeaseRef, events: &[ExecutionEvent]) -> Result<usize>;
    async fn credentials(&self, lease: &LeaseRef) -> Result<Credentials>;
}

pub struct WspSession {
    client: awman_wsp::Client,
    server: String,
    revoked_flag: AtomicBool,
    revoked: CancellationToken,
}

impl std::fmt::Debug for WspSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WspSession")
            .field("server", &self.server)
            .field("revoked", &self.is_revoked())
            .finish_non_exhaustive()
    }
}

impl WspSession {
    /// `server` is the `workServers` key; `refresh_token` comes from the
    /// owner-only link store and is held only in memory.
    pub fn connect(server: &str, url: &str, refresh_token: String) -> Result<Self> {
        let client = awman_wsp::Client::new(url, refresh_token)
            .map_err(|e| WorkerEngineError::Invalid(format!("work server '{server}': {e}")))?;
        Ok(Self {
            client,
            server: server.to_owned(),
            revoked_flag: AtomicBool::new(false),
            revoked: CancellationToken::new(),
        })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_flag.load(Ordering::SeqCst)
    }

    /// Fires once the link is revoked.
    pub fn revoked(&self) -> CancellationToken {
        self.revoked.clone()
    }

    fn map<T>(&self, result: std::result::Result<T, awman_wsp::Error>) -> Result<T> {
        result.map_err(|e| {
            let e = WorkerEngineError::from(e);
            if e == WorkerEngineError::LinkRevoked {
                self.revoked_flag.store(true, Ordering::SeqCst);
                self.revoked.cancel();
            }
            e
        })
    }

    /// The client for one lease's calls, pinned to the lease's claim-time
    /// epoch (sent as `x-wsp-epoch`): a run keeps its own epoch even after
    /// the server re-leases the same ID, so the server fences the old run.
    fn lease_client(&self, lease: &LeaseRef) -> awman_wsp::Client {
        self.client
            .with_lease_epoch(&lease.server_lease_id, lease.epoch)
    }

    fn live(&self) -> Result<()> {
        if self.is_revoked() {
            return Err(WorkerEngineError::LinkRevoked);
        }
        Ok(())
    }

    /// `POST /v1/workers/register`, on start and whenever labels change.
    pub async fn register(&self, request: &RegisterRequest) -> Result<RegisterResponse> {
        self.live()?;
        self.map(self.client.register(request).await)
    }

    /// `GET /v1/queue`: items eligible for this worker, in server order.
    pub async fn queue(&self, limit: usize, cursor: Option<&str>) -> Result<List<QueueItem>> {
        self.live()?;
        self.map(self.client.queue(limit, cursor).await)
    }

    /// `POST /v1/claims` (long-poll up to `wait_ms`, capped at 30 s by the
    /// client): a manual pick with `item_id`, else the next eligible item.
    pub async fn claim(&self, item_id: Option<&str>, wait_ms: u64) -> Result<ClaimOutcome> {
        self.live()?;
        self.map(self.client.claim_outcome(item_id, wait_ms).await)
    }

    pub async fn result(
        &self,
        lease: &LeaseRef,
        result: &LeaseResult<CommandResult>,
    ) -> Result<()> {
        self.live()?;
        self.map(
            self.lease_client(lease)
                .result(&lease.server_lease_id, result)
                .await,
        )
    }

    pub async fn release(
        &self,
        lease: &LeaseRef,
        request: &ReleaseRequest<CommandResult>,
    ) -> Result<()> {
        self.live()?;
        self.map(
            self.lease_client(lease)
                .release(&lease.server_lease_id, request)
                .await,
        )
    }

    /// `GET /v1/leases/{id}/terminal` (`awman.terminal.v1`).
    pub async fn open_terminal(
        &self,
        lease: &LeaseRef,
    ) -> Result<awman_wsp::terminal::TerminalSocket> {
        self.live()?;
        self.map(
            self.lease_client(lease)
                .open_terminal(&lease.server_lease_id)
                .await,
        )
    }
}

#[async_trait]
impl LeaseApi for WspSession {
    async fn heartbeat(
        &self,
        lease: &LeaseRef,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse> {
        self.live()?;
        self.map(
            self.lease_client(lease)
                .heartbeat(&lease.server_lease_id, request)
                .await,
        )
    }

    async fn post_events(&self, lease: &LeaseRef, events: &[ExecutionEvent]) -> Result<usize> {
        self.live()?;
        self.map(
            self.lease_client(lease)
                .events_accepted(
                    &lease.server_lease_id,
                    events.iter().map(ExecutionEvent::public_value).collect(),
                )
                .await,
        )
    }

    async fn credentials(&self, lease: &LeaseRef) -> Result<Credentials> {
        self.live()?;
        self.map(
            self.lease_client(lease)
                .credentials(&lease.server_lease_id)
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_revocation_state_model_is_monotonic_under_error_and_success_histories() {
        let client = awman_wsp::Client::new("http://localhost", String::new()).unwrap();
        for seed in 0..256 {
            let session = WspSession {
                client: client.clone(),
                server: "model".into(),
                revoked_flag: AtomicBool::new(false),
                revoked: CancellationToken::new(),
            };
            let mut revoked = false;
            for bit in 0..8 {
                if seed & (1 << bit) != 0 {
                    let _: Result<()> = session.map(Err(awman_wsp::Error::LinkRevoked));
                    revoked = true;
                } else {
                    session.map(Ok(())).unwrap();
                }
                assert_eq!(session.is_revoked(), revoked, "seed {seed}, step {bit}");
                assert_eq!(session.revoked().is_cancelled(), revoked);
                assert_eq!(session.live().is_err(), revoked);
            }
        }
    }
}
