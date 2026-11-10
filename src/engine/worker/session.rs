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
    use crate::data::execution_event::EventPayload;
    use awman_wsp::mock::{MockWorkServer, ServingMock};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    async fn linked(server: &Arc<MockWorkServer>) -> (ServingMock, String) {
        let serving = server.clone().serve().await.unwrap();
        let link = server.link();
        server.approve_link(
            &link.link_id,
            BTreeMap::from([("owner".into(), "github:alice".into())]),
        );
        let client = awman_wsp::Client::new(&serving.base_url, String::new()).unwrap();
        let token = client
            .poll_link(&link.link_id)
            .await
            .unwrap()
            .expect("approved");
        (serving, token.refresh_token)
    }

    fn register_request() -> RegisterRequest {
        RegisterRequest {
            name: "box-daemon".into(),
            labels: BTreeMap::from([("awman/host".into(), "daemon".into())]),
            capacity: 1,
            features: vec![awman_wsp::features::LEASE_CREDENTIALS.into()],
        }
    }

    fn event(sequence: u64) -> ExecutionEvent {
        ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: 1,
            sequence,
            source: None,
            payload: EventPayload::StdoutLine(format!("line {sequence}")),
        }
    }

    fn result() -> LeaseResult<CommandResult> {
        LeaseResult {
            result: CommandResult::default(),
            outputs: None,
            outputs_mounted: false,
            duration_ms: 5,
        }
    }

    #[tokio::test]
    async fn wsp_session_registers_claims_heartbeats_posts_events_and_results() {
        let server = Arc::new(MockWorkServer::new());
        let (serving, refresh) = linked(&server).await;
        let session = WspSession::connect("team", &serving.base_url, refresh).unwrap();
        assert!(!format!("{session:?}").contains("refresh"));
        let registered = session.register(&register_request()).await.unwrap();
        assert!(!registered.worker_id.is_empty());
        assert_eq!(registered.server_labels["owner"], "github:alice");
        assert!(matches!(
            session.claim(None, 0).await.unwrap(),
            ClaimOutcome::Empty
        ));
        server
            .enqueue(
                awman_wsp::conformance::work_item("item-1"),
                chrono::Utc::now(),
            )
            .unwrap();
        let queue = session.queue(50, None).await.unwrap();
        assert_eq!(queue.items.len(), 1);
        let ClaimOutcome::Leased(lease) = session.claim(Some("item-1"), 0).await.unwrap() else {
            panic!("expected a lease");
        };
        let lease = LeaseRef::of(&lease);
        let beat = session
            .heartbeat(
                &lease,
                &HeartbeatRequest {
                    state: awman_wsp::HeartbeatState::Running,
                    step: None,
                    progress: None,
                },
            )
            .await
            .unwrap();
        assert!(!beat.cancel);
        assert_eq!(
            session
                .post_events(&lease, &[event(0), event(1)])
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            session
                .post_events(&lease, &[event(1), event(2)])
                .await
                .unwrap(),
            1,
            "idempotent by (attempt, sequence)"
        );
        assert_eq!(server.events_for(&lease.server_lease_id).len(), 3);
        server.set_credentials(
            "item-1",
            awman_wsp::Credentials {
                kind: awman_wsp::CredentialKind::Bearer,
                token: Some("fresh".into()),
                username: None,
                expires_at: None,
            },
        );
        assert_eq!(
            session.credentials(&lease).await.unwrap().token.as_deref(),
            Some("fresh")
        );
        session.result(&lease, &result()).await.unwrap();
        assert!(server.result_for("item-1").is_some());
        assert_eq!(
            session.result(&lease, &result()).await.unwrap_err(),
            WorkerEngineError::LeaseLost,
            "results are never accepted twice"
        );
    }

    #[tokio::test]
    async fn expired_lease_is_lost_and_revoked_link_latches_revocation() {
        let server = Arc::new(MockWorkServer::new());
        let (serving, refresh) = linked(&server).await;
        let session = WspSession::connect("team", &serving.base_url, refresh.clone()).unwrap();
        session.register(&register_request()).await.unwrap();
        server
            .enqueue(
                awman_wsp::conformance::work_item("item-2"),
                chrono::Utc::now(),
            )
            .unwrap();
        let ClaimOutcome::Leased(lease) = session.claim(None, 0).await.unwrap() else {
            panic!("expected a lease");
        };
        let lease = LeaseRef::of(&lease);
        server.expire_leases();
        let err = session
            .heartbeat(
                &lease,
                &HeartbeatRequest {
                    state: awman_wsp::HeartbeatState::Running,
                    step: None,
                    progress: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err, WorkerEngineError::LeaseLost);
        let revoked = session.revoked();
        server.revoke(&refresh);
        server.invalidate_access_tokens();
        assert_eq!(
            session.queue(10, None).await.unwrap_err(),
            WorkerEngineError::LinkRevoked
        );
        assert!(session.is_revoked() && revoked.is_cancelled());
        assert_eq!(
            session.claim(None, 0).await.unwrap_err(),
            WorkerEngineError::LinkRevoked,
            "no further claims"
        );
    }

    #[tokio::test]
    async fn lease_calls_carry_the_lease_ref_epoch_and_a_stale_epoch_is_fenced() {
        let server = Arc::new(MockWorkServer::new());
        let (serving, refresh) = linked(&server).await;
        let session = WspSession::connect("team", &serving.base_url, refresh.clone()).unwrap();
        session.register(&register_request()).await.unwrap();
        server
            .enqueue(
                awman_wsp::conformance::work_item("item-epoch"),
                chrono::Utc::now(),
            )
            .unwrap();
        let ClaimOutcome::Leased(lease) = session.claim(None, 0).await.unwrap() else {
            panic!("expected a lease");
        };
        let beat = HeartbeatRequest {
            state: awman_wsp::HeartbeatState::Running,
            step: None,
            progress: None,
        };
        // A second session that never claimed this lease (e.g. another
        // handle of the same link) still sends the epoch from the LeaseRef.
        let other = WspSession::connect("team", &serving.base_url, refresh).unwrap();
        other
            .heartbeat(&LeaseRef::of(&lease), &beat)
            .await
            .expect("pinned epoch accepted");
        let stale = LeaseRef {
            server_lease_id: lease.id.clone(),
            epoch: lease.epoch + 1,
        };
        assert_eq!(
            session.heartbeat(&stale, &beat).await.unwrap_err(),
            WorkerEngineError::LeaseLost,
            "the server fences a run holding another epoch"
        );
        session
            .heartbeat(&LeaseRef::of(&lease), &beat)
            .await
            .expect("the real epoch still holds the lease");
    }

    #[tokio::test]
    async fn terminal_relay_streams_over_the_wsp_socket() {
        use crate::data::daemon_state::FeatureName;
        use crate::data::ids::LeaseId;
        use crate::data::terminal::{TerminalOwner, TerminalOwnerInfo};
        use crate::engine::terminal_hub::{
            input_channel, TerminalHub, TerminalHubConfig, TerminalInput, TerminalRegistration,
        };
        use crate::engine::worker::terminal_relay::TerminalRelay;

        let server = Arc::new(MockWorkServer::new());
        let (serving, refresh) = linked(&server).await;
        let session = WspSession::connect("team", &serving.base_url, refresh).unwrap();
        let mut request = register_request();
        request.features.push(awman_wsp::features::TERMINAL.into());
        session.register(&request).await.unwrap();
        server
            .enqueue(
                awman_wsp::conformance::work_item("item-3"),
                chrono::Utc::now(),
            )
            .unwrap();
        server.set_terminal("item-3", true, false);
        let ClaimOutcome::Leased(wire) = session.claim(None, 0).await.unwrap() else {
            panic!("expected a lease");
        };
        assert!(wire.terminal);
        let lease = LeaseRef::of(&wire);
        let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
        let owner = TerminalOwner::Lease(LeaseId::new());
        let (stdin, _rx) = input_channel();
        let handle = hub.register(
            TerminalRegistration {
                owner: TerminalOwnerInfo {
                    owner: owner.clone(),
                    feature: FeatureName::Worker,
                    source: None,
                    attempt: 1,
                    allow_remote_input: false,
                },
                agent: "claude".into(),
                model: None,
                cols: 80,
                rows: 24,
            },
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        let mut socket = session.open_terminal(&lease).await.unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let relay = TerminalRelay::new(hub.clone(), owner, false);
        let task = {
            let stop = stop.clone();
            tokio::spawn(async move { relay.run(&mut socket, stop).await })
        };
        let tid = handle.id().to_string();
        let wait_for = |want: fn(&awman_wsp::terminal::TerminalFrame) -> bool| {
            let (server, lease, tid) = (server.clone(), lease.server_lease_id.clone(), tid.clone());
            async move {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let frames = server.terminal_frames(&lease);
                    if frames
                        .iter()
                        .any(|f| f.terminal_id == tid && want(&f.frame))
                    {
                        return;
                    }
                    assert!(tokio::time::Instant::now() < deadline, "frames: {frames:?}");
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        };
        wait_for(|f| matches!(f, awman_wsp::terminal::TerminalFrame::Snapshot { .. })).await;
        handle.push_output(b"hi");
        wait_for(|f| matches!(f, awman_wsp::terminal::TerminalFrame::Output { .. })).await;
        stop.cancel();
        task.await.unwrap().unwrap();
    }
}
