//! Owner-local terminal exchange for foreground lease execution. Credentials and
//! WSP fencing stay with the lease holder; the executing process owns its hub.
use crate::data::worker::wire::{TerminalExchange, TerminalExchangeReply};
use crate::engine::worker::{
    heartbeat::HeartbeatControl,
    terminal_relay::{
        reconnect_terminal, LeaseTerminalOpener, TerminalChannel, TerminalConnectionRelay,
        TerminalRelayOutcome,
    },
    LeaseRef, WorkerEngineError, WspSession,
};
use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// Delay before retrying a failed owner-local exchange (a local hop, not the
/// WSP reconnect policy, which stays in the shared relay).
const EXCHANGE_RETRY: std::time::Duration = std::time::Duration::from_millis(250);
/// Remote frames held by the pump while the relay is not reading.
const INBOUND_LIMIT: usize = 1024;
/// How long one exchange waits for the socket owner before answering with
/// the current status, well inside the owner-local client's own timeout.
const EXCHANGE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

type ExchangeRequest = (TerminalExchange, oneshot::Sender<TerminalExchangeReply>);

pub(super) struct Bridge {
    outgoing: mpsc::Sender<ExchangeRequest>,
    status: watch::Receiver<TerminalExchangeReply>,
    stop: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Every socket has its own input queue, refusal set and generation. Dropping
/// this guard also announces disconnect when cancellation drops a blocked send.
struct ConnectionGuard(watch::Sender<TerminalExchangeReply>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.send_replace(TerminalExchangeReply::default());
    }
}
struct Connection {
    requests: mpsc::Receiver<ExchangeRequest>,
    status: watch::Sender<TerminalExchangeReply>,
    generation: u64,
    granted: bool,
    policy: Arc<dyn Fn() -> bool + Send + Sync>,
}
#[async_trait::async_trait]
impl TerminalConnectionRelay for Connection {
    async fn relay_connection(
        &mut self,
        socket: &mut dyn TerminalChannel,
    ) -> Result<(), WorkerEngineError> {
        self.generation += 1;
        let generation = Some(self.generation);
        let _guard = ConnectionGuard(self.status.clone());
        self.status.send_replace(TerminalExchangeReply {
            generation,
            ..Default::default()
        });
        let mut input = Vec::new();
        let mut refused = std::collections::HashSet::new();
        loop {
            tokio::select! {
                request = self.requests.recv() => {
                    let Some((request, ack)) = request else { return Ok(()); };
                    let accepted = request.generation == generation;
                    if accepted {
                        for frame in request.frames { socket.send(frame).await?; }
                    }
                    let reply = TerminalExchangeReply {
                        generation, accepted, unavailable: false,
                        frames: if accepted { std::mem::take(&mut input) } else { vec![] },
                    };
                    // A caller that gave up does not lose the remote input.
                    if let Err(reply) = ack.send(reply) {
                        input.splice(0..0, reply.frames);
                    }
                },
                frame = socket.recv(), if input.len() < 256 => match frame {
                    Some(Ok(frame)) => {
                        if matches!(frame.frame, TerminalFrame::Input {..} | TerminalFrame::Resize {..} | TerminalFrame::TakeControl)
                            && !(self.granted && (self.policy)()) {
                            if refused.insert(frame.terminal_id.clone()) {
                                socket.send(LeaseTerminalFrame { terminal_id: frame.terminal_id,
                                    frame: TerminalFrame::Error { code: crate::engine::worker::terminal_relay::INPUT_REFUSED.into(), error: "remote input is not allowed for this lease".into() } }).await?;
                            }
                        } else { input.push(frame); }
                    },
                    Some(Err(error)) => return Err(error),
                    None => return Ok(()),
                }
            }
        }
    }
}
impl Bridge {
    pub fn start(
        session: Arc<WspSession>,
        lease: &awman_wsp::Lease,
        control: HeartbeatControl,
        done: CancellationToken,
        policy: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<Self> {
        let (outgoing, requests) = mpsc::channel(256);
        let (status, receiver) = watch::channel(TerminalExchangeReply::default());
        let mut opener = LeaseTerminalOpener::new(session, LeaseRef::of(lease));
        let mut connection = Connection {
            requests,
            status: status.clone(),
            generation: 0,
            granted: lease.remote_input,
            policy,
        };
        let stop = done.clone();
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = done.cancelled() => TerminalRelayOutcome::Completed,
                _ = control.cancelled().cancelled_owned() => TerminalRelayOutcome::Completed,
                result = reconnect_terminal(&mut opener, done.clone(), &mut connection) => result,
            };
            if matches!(
                result,
                TerminalRelayOutcome::LeaseLost | TerminalRelayOutcome::LinkRevoked
            ) {
                control.fence();
            }
            status.send_replace(TerminalExchangeReply {
                unavailable: true,
                ..Default::default()
            });
        });
        Arc::new(Self {
            outgoing,
            status: receiver,
            stop,
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }
    /// Join the socket owner before posting the terminal lease report.
    pub async fn stop(&self) {
        self.stop.cancel();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
    pub async fn exchange(
        &self,
        request: TerminalExchange,
    ) -> Result<TerminalExchangeReply, String> {
        if request.frames.len() > 256 {
            return Err("too many terminal frames".into());
        }
        let mut status = self.status.clone();
        let current = status.borrow_and_update().clone();
        if current.generation.is_none() || current.generation != request.generation {
            return Ok(current);
        }
        let requested_generation = request.generation;
        let (ack, received) = oneshot::channel();
        tokio::select! {
            biased;
            _ = status.changed() => Ok(status.borrow().clone()),
            // A stalled socket answers with its status instead of holding the
            // caller past its timeout; an unacknowledged Exit is resent after resynchronization.
            _ = tokio::time::sleep(EXCHANGE_WAIT) => Ok(TerminalExchangeReply {
                accepted: false,
                frames: vec![],
                ..status.borrow().clone()
            }),
            result = async {
                self.outgoing.send((request, ack)).await.map_err(|_| "terminal disconnected")?;
                received.await.map_err(|_| "terminal disconnected".to_string())
            } => {
                // A failed socket send drops its acknowledgement before the
                // disconnect notification is polled. It is a transport reset.
                match result {
                    Ok(reply) => Ok(reply),
                    Err(_) => {
                        if status.borrow().generation == requested_generation
                            && !status.borrow().unavailable {
                            let _ = status.changed().await;
                        }
                        Ok(status.borrow().clone())
                    }
                }
            }
        }
    }
}

/// Frames are batched to the lease holder once per pump tick. Only an Exit
/// send waits until the lease holder wrote it to WSP: a generation change
/// cancels it unacknowledged, so the shared relay keeps the pending exit.
/// Other frames are not acknowledged; a reset resynchronizes with snapshots.
struct PendingFrame {
    generation: Option<u64>,
    frame: LeaseTerminalFrame,
    ack: Option<oneshot::Sender<()>>,
}
struct Channel {
    outgoing: mpsc::Sender<PendingFrame>,
    incoming: mpsc::Receiver<(Option<u64>, LeaseTerminalFrame)>,
    generation: watch::Receiver<Option<u64>>,
}
#[async_trait::async_trait]
impl TerminalChannel for Channel {
    async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
        let disconnected = || WorkerEngineError::Invalid("terminal disconnected".into());
        let generation = *self.generation.borrow();
        let (ack, received) = if matches!(frame.frame, TerminalFrame::Exit { .. }) {
            let (ack, received) = oneshot::channel();
            (Some(ack), Some(received))
        } else {
            (None, None)
        };
        self.outgoing
            .send(PendingFrame {
                generation,
                frame,
                ack,
            })
            .await
            .map_err(|_| disconnected())?;
        match received {
            Some(received) => received.await.map_err(|_| disconnected()),
            None => Ok(()),
        }
    }
    async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
        while let Some((generation, frame)) = self.incoming.recv().await {
            if generation == *self.generation.borrow() {
                return Some(Ok(frame));
            }
        }
        None
    }
}

pub(super) struct Relay {
    hub: Arc<crate::engine::terminal_hub::TerminalHub>,
    owner: crate::data::terminal::TerminalOwner,
    delivered_exits: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    unavailable: Arc<AtomicBool>,
    generation: watch::Receiver<Option<u64>>,
    stop: CancellationToken,
}
impl Relay {
    pub async fn flush(&self) {
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            // While unavailable or disconnected nothing can be delivered.
            if self.unavailable.load(Ordering::Acquire) || self.generation.borrow().is_none() {
                return;
            }
            let delivered = self.delivered_exits.lock().unwrap().clone();
            if self.hub.list(Some(&self.owner)).iter().all(|terminal| {
                terminal.state == crate::data::terminal::TerminalState::Exited
                    && delivered.contains(terminal.id.as_str())
            }) {
                return;
            }
            if self.stop.is_cancelled() || tokio::time::Instant::now() >= until {
                // Terminal presentation is best effort and cannot change the
                // execution outcome when the network remains unavailable.
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub(super) fn relay<F, Fut>(
    hub: Arc<crate::engine::terminal_hub::TerminalHub>,
    job_id: &str,
    granted: bool,
    stop: CancellationToken,
    exchange: F,
) -> Relay
where
    F: Fn(TerminalExchange) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<TerminalExchangeReply, String>> + Send,
{
    let owner = crate::data::terminal::TerminalOwner::Command(
        crate::data::ids::CommandId::parse(job_id).expect("prepared command id"),
    );
    let (outgoing, mut out) = mpsc::channel(256);
    let (incoming, input) = mpsc::channel(256);
    let (generation, generations) = watch::channel(None);
    let mut channel = Channel {
        outgoing,
        incoming: input,
        generation: generations.clone(),
    };
    let relay_stop = stop.clone();
    let delivered_exits = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let unavailable = Arc::new(AtomicBool::new(false));
    let relay = Relay {
        hub: hub.clone(),
        owner: owner.clone(),
        delivered_exits: delivered_exits.clone(),
        unavailable: unavailable.clone(),
        generation: generations.clone(),
        stop: stop.clone(),
    };
    tokio::spawn(async move {
        let _ = crate::engine::worker::terminal_relay::TerminalRelay::new(hub, owner, granted)
            .run_resynchronizing(&mut channel, generations, relay_stop)
            .await;
    });
    tokio::spawn(async move {
        let run = async {
            let mut inbound = std::collections::VecDeque::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                let mut pending = Vec::new();
                while pending.len() < 256 {
                    match out.try_recv() {
                        Ok(frame) => pending.push(frame),
                        Err(_) => break,
                    }
                }
                let current = *generation.borrow();
                pending.retain(|frame: &PendingFrame| frame.generation == current);
                let reply = match exchange(TerminalExchange {
                    generation: current,
                    frames: pending.iter().map(|frame| frame.frame.clone()).collect(),
                })
                .await
                {
                    Ok(reply) => reply,
                    Err(error) => {
                        // A failed local hop is a transport reset, never the end
                        // of terminals for this lease: drop unacknowledged sends
                        // (pending exits survive in the relay) and try again.
                        tracing::debug!(%error, "owner-local terminal exchange failed");
                        generation.send_replace(None);
                        drop(pending);
                        tokio::time::sleep(EXCHANGE_RETRY).await;
                        continue;
                    }
                };
                if reply.unavailable {
                    unavailable.store(true, Ordering::Release);
                    generation.send_replace(None);
                    return Ok(());
                }
                // Publish the reset before dropping unacknowledged sends.
                // The shared relay cancels its connection and releases grants.
                generation.send_if_modified(|current| {
                    if *current == reply.generation {
                        false
                    } else {
                        *current = reply.generation;
                        true
                    }
                });
                if reply.accepted {
                    for frame in pending {
                        if matches!(frame.frame.frame, TerminalFrame::Exit { .. }) {
                            delivered_exits
                                .lock()
                                .unwrap()
                                .insert(frame.frame.terminal_id);
                        }
                        if let Some(ack) = frame.ack {
                            let _ = ack.send(());
                        }
                    }
                }
                // Never block on the relay: it may itself be waiting for this
                // pump. Overflow waits here, bounded, for the next tick.
                inbound.extend(
                    reply
                        .frames
                        .into_iter()
                        .map(|frame| (reply.generation, frame)),
                );
                while inbound.len() > INBOUND_LIMIT {
                    inbound.pop_front();
                    tracing::warn!("dropping a remote terminal frame: the relay is not reading");
                }
                while let Some(frame) = inbound.pop_front() {
                    match incoming.try_send(frame) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(frame)) => {
                            inbound.push_front(frame);
                            break;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return Err("terminal receiver ended".to_string());
                        }
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        };
        tokio::select! {
            biased;
            _ = stop.cancelled() => {},
            _ = run => { unavailable.store(true, Ordering::Release); stop.cancel(); },
        }
    });
    relay
}

#[cfg(test)]
mod tests;
