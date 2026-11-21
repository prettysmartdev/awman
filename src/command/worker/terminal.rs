//! Owner-local terminal exchange for foreground lease execution. Credentials and
//! WSP fencing stay with the lease holder; the executing process owns its hub.
use crate::engine::worker::{
    heartbeat::HeartbeatControl, terminal_relay::TerminalChannel, LeaseRef, WorkerEngineError,
    WspSession,
};
use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

pub(super) struct Bridge {
    outgoing: mpsc::Sender<(Vec<LeaseTerminalFrame>, tokio::sync::oneshot::Sender<()>)>,
    incoming: Mutex<mpsc::Receiver<LeaseTerminalFrame>>,
}
impl Bridge {
    pub fn start(
        session: Arc<WspSession>,
        lease: &awman_wsp::Lease,
        control: HeartbeatControl,
        done: CancellationToken,
        policy: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<Self> {
        let (outgoing, mut send) =
            mpsc::channel::<(Vec<LeaseTerminalFrame>, tokio::sync::oneshot::Sender<()>)>(256);
        let (receive, incoming) = mpsc::channel(256);
        let reference = LeaseRef::of(lease);
        let granted = lease.remote_input;
        tokio::spawn(async move {
            let fence = control.fenced();
            let run = async {
                let mut socket = session.open_terminal(&reference).await?;
                let mut refused = std::collections::HashSet::new();
                loop {
                    tokio::select! {
                        Some((frames, ack)) = send.recv() => {
                            for frame in frames { socket.send(frame).await?; }
                            let _ = ack.send(());
                        },
                        frame = socket.recv() => match frame {
                            Some(Ok(frame)) => {
                                if matches!(frame.frame, TerminalFrame::Input {..} | TerminalFrame::Resize {..} | TerminalFrame::TakeControl)
                                    && !(granted && policy()) {
                                    if refused.insert(frame.terminal_id.clone()) {
                                        socket.send(LeaseTerminalFrame { terminal_id: frame.terminal_id,
                                            frame: TerminalFrame::Error { code: "input_refused".into(), error: "remote input is not allowed for this lease".into() } }).await?;
                                    }
                                } else if receive.send(frame).await.is_err() { return Ok(()); }
                            }
                            Some(Err(error)) => return Err(error),
                            None => return Err(WorkerEngineError::Invalid("lease terminal disconnected".into())),
                        }
                    }
                }
            };
            let result = tokio::select! {
                biased;
                _ = done.cancelled() => return,
                _ = fence.cancelled() => return,
                result = run => result,
            };
            match result {
                Err(WorkerEngineError::LeaseLost | WorkerEngineError::LinkRevoked) => {
                    control.fence()
                }
                Err(_) => control.cancelled().cancel(),
                Ok(()) => {}
            }
        });
        Arc::new(Self {
            outgoing,
            incoming: Mutex::new(incoming),
        })
    }
    pub async fn exchange(
        &self,
        frames: Vec<LeaseTerminalFrame>,
    ) -> Result<Vec<LeaseTerminalFrame>, String> {
        if frames.len() > 256 {
            return Err("too many terminal frames".into());
        }
        if !frames.is_empty() {
            let (ack, received) = tokio::sync::oneshot::channel();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                self.outgoing
                    .send((frames, ack))
                    .await
                    .map_err(|_| "terminal disconnected")?;
                received.await.map_err(|_| "terminal disconnected")
            })
            .await
            .map_err(|_| "terminal backpressure")??;
        }
        let mut receive = self.incoming.lock().await;
        let mut frames = Vec::new();
        while frames.len() < 256 {
            match receive.try_recv() {
                Ok(frame) => frames.push(frame),
                Err(_) => break,
            }
        }
        Ok(frames)
    }
}

/// In-process channel also used by the owner-local HTTP pump. Neither side
/// can accumulate unbounded terminal bytes behind a disconnected presenter.
pub(super) struct Channel {
    pub outgoing: mpsc::Sender<LeaseTerminalFrame>,
    pub incoming: mpsc::Receiver<LeaseTerminalFrame>,
}
#[async_trait::async_trait]
impl TerminalChannel for Channel {
    async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
        self.outgoing
            .send(frame)
            .await
            .map_err(|_| WorkerEngineError::Invalid("terminal disconnected".into()))
    }
    async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
        self.incoming.recv().await.map(Ok)
    }
}

pub(super) struct Relay {
    hub: Arc<crate::engine::terminal_hub::TerminalHub>,
    owner: crate::data::terminal::TerminalOwner,
    delivered_exits: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    stop: CancellationToken,
}
impl Relay {
    pub async fn flush(&self) -> Result<(), String> {
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let delivered = self.delivered_exits.lock().unwrap().clone();
            if self.hub.list(Some(&self.owner)).iter().all(|terminal| {
                terminal.state == crate::data::terminal::TerminalState::Exited
                    && delivered.contains(terminal.id.as_str())
            }) {
                return Ok(());
            }
            if self.stop.is_cancelled() || tokio::time::Instant::now() >= until {
                return Err("foreground terminal output did not drain".into());
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
    cancel: CancellationToken,
    exchange: F,
) -> Relay
where
    F: Fn(Vec<LeaseTerminalFrame>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Vec<LeaseTerminalFrame>, String>> + Send,
{
    let owner = crate::data::terminal::TerminalOwner::Command(
        crate::data::ids::CommandId::parse(job_id).expect("prepared command id"),
    );
    let (outgoing, mut out) = mpsc::channel(256);
    let (incoming, input) = mpsc::channel(256);
    let mut channel = Channel {
        outgoing,
        incoming: input,
    };
    let relay_stop = stop.clone();
    let relay_cancel = cancel.clone();
    let delivered_exits = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let relay = Relay {
        hub: hub.clone(),
        owner: owner.clone(),
        delivered_exits: delivered_exits.clone(),
        stop: stop.clone(),
    };
    tokio::spawn(async move {
        if crate::engine::worker::terminal_relay::TerminalRelay::new(hub, owner, granted)
            .run(&mut channel, relay_stop)
            .await
            .is_err()
        {
            relay_cancel.cancel();
        }
    });
    tokio::spawn(async move {
        let run = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                let mut frames = Vec::new();
                while frames.len() < 256 {
                    match out.try_recv() {
                        Ok(frame) => frames.push(frame),
                        Err(_) => break,
                    }
                }
                let exits: Vec<_> = frames
                    .iter()
                    .filter(|frame| matches!(frame.frame, TerminalFrame::Exit { .. }))
                    .map(|frame| frame.terminal_id.clone())
                    .collect();
                let input = exchange(frames).await?;
                delivered_exits.lock().unwrap().extend(exits);
                for frame in input {
                    incoming
                        .send(frame)
                        .await
                        .map_err(|_| "terminal receiver ended".to_string())?;
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        };
        tokio::select! {
            biased;
            _ = stop.cancelled() => {},
            result = run => if result.is_err() { cancel.cancel(); stop.cancel(); },
        }
    });
    relay
}
