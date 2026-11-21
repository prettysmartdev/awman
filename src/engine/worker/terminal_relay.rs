//! Relay a lease's terminals over one `awman.terminal.v1` WebSocket.
//!
//! Every hub terminal owned by the lease's job is subscribed (including ones
//! opened later) and its 0141 frames are sent tagged with `terminal_id`. A
//! lagged viewer gets the hub's `lagged` frame followed by a fresh snapshot.
//! Inbound `input`, `resize` and `take_control` frames are applied only when
//! `allow_input` holds, which the caller computes as the lease's
//! `remote_input` grant AND `daemon.terminal.remoteInput`; otherwise they are
//! dropped and the server is answered one `error{code: "input_refused"}` per
//! terminal.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame as WireFrame};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::{stream::FuturesUnordered, StreamExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use crate::data::ids::TerminalId;
use crate::data::terminal::{TerminalFrame, TerminalOwner};
use crate::engine::terminal_hub::{
    InputGrant, TerminalHub, TerminalHubError, TerminalHubEvent, TerminalSubscription,
};

/// Error code answered to refused remote input.
pub const INPUT_REFUSED: &str = "input_refused";

/// The lease terminal connection (the WSP socket, or a test double).
#[async_trait]
pub trait TerminalChannel: Send {
    async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError>;
    /// `None` when the server closed the connection.
    async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>>;
}

/// How often an idle lease terminal socket is pinged.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// A socket whose peer has sent nothing (not even a pong) for this long, or
/// whose send cannot complete for this long, is treated as dropped, so a
/// silent network loss reconnects instead of waiting for TCP to give up.
const PEER_TIMEOUT: Duration = Duration::from_secs(45);
/// Bound on one terminal handshake; expiry is an ordinary transient failure.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

fn peer_lost() -> WorkerEngineError {
    WorkerEngineError::Invalid("lease terminal peer stopped responding".into())
}

#[async_trait]
impl TerminalChannel for awman_wsp::terminal::TerminalSocket {
    async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
        match tokio::time::timeout(PEER_TIMEOUT, futures_util::SinkExt::send(self, frame)).await {
            Ok(result) => result.map_err(Into::into),
            Err(_) => Err(peer_lost()),
        }
    }
    async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
        loop {
            // The schedule lives on the socket, so a recv future that a select
            // loop drops and recreates neither floods pings nor skips them.
            let lost_at = self.last_seen() + PEER_TIMEOUT;
            let ping_at = self
                .last_seen()
                .max(self.last_ping().unwrap_or(self.last_seen()))
                + KEEPALIVE_INTERVAL;
            tokio::select! {
                frame = futures_util::StreamExt::next(self) => {
                    return frame.map(|r| r.map_err(Into::into));
                }
                _ = tokio::time::sleep_until(ping_at.min(lost_at)) => {
                    if tokio::time::Instant::now() >= lost_at {
                        return Some(Err(peer_lost()));
                    }
                    match tokio::time::timeout(PEER_TIMEOUT, self.ping()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return Some(Err(error.into())),
                        Err(_) => return Some(Err(peer_lost())),
                    }
                }
            }
        }
    }
}

/// Opens a fresh socket at the same claim-time lease epoch. Implementations must
/// retain the client's existing refresh-once behavior for HTTP 401.
#[async_trait]
pub trait TerminalChannelOpener: Send {
    async fn open(&mut self) -> Result<Box<dyn TerminalChannel>, WorkerEngineError>;

    /// Link-wide revocation notification, also observed during backoff.
    fn revoked(&self) -> Option<CancellationToken> {
        None
    }
}

/// Production opener shared by daemon and foreground lease holders.
pub struct LeaseTerminalOpener {
    session: Arc<super::session::WspSession>,
    lease: super::session::LeaseRef,
}

impl LeaseTerminalOpener {
    pub fn new(session: Arc<super::session::WspSession>, lease: super::session::LeaseRef) -> Self {
        Self { session, lease }
    }
}

#[async_trait]
impl TerminalChannelOpener for LeaseTerminalOpener {
    async fn open(&mut self) -> Result<Box<dyn TerminalChannel>, WorkerEngineError> {
        Ok(Box::new(self.session.open_terminal(&self.lease).await?))
    }

    fn revoked(&self) -> Option<CancellationToken> {
        Some(self.session.revoked())
    }
}

/// A terminal transport ending never means that lease execution failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalRelayOutcome {
    /// The caller's done token fired (including owner cancellation or expiry).
    Completed,
    /// HTTP 404: stop offering terminals for this lease, but keep executing it.
    TerminalsUnavailable,
    /// Fence the run, cancel its containers and discard its result.
    LeaseLost,
    /// Fence the run and stop claiming from the revoked server link.
    LinkRevoked,
}

fn terminal_outcome(error: &WorkerEngineError) -> Option<TerminalRelayOutcome> {
    match error {
        WorkerEngineError::LeaseLost => Some(TerminalRelayOutcome::LeaseLost),
        WorkerEngineError::LinkRevoked => Some(TerminalRelayOutcome::LinkRevoked),
        _ => None,
    }
}

struct ReconnectBackoff(Duration);

impl ReconnectBackoff {
    const CAP: Duration = Duration::from_secs(10);

    fn new() -> Self {
        Self(Duration::from_millis(500))
    }

    fn next(&mut self) -> Duration {
        let delay = self.0;
        self.0 = (delay * 2).min(Self::CAP);
        delay
    }
}

// Subscriptions, rather than output frames, survive a disconnect. Each has only
// the hub's bounded broadcast ring and snapshot/exit recovery metadata.
struct RelayState {
    events: broadcast::Receiver<TerminalHubEvent>,
    subscriptions: HashMap<TerminalId, TerminalSubscription>,
    // Only exits whose send failed survive here, at most one per subscription.
    pending_exits: HashMap<TerminalId, i32>,
}

impl RelayState {
    fn new(hub: &TerminalHub) -> Self {
        Self {
            events: hub.events(),
            subscriptions: HashMap::new(),
            pending_exits: HashMap::new(),
        }
    }
}

/// One connection's frame exchange. Implementations send current snapshots
/// before output, recover disconnected exits, and keep input grants local to
/// this call. Foreground exchange adapters can request that resynchronization
/// over their existing owner-local channel without implementing retry policy.
#[async_trait]
pub trait TerminalConnectionRelay: Send {
    async fn relay_connection(
        &mut self,
        channel: &mut dyn TerminalChannel,
    ) -> Result<(), WorkerEngineError>;
}

struct HubConnectionRelay<'a> {
    relay: &'a TerminalRelay,
    state: RelayState,
}

#[async_trait]
impl TerminalConnectionRelay for HubConnectionRelay<'_> {
    async fn relay_connection(
        &mut self,
        channel: &mut dyn TerminalChannel,
    ) -> Result<(), WorkerEngineError> {
        self.relay.connected(channel, &mut self.state).await
    }
}

/// Shared transport retry driver for hub relays and owner-local exchange
/// adapters. Lease sockets must not duplicate this policy in their callers.
pub async fn reconnect_terminal(
    opener: &mut dyn TerminalChannelOpener,
    done: CancellationToken,
    connection: &mut dyn TerminalConnectionRelay,
) -> TerminalRelayOutcome {
    let revoked = opener.revoked().unwrap_or_default();
    tokio::select! {
        biased;
        _ = done.cancelled() => TerminalRelayOutcome::Completed,
        _ = revoked.cancelled() => TerminalRelayOutcome::LinkRevoked,
        outcome = retry_connections(opener, &done, connection) => outcome,
    }
}

async fn retry_connections(
    opener: &mut dyn TerminalChannelOpener,
    done: &CancellationToken,
    connection: &mut dyn TerminalConnectionRelay,
) -> TerminalRelayOutcome {
    let mut backoff = ReconnectBackoff::new();
    loop {
        // The ready timer and cancellation may arrive in the same poll.
        if done.is_cancelled() {
            return TerminalRelayOutcome::Completed;
        }
        // A server that accepts TCP but never answers the upgrade is a
        // transient failure, not a hang.
        let opened = tokio::time::timeout(OPEN_TIMEOUT, opener.open())
            .await
            .unwrap_or_else(|_| {
                Err(WorkerEngineError::Invalid(
                    "lease terminal handshake timed out".into(),
                ))
            });
        match opened {
            Ok(mut channel) => {
                if done.is_cancelled() {
                    return TerminalRelayOutcome::Completed;
                }
                let connected_at = tokio::time::Instant::now();
                // All remote-input grants belong to connected: any socket
                // failure drops them before the reconnect delay.
                let result = connection.relay_connection(channel.as_mut()).await;
                // A connection that stayed up longer than the largest delay
                // proved itself: the next drop starts the schedule again. A
                // server that accepts and closes at once keeps backing off.
                if connected_at.elapsed() >= ReconnectBackoff::CAP {
                    backoff = ReconnectBackoff::new();
                }
                if let Err(error) = result {
                    if let Some(outcome) = terminal_outcome(&error) {
                        return outcome;
                    }
                }
            }
            Err(error) => {
                if let Some(outcome) = terminal_outcome(&error) {
                    return outcome;
                }
                if matches!(error, WorkerEngineError::Server { status: 404, .. }) {
                    return TerminalRelayOutcome::TerminalsUnavailable;
                }
            }
        }
        tokio::time::sleep(backoff.next()).await;
    }
}

/// Map a hub frame to its wire form.
pub fn wire_frame(frame: TerminalFrame) -> Option<WireFrame> {
    Some(match frame {
        TerminalFrame::Error { code, error } => WireFrame::Error { code, error },
        TerminalFrame::Snapshot {
            has_output,
            cols,
            rows,
            screen,
        } => WireFrame::Snapshot {
            has_output,
            cols,
            rows,
            screen: STANDARD.encode(screen),
        },
        TerminalFrame::Output { bytes } => WireFrame::Output {
            bytes: STANDARD.encode(bytes),
        },
        TerminalFrame::Resize { cols, rows } => WireFrame::Resize { cols, rows },
        TerminalFrame::Exit { code } => WireFrame::Exit { code: Some(code) },
        TerminalFrame::Lagged => WireFrame::Lagged,
        TerminalFrame::AutomationState { .. }
        | TerminalFrame::TurnAccepted { .. }
        | TerminalFrame::TurnResult { .. }
        | TerminalFrame::ExecutionClosed { .. }
        | TerminalFrame::ExecutionReplaced { .. }
        | TerminalFrame::CredentialUpdateState { .. }
        | TerminalFrame::OperationError { .. } => return None,
    })
}

pub struct TerminalRelay {
    hub: Arc<TerminalHub>,
    owner: TerminalOwner,
    allow_input: bool,
    input_policy: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

fn error_frame(terminal_id: &str, code: &str, error: &str) -> LeaseTerminalFrame {
    LeaseTerminalFrame {
        terminal_id: terminal_id.to_owned(),
        frame: WireFrame::Error {
            code: code.into(),
            error: error.into(),
        },
    }
}

impl TerminalRelay {
    pub fn new(hub: Arc<TerminalHub>, owner: TerminalOwner, allow_input: bool) -> Self {
        Self {
            hub,
            owner,
            allow_input,
            input_policy: None,
        }
    }

    /// Live owner policy, evaluated for each incoming input frame.
    pub fn with_input_policy(mut self, policy: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.input_policy = Some(policy);
        self
    }
    /// Relay one already-open channel. Kept for local exchange adapters; lease
    /// sockets should use `run_reconnecting` so EOF cannot cancel execution.
    pub async fn run(
        self,
        channel: &mut dyn TerminalChannel,
        stop: CancellationToken,
    ) -> Result<(), WorkerEngineError> {
        let mut state = RelayState::new(&self.hub);
        tokio::select! {
            biased;
            _ = stop.cancelled() => Ok(()),
            result = self.connected(channel, &mut state) => result,
        }
    }

    /// Own the terminal transport until completion, fencing, revocation or 404.
    /// `done` must also be cancelled by the lease holder on expiry or owner
    /// cancellation. Opening and blocked sends are cancellable. No opener is
    /// polled once `done` is cancelled, including after a backoff timer fires.
    pub async fn run_reconnecting(
        self,
        opener: &mut dyn TerminalChannelOpener,
        done: CancellationToken,
    ) -> TerminalRelayOutcome {
        let mut connection = HubConnectionRelay {
            relay: &self,
            state: RelayState::new(&self.hub),
        };
        reconnect_terminal(opener, done, &mut connection).await
    }

    /// Relay over an owner-local channel whose remote socket generation is
    /// signalled separately. Resubscription and input-grant lifetime are identical
    /// to a direct socket reconnect; retry policy stays with the socket owner.
    pub async fn run_resynchronizing(
        self,
        channel: &mut dyn TerminalChannel,
        mut generation: tokio::sync::watch::Receiver<Option<u64>>,
        stop: CancellationToken,
    ) -> Result<(), WorkerEngineError> {
        let mut state = RelayState::new(&self.hub);
        loop {
            let connected = generation.borrow_and_update().is_some();
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                changed = generation.changed() => if changed.is_err() { return Ok(()); },
                result = self.connected(channel, &mut state), if connected => {
                    if result.is_ok() {
                        return result;
                    }
                    // An unacknowledged send is a transport reset, not the end of
                    // the relay: wait for the next generation (or briefly, for a
                    // reset that kept its generation) and resynchronize.
                    tokio::select! {
                        biased;
                        _ = stop.cancelled() => return Ok(()),
                        changed = generation.changed() => if changed.is_err() { return Ok(()); },
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
            }
        }
    }

    async fn connected(
        &self,
        channel: &mut dyn TerminalChannel,
        state: &mut RelayState,
    ) -> Result<(), WorkerEngineError> {
        let RelayState {
            events,
            subscriptions,
            pending_exits,
        } = state;
        // Capture current mirrors and receivers atomically, replacing stale
        // output with snapshots. Old subscriptions for evicted finished records
        // retain the hub's final-screen/exit recovery messages.
        for record in self.hub.list(Some(&self.owner)) {
            if let Ok(subscription) = self.hub.subscribe(&record.id) {
                subscriptions.insert(record.id, subscription);
            }
        }
        // Keep the same bounded hub-event receiver during backoff. It can
        // recover exits for terminals opened and forgotten while disconnected.
        let mut owned: HashSet<_> = subscriptions.keys().cloned().collect();
        loop {
            match events.try_recv() {
                Ok(TerminalHubEvent::Opened(record)) if record.owner_id == self.owner => {
                    owned.insert(record.id.clone());
                    if let Ok(subscription) = self.hub.subscribe(&record.id) {
                        subscriptions.entry(record.id).or_insert(subscription);
                    }
                }
                Ok(TerminalHubEvent::Closed { id, exit_code, .. }) if owned.contains(&id) => {
                    pending_exits.insert(id, exit_code);
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        for (id, subscription) in subscriptions.iter_mut() {
            if self.hub.get(id).is_ok() {
                if let Some(frame) = subscription.next().await.and_then(wire_frame) {
                    channel
                        .send(LeaseTerminalFrame {
                            terminal_id: id.to_string(),
                            frame,
                        })
                        .await?;
                }
            } else {
                // Only finished terminals can be forgotten. Drain their bounded
                // ring, discarding obsolete output, and preserve the final exit.
                while let Some(frame) = subscription.next().await {
                    match frame {
                        TerminalFrame::Snapshot { .. } => {
                            channel
                                .send(LeaseTerminalFrame {
                                    terminal_id: id.to_string(),
                                    frame: wire_frame(frame).expect("snapshot has wire form"),
                                })
                                .await?;
                        }
                        TerminalFrame::Exit { code } => {
                            pending_exits.insert(id.clone(), code);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        // All current snapshots precede every output and recovered exit.
        for id in pending_exits.keys().cloned().collect::<Vec<_>>() {
            channel
                .send(LeaseTerminalFrame {
                    terminal_id: id.to_string(),
                    frame: WireFrame::Exit {
                        code: Some(pending_exits[&id]),
                    },
                })
                .await?;
            pending_exits.remove(&id);
            subscriptions.remove(&id);
        }
        let mut grants: HashMap<TerminalId, InputGrant> = HashMap::new();
        let mut refused: HashSet<String> = HashSet::new();
        loop {
            let next = async {
                let mut pending: FuturesUnordered<_> = subscriptions
                    .iter_mut()
                    .map(|(id, sub)| async { (id.clone(), sub.next().await) })
                    .collect();
                pending.next().await
            };
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(TerminalHubEvent::Opened(record)) if record.owner_id == self.owner => {
                        if let Ok(subscription) = self.hub.subscribe(&record.id) {
                            subscriptions.entry(record.id).or_insert(subscription);
                        }
                    }
                    Ok(TerminalHubEvent::Forgotten(id)) => { grants.remove(&id); }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for record in self.hub.list(Some(&self.owner)) {
                            if !subscriptions.contains_key(&record.id) {
                                if let Ok(subscription) = self.hub.subscribe(&record.id) {
                                    subscriptions.insert(record.id, subscription);
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                // Inbound frames are few and small; reading them ahead of
                // output keeps input responsive under sustained output.
                incoming = channel.recv() => match incoming {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(frame)) => {
                        let subscribed = subscriptions.keys().cloned().collect();
                        if let Some(reply) = self.inbound(frame, &subscribed, &mut grants, &mut refused) {
                            channel.send(reply).await?;
                        }
                    }
                },
                Some((id, frame)) = next => {
                    match frame {
                        Some(frame) => {
                            let exited = if let TerminalFrame::Exit { code } = &frame {
                                pending_exits.insert(id.clone(), *code);
                                true
                            } else { false };
                            if let Some(frame) = wire_frame(frame) {
                                channel.send(LeaseTerminalFrame { terminal_id: id.to_string(), frame }).await?;
                            }
                            if exited {
                                pending_exits.remove(&id);
                                subscriptions.remove(&id);
                                grants.remove(&id);
                            }
                        },
                        None => { subscriptions.remove(&id); }
                    }
                },
            }
        }
    }

    /// Apply one server→worker frame; the optional reply goes back at once.
    fn inbound(
        &self,
        frame: LeaseTerminalFrame,
        subscribed: &HashSet<TerminalId>,
        grants: &mut HashMap<TerminalId, InputGrant>,
        refused: &mut HashSet<String>,
    ) -> Option<LeaseTerminalFrame> {
        let tid = frame.terminal_id.clone();
        let id = TerminalId::parse(&tid)
            .ok()
            .filter(|id| subscribed.contains(id));
        let wants_input = matches!(
            frame.frame,
            WireFrame::Input { .. } | WireFrame::Resize { .. } | WireFrame::TakeControl
        );
        if !wants_input {
            if let (WireFrame::Detached { .. }, Some(id)) = (&frame.frame, &id) {
                grants.remove(id);
            }
            if !matches!(frame.frame, WireFrame::Detached { .. }) {
                return Some(error_frame(
                    &tid,
                    "invalid_frame",
                    "server-only terminal frame",
                ));
            }
            return None;
        }
        let Some(id) = id else {
            return Some(error_frame(
                &tid,
                "not_found",
                "no such terminal on this lease",
            ));
        };
        if !self.allow_input || self.input_policy.as_ref().is_some_and(|policy| !policy()) {
            return refused.insert(tid.clone()).then(|| {
                error_frame(
                    &tid,
                    INPUT_REFUSED,
                    "remote input is not allowed for this lease",
                )
            });
        }
        // WI 0070: a controller-owned automation terminal accepts raw input
        // only through the owner-local admission gate, never a relay.
        if self
            .hub
            .get(&id)
            .is_ok_and(|record| record.automation.is_some())
        {
            return Some(error_frame(
                &tid,
                INPUT_REFUSED,
                "remote input is not available for an automation terminal",
            ));
        }
        // A sealed or rebound binding revokes grants: drop a stale one and
        // re-acquire instead of failing every later keystroke.
        if grants.get(&id).is_some_and(|grant| !grant.is_valid()) {
            grants.remove(&id);
        }
        if !grants.contains_key(&id) {
            match self.hub.take_control(&id) {
                Ok(grant) => {
                    grants.insert(id.clone(), grant);
                }
                Err(TerminalHubError::ControlHeld) => {
                    return Some(error_frame(
                        &tid,
                        "control_held",
                        "another viewer holds terminal control",
                    ));
                }
                Err(e) => return Some(error_frame(&tid, "terminal_closed", &e.to_string())),
            }
        }
        let grant = grants.get(&id).expect("grant just ensured");
        let result = match frame.frame {
            WireFrame::Input { bytes } => match STANDARD.decode(bytes) {
                Ok(bytes) => grant.send_input(bytes),
                Err(_) => {
                    return Some(error_frame(
                        &tid,
                        "invalid_frame",
                        "input bytes are not base64",
                    ));
                }
            },
            WireFrame::Resize { cols, rows } => grant.resize(cols, rows),
            _ => Ok(()),
        };
        result
            .err()
            .map(|e| error_frame(&tid, "terminal_input_failed", &e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{daemon_state::FeatureName, ids::LeaseId, terminal::TerminalOwnerInfo};
    use crate::engine::terminal_hub::{
        input_channel, TerminalHandle, TerminalHubConfig, TerminalInput, TerminalRegistration,
    };
    use awman_test_support::DeadlineBudget;
    use std::{
        collections::VecDeque,
        future::Future,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::{mpsc, oneshot};

    async fn within<F: Future>(future: F) -> F::Output {
        let budget = DeadlineBudget::new(Duration::from_secs(1));
        tokio::time::timeout(budget.remaining(), future)
            .await
            .expect("relay deadline")
    }

    struct Channel {
        input: mpsc::Receiver<Result<LeaseTerminalFrame, WorkerEngineError>>,
        output: mpsc::Sender<LeaseTerminalFrame>,
        dropped: Option<oneshot::Sender<()>>,
        fail_exit: bool,
    }

    impl Drop for Channel {
        fn drop(&mut self) {
            let _ = self.dropped.take().unwrap().send(());
        }
    }

    #[async_trait]
    impl TerminalChannel for Channel {
        async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
            if self.fail_exit && matches!(frame.frame, WireFrame::Exit { .. }) {
                return Err(WorkerEngineError::Transport("exit send failed".into()));
            }
            self.output
                .send(frame)
                .await
                .map_err(|_| WorkerEngineError::Transport("closed".into()))
        }
        async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
            self.input.recv().await
        }
    }

    struct Peer {
        input: mpsc::Sender<Result<LeaseTerminalFrame, WorkerEngineError>>,
        output: mpsc::Receiver<LeaseTerminalFrame>,
        dropped: oneshot::Receiver<()>,
    }

    fn channel() -> (Box<dyn TerminalChannel>, Peer) {
        channel_failing_exit(false)
    }

    fn channel_failing_exit(fail_exit: bool) -> (Box<dyn TerminalChannel>, Peer) {
        let (input, receive) = mpsc::channel(256);
        let (output, frames) = mpsc::channel(256);
        let (dropped, closed) = oneshot::channel();
        (
            Box::new(Channel {
                input: receive,
                output,
                dropped: Some(dropped),
                fail_exit,
            }),
            Peer {
                input,
                output: frames,
                dropped: closed,
            },
        )
    }

    struct Opener {
        results: VecDeque<Result<Box<dyn TerminalChannel>, WorkerEngineError>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TerminalChannelOpener for Opener {
        async fn open(&mut self) -> Result<Box<dyn TerminalChannel>, WorkerEngineError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.results.pop_front().expect("unexpected reconnect")
        }
    }

    fn opener(results: Vec<Result<Box<dyn TerminalChannel>, WorkerEngineError>>) -> Opener {
        Opener {
            results: results.into(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn fixture() -> (Arc<TerminalHub>, TerminalOwner) {
        (
            Arc::new(TerminalHub::new(TerminalHubConfig {
                subscriber_capacity: 2,
                ..Default::default()
            })),
            TerminalOwner::Lease(LeaseId::new()),
        )
    }

    fn register(
        hub: &Arc<TerminalHub>,
        owner: &TerminalOwner,
    ) -> (TerminalHandle, mpsc::Receiver<Vec<u8>>) {
        let (stdin, input) = input_channel();
        let handle = hub.register(
            TerminalRegistration {
                automation: None,
                owner: TerminalOwnerInfo {
                    owner: owner.clone(),
                    feature: FeatureName::Worker,
                    source: None,
                    attempt: 1,
                    allow_remote_input: true,
                },
                agent: "fake".into(),
                model: None,
                cols: 80,
                rows: 24,
            },
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        (handle, input)
    }

    #[tokio::test(start_paused = true)]
    async fn transient_eof_reconnects_with_current_snapshots_before_output_and_releases_control() {
        let (hub, owner) = fixture();
        let (terminal, mut input) = register(&hub, &owner);
        let (another, _another_input) = register(&hub, &owner);
        let (first, mut peer) = channel();
        let (second, mut replacement) = channel();
        let mut opener = opener(vec![Ok(first), Ok(second)]);
        let done = CancellationToken::new();
        let stop = done.clone();
        let relay_hub = hub.clone();
        let task = tokio::spawn(async move {
            TerminalRelay::new(relay_hub, owner, true)
                .run_reconnecting(&mut opener, stop)
                .await
        });
        for _ in 0..2 {
            assert!(matches!(
                within(peer.output.recv()).await.unwrap().frame,
                WireFrame::Snapshot { .. }
            ));
        }
        peer.input
            .send(Ok(LeaseTerminalFrame {
                terminal_id: terminal.id().to_string(),
                frame: WireFrame::Input {
                    bytes: STANDARD.encode(b"input"),
                },
            }))
            .await
            .unwrap();
        assert_eq!(within(input.recv()).await.unwrap(), b"input");
        assert!(matches!(
            hub.take_control(terminal.id()),
            Err(TerminalHubError::ControlHeld)
        ));
        drop(peer.input);
        within(peer.dropped).await.unwrap();
        // Disconnection has dropped the remote holder, before opening again.
        drop(hub.take_control(terminal.id()).unwrap());
        for _ in 0..20 {
            terminal.push_output(b"offline");
        }
        tokio::time::advance(Duration::from_millis(500)).await;
        let snapshots = [
            within(replacement.output.recv()).await.unwrap(),
            within(replacement.output.recv()).await.unwrap(),
        ];
        assert!(snapshots
            .iter()
            .all(|f| matches!(f.frame, WireFrame::Snapshot { .. })));
        let current = snapshots
            .iter()
            .find(|f| f.terminal_id == terminal.id().as_str())
            .unwrap();
        let WireFrame::Snapshot { screen, .. } = &current.frame else {
            panic!("snapshot")
        };
        assert!(String::from_utf8(STANDARD.decode(screen).unwrap())
            .unwrap()
            .contains("offline"));
        terminal.push_output(b"live");
        let frame = within(replacement.output.recv()).await.unwrap();
        assert_eq!(
            frame.frame,
            WireFrame::Output {
                bytes: STANDARD.encode(b"live")
            }
        );
        assert_ne!(terminal.id(), another.id());
        done.cancel();
        assert_eq!(within(task).await.unwrap(), TerminalRelayOutcome::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_ending_and_forgotten_during_backoff_still_gets_exit_after_snapshots() {
        let (hub, owner) = fixture();
        let (terminal, _input) = register(&hub, &owner);
        let (_live, _live_input) = register(&hub, &owner);
        let (first, mut peer) = channel();
        let (second, mut replacement) = channel();
        let mut opener = opener(vec![Ok(first), Ok(second)]);
        let done = CancellationToken::new();
        let stop = done.clone();
        let relay_hub = hub.clone();
        let task = tokio::spawn(async move {
            TerminalRelay::new(relay_hub, owner, false)
                .run_reconnecting(&mut opener, stop)
                .await
        });
        for _ in 0..2 {
            assert!(matches!(
                within(peer.output.recv()).await.unwrap().frame,
                WireFrame::Snapshot { .. }
            ));
        }
        drop(peer.input);
        within(peer.dropped).await.unwrap();
        for _ in 0..20 {
            terminal.push_output(b"finished offline");
        }
        terminal.exit(7);
        assert!(hub.forget(terminal.id()));
        tokio::time::advance(Duration::from_millis(500)).await;
        for _ in 0..2 {
            assert!(matches!(
                within(replacement.output.recv()).await.unwrap().frame,
                WireFrame::Snapshot { .. }
            ));
        }
        let exit = within(replacement.output.recv()).await.unwrap();
        assert_eq!(exit.terminal_id, terminal.id().as_str());
        assert_eq!(exit.frame, WireFrame::Exit { code: Some(7) });
        done.cancel();
        assert_eq!(within(task).await.unwrap(), TerminalRelayOutcome::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_receive_error_recovers_an_exit_for_a_terminal_created_during_backoff() {
        let (hub, owner) = fixture();
        let (first, peer) = channel();
        let (second, mut replacement) = channel();
        let mut opener = opener(vec![Ok(first), Ok(second)]);
        let done = CancellationToken::new();
        let stop = done.clone();
        let relay_hub = hub.clone();
        let relay_owner = owner.clone();
        let task = tokio::spawn(async move {
            TerminalRelay::new(relay_hub, relay_owner, false)
                .run_reconnecting(&mut opener, stop)
                .await
        });
        peer.input
            .send(Err(WorkerEngineError::Transport("dropped".into())))
            .await
            .unwrap();
        within(peer.dropped).await.unwrap();
        let (ended, _input) = register(&hub, &owner);
        ended.push_output(b"offline");
        ended.exit(4);
        assert!(hub.forget(ended.id()));
        let (retained, _retained_input) = register(&hub, &owner);
        retained.exit(5);
        tokio::time::advance(Duration::from_millis(500)).await;
        let snapshot = within(replacement.output.recv()).await.unwrap();
        assert_eq!(snapshot.terminal_id, retained.id().as_str());
        assert!(matches!(snapshot.frame, WireFrame::Snapshot { .. }));
        let exits = [
            within(replacement.output.recv()).await.unwrap(),
            within(replacement.output.recv()).await.unwrap(),
        ];
        assert!(exits.contains(&LeaseTerminalFrame {
            terminal_id: ended.id().to_string(),
            frame: WireFrame::Exit { code: Some(4) }
        }));
        assert!(exits.contains(&LeaseTerminalFrame {
            terminal_id: retained.id().to_string(),
            frame: WireFrame::Exit { code: Some(5) }
        }));
        done.cancel();
        assert_eq!(within(task).await.unwrap(), TerminalRelayOutcome::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_exit_send_survives_another_disconnect_after_the_record_is_forgotten() {
        let (hub, owner) = fixture();
        let (terminal, _input) = register(&hub, &owner);
        let (first, mut peer) = channel();
        let (second, mut failed_exit) = channel_failing_exit(true);
        let (third, mut replacement) = channel();
        let mut opener = opener(vec![Ok(first), Ok(second), Ok(third)]);
        let done = CancellationToken::new();
        let stop = done.clone();
        let relay_hub = hub.clone();
        let task = tokio::spawn(async move {
            TerminalRelay::new(relay_hub, owner, false)
                .run_reconnecting(&mut opener, stop)
                .await
        });
        assert!(matches!(
            within(peer.output.recv()).await.unwrap().frame,
            WireFrame::Snapshot { .. }
        ));
        drop(peer.input);
        within(peer.dropped).await.unwrap();
        for _ in 0..20 {
            terminal.push_output(b"offline");
        }
        terminal.exit(9);
        assert!(hub.forget(terminal.id()));
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(matches!(
            within(failed_exit.output.recv()).await.unwrap().frame,
            WireFrame::Snapshot { .. }
        ));
        within(failed_exit.dropped).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        let exit = within(replacement.output.recv()).await.unwrap();
        assert_eq!(
            exit,
            LeaseTerminalFrame {
                terminal_id: terminal.id().to_string(),
                frame: WireFrame::Exit { code: Some(9) }
            }
        );
        done.cancel();
        assert_eq!(within(task).await.unwrap(), TerminalRelayOutcome::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn fencing_and_revocation_from_socket_or_open_stop_without_retry() {
        for (error, expected) in [
            (
                WorkerEngineError::LeaseLost,
                TerminalRelayOutcome::LeaseLost,
            ),
            (
                WorkerEngineError::LinkRevoked,
                TerminalRelayOutcome::LinkRevoked,
            ),
        ] {
            let (hub, owner) = fixture();
            let mut failed_open = opener(vec![Err(error.clone())]);
            assert_eq!(
                within(
                    TerminalRelay::new(hub.clone(), owner.clone(), false)
                        .run_reconnecting(&mut failed_open, CancellationToken::new())
                )
                .await,
                expected
            );
            assert_eq!(failed_open.calls.load(Ordering::SeqCst), 1);
            let (socket, peer) = channel();
            peer.input.send(Err(error)).await.unwrap();
            let mut failed_socket = opener(vec![Ok(socket)]);
            assert_eq!(
                within(
                    TerminalRelay::new(hub, owner, false)
                        .run_reconnecting(&mut failed_socket, CancellationToken::new())
                )
                .await,
                expected
            );
            assert_eq!(failed_socket.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_terminals_stop_retrying_without_fencing_execution() {
        let (hub, owner) = fixture();
        let mut opener = opener(vec![Err(WorkerEngineError::Server {
            status: 404,
            code: "not_found".into(),
            message: String::new(),
        })]);
        assert_eq!(
            within(
                TerminalRelay::new(hub, owner, false)
                    .run_reconnecting(&mut opener, CancellationToken::new())
            )
            .await,
            TerminalRelayOutcome::TerminalsUnavailable
        );
        assert_eq!(opener.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn done_during_backoff_or_before_start_opens_no_further_socket() {
        let (hub, owner) = fixture();
        let (socket, peer) = channel();
        drop(peer.input);
        let mut opener = opener(vec![Ok(socket)]);
        let calls = opener.calls.clone();
        let done = CancellationToken::new();
        let stop = done.clone();
        let relay_hub = hub.clone();
        let relay_owner = owner.clone();
        let task = tokio::spawn(async move {
            TerminalRelay::new(relay_hub, relay_owner, false)
                .run_reconnecting(&mut opener, stop)
                .await
        });
        within(peer.dropped).await.unwrap();
        done.cancel();
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(within(task).await.unwrap(), TerminalRelayOutcome::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut never = Opener {
            results: VecDeque::new(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        assert_eq!(
            within(TerminalRelay::new(hub, owner, false).run_reconnecting(&mut never, done)).await,
            TerminalRelayOutcome::Completed
        );
        assert_eq!(never.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_open_and_send_errors_retry_with_capped_backoff() {
        let (hub, owner) = fixture();
        let (_terminal, _input) = register(&hub, &owner);
        let (socket, peer) = channel();
        drop(peer.output); // Force a send failure on the initial snapshot.
        let (second, mut replacement) = channel();
        let mut opener = opener(vec![
            Err(WorkerEngineError::Invalid("malformed handshake".into())),
            Ok(socket),
            Ok(second),
        ]);
        let calls = opener.calls.clone();
        let done = CancellationToken::new();
        let stop = done.clone();
        let task = tokio::spawn(async move {
            TerminalRelay::new(hub, owner, false)
                .run_reconnecting(&mut opener, stop)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(499)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        within(peer.dropped).await.unwrap();
        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(
            within(replacement.output.recv()).await.unwrap().frame,
            WireFrame::Snapshot { .. }
        ));
        done.cancel();
        assert_eq!(within(task).await.unwrap(), TerminalRelayOutcome::Completed);
        let mut backoff = ReconnectBackoff::new();
        assert_eq!(
            (0..9)
                .map(|_| backoff.next().as_millis())
                .collect::<Vec<_>>(),
            [500, 1000, 2000, 4000, 8000, 10000, 10000, 10000, 10000]
        );
    }

    /// Opens recorded on the paused clock; the first open can hang forever.
    struct TimedOpener {
        opens: Arc<std::sync::Mutex<Vec<tokio::time::Instant>>>,
        hang_first: bool,
    }

    #[async_trait]
    impl TerminalChannelOpener for TimedOpener {
        async fn open(&mut self) -> Result<Box<dyn TerminalChannel>, WorkerEngineError> {
            let first = {
                let mut opens = self.opens.lock().unwrap();
                opens.push(tokio::time::Instant::now());
                opens.len() == 1
            };
            if first && self.hang_first {
                std::future::pending::<()>().await;
            }
            Ok(channel().0)
        }
    }

    /// Each connection stays up for its scripted duration, then ends in EOF.
    struct Hold(VecDeque<Duration>);

    #[async_trait]
    impl TerminalConnectionRelay for Hold {
        /// hygiene-allow(H3): runs only under `start_paused`; the sleep advances Tokio's virtual clock to script how long a connection lasts, no wall time passes.
        async fn relay_connection(
            &mut self,
            _channel: &mut dyn TerminalChannel,
        ) -> Result<(), WorkerEngineError> {
            tokio::time::sleep(self.0.pop_front().unwrap_or_default()).await;
            Ok(())
        }
    }

    /// hygiene-allow(H3): runs only under `start_paused`; the poll sleeps on Tokio's virtual clock, which auto-advances, so no wall time passes.
    async fn open_gaps(hang_first: bool, holds: Vec<Duration>, opens: usize) -> Vec<Duration> {
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut opener = TimedOpener {
            opens: recorded.clone(),
            hang_first,
        };
        let done = CancellationToken::new();
        let stop = done.clone();
        let task = tokio::spawn(async move {
            reconnect_terminal(&mut opener, stop, &mut Hold(holds.into())).await
        });
        while recorded.lock().unwrap().len() < opens {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        done.cancel();
        assert_eq!(task.await.unwrap(), TerminalRelayOutcome::Completed);
        let times = recorded.lock().unwrap().clone();
        times.windows(2).map(|w| w[1] - w[0]).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_starts_again_after_a_connection_that_outlived_the_cap() {
        let ms = Duration::from_millis;
        let gaps = open_gaps(
            false,
            vec![ms(0), ms(0), ms(0), Duration::from_secs(20), ms(0)],
            6,
        )
        .await;
        // 500 ms, 1 s, 2 s; the 20 s connection resets the schedule, so its
        // drop waits 500 ms (not 4 s) and the next quick failure 1 s.
        assert_eq!(gaps, [ms(500), ms(1000), ms(2000), ms(20_500), ms(1000)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_handshake_that_never_answers_times_out_and_is_retried() {
        let gaps = open_gaps(true, vec![], 2).await;
        assert_eq!(gaps, [OPEN_TIMEOUT + Duration::from_millis(500)]);
    }
}
