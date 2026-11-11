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

use async_trait::async_trait;
use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame as WireFrame};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use crate::data::ids::TerminalId;
use crate::data::terminal::{TerminalFrame, TerminalOwner};
use crate::engine::terminal_hub::{InputGrant, TerminalHub, TerminalHubError, TerminalHubEvent};

/// Error code answered to refused remote input.
pub const INPUT_REFUSED: &str = "input_refused";

/// The lease terminal connection (the WSP socket, or a test double).
#[async_trait]
pub trait TerminalChannel: Send {
    async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError>;
    /// `None` when the server closed the connection.
    async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>>;
}

#[async_trait]
impl TerminalChannel for awman_wsp::terminal::TerminalSocket {
    async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
        futures_util::SinkExt::send(self, frame)
            .await
            .map_err(Into::into)
    }
    async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
        futures_util::StreamExt::next(self)
            .await
            .map(|r| r.map_err(Into::into))
    }
}

/// Map a hub frame to its wire form.
pub fn wire_frame(frame: TerminalFrame) -> WireFrame {
    match frame {
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
    }
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
    fn subscribe(
        &self,
        id: TerminalId,
        out: &mpsc::Sender<LeaseTerminalFrame>,
        stop: &CancellationToken,
        subscribed: &mut HashSet<TerminalId>,
    ) {
        if !subscribed.insert(id.clone()) {
            return;
        }
        let Ok(mut subscription) = self.hub.subscribe(&id) else {
            return;
        };
        let (out, stop) = (out.clone(), stop.clone());
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = stop.cancelled() => return,
                    frame = subscription.next() => frame,
                };
                let Some(frame) = frame else { return };
                let tagged = LeaseTerminalFrame {
                    terminal_id: id.to_string(),
                    frame: wire_frame(frame),
                };
                if out.send(tagged).await.is_err() {
                    return;
                }
            }
        });
    }

    /// Relay until `stop` fires or the server closes the connection.
    pub async fn run(
        self,
        channel: &mut dyn TerminalChannel,
        stop: CancellationToken,
    ) -> Result<(), WorkerEngineError> {
        let tasks = stop.child_token();
        let _stop_tasks = tasks.clone().drop_guard();
        let (out_tx, mut out_rx) = mpsc::channel(256);
        let mut events = self.hub.events();
        let mut subscribed = HashSet::new();
        for record in self.hub.list(Some(&self.owner)) {
            self.subscribe(record.id, &out_tx, &tasks, &mut subscribed);
        }
        let mut grants: HashMap<TerminalId, InputGrant> = HashMap::new();
        let mut refused: HashSet<String> = HashSet::new();
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                event = events.recv() => match event {
                    Ok(TerminalHubEvent::Opened(record)) if record.owner_id == self.owner => {
                        self.subscribe(record.id, &out_tx, &tasks, &mut subscribed);
                    }
                    Ok(TerminalHubEvent::Forgotten(id)) => {
                        grants.remove(&id);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for record in self.hub.list(Some(&self.owner)) {
                            self.subscribe(record.id, &out_tx, &tasks, &mut subscribed);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {}
                },
                Some(frame) = out_rx.recv() => channel.send(frame).await?,
                incoming = channel.recv() => match incoming {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(frame)) => {
                        if let Some(reply) = self.inbound(frame, &subscribed, &mut grants, &mut refused) {
                            channel.send(reply).await?;
                        }
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
    use crate::data::daemon_state::FeatureName;
    use crate::data::ids::LeaseId;
    use crate::data::terminal::TerminalOwnerInfo;
    use crate::engine::terminal_hub::{
        input_channel, TerminalHubConfig, TerminalInput, TerminalRegistration,
    };
    use std::time::Duration;

    struct Pipe {
        to_server: mpsc::UnboundedSender<LeaseTerminalFrame>,
        from_server: mpsc::UnboundedReceiver<LeaseTerminalFrame>,
    }

    #[async_trait]
    impl TerminalChannel for Pipe {
        async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
            self.to_server
                .send(frame)
                .map_err(|_| WorkerEngineError::Transport("closed".into()))
        }
        async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
            self.from_server.recv().await.map(Ok)
        }
    }

    struct Server {
        frames: mpsc::UnboundedReceiver<LeaseTerminalFrame>,
        input: mpsc::UnboundedSender<LeaseTerminalFrame>,
    }

    fn pipe() -> (Pipe, Server) {
        let (to_server, frames) = mpsc::unbounded_channel();
        let (input, from_server) = mpsc::unbounded_channel();
        (
            Pipe {
                to_server,
                from_server,
            },
            Server { frames, input },
        )
    }

    fn register(
        hub: &Arc<TerminalHub>,
        owner: &TerminalOwner,
    ) -> (
        crate::engine::terminal_hub::TerminalHandle,
        mpsc::Receiver<Vec<u8>>,
    ) {
        let (stdin, rx) = input_channel();
        let handle = hub.register(
            TerminalRegistration {
                owner: TerminalOwnerInfo {
                    owner: owner.clone(),
                    feature: FeatureName::Worker,
                    source: None,
                    attempt: 1,
                    allow_remote_input: true,
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
        (handle, rx)
    }

    async fn next_frame(server: &mut Server) -> LeaseTerminalFrame {
        tokio::time::timeout(Duration::from_secs(5), server.frames.recv())
            .await
            .expect("a frame")
            .expect("open")
    }

    // Channel ordering is the acknowledgement: this reply follows every input
    // already sent, so absence assertions never depend on a guessed delay.
    async fn input_barrier(server: &mut Server) {
        server
            .input
            .send(LeaseTerminalFrame {
                terminal_id: "barrier".into(),
                frame: WireFrame::TakeControl,
            })
            .unwrap();
        assert_eq!(next_frame(server).await.terminal_id, "barrier");
    }

    fn start(
        hub: &Arc<TerminalHub>,
        owner: &TerminalOwner,
        allow: bool,
    ) -> (
        Server,
        CancellationToken,
        tokio::task::JoinHandle<Result<(), WorkerEngineError>>,
    ) {
        let (mut pipe, server) = pipe();
        let stop = CancellationToken::new();
        let relay = TerminalRelay::new(hub.clone(), owner.clone(), allow);
        let task = {
            let stop = stop.clone();
            tokio::spawn(async move { relay.run(&mut pipe, stop).await })
        };
        (server, stop, task)
    }

    #[tokio::test]
    async fn multiplexes_every_terminal_of_the_owner_tagged_by_terminal_id() {
        let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
        let owner = TerminalOwner::Lease(LeaseId::new());
        let (first, _rx1) = register(&hub, &owner);
        let (_other, _rx) = register(&hub, &TerminalOwner::Lease(LeaseId::new()));
        let (mut server, stop, task) = start(&hub, &owner, false);
        let snap = next_frame(&mut server).await;
        assert_eq!(snap.terminal_id, first.id().to_string());
        assert!(matches!(snap.frame, WireFrame::Snapshot { .. }));
        let (second, _rx2) = register(&hub, &owner);
        let snap = next_frame(&mut server).await;
        assert_eq!(snap.terminal_id, second.id().to_string());
        second.push_output(b"hello");
        let out = next_frame(&mut server).await;
        assert_eq!(out.terminal_id, second.id().to_string());
        assert_eq!(
            out.frame,
            WireFrame::Output {
                bytes: STANDARD.encode("hello")
            }
        );
        first.exit(3);
        let exit = next_frame(&mut server).await;
        assert_eq!(exit.terminal_id, first.id().to_string());
        assert_eq!(exit.frame, WireFrame::Exit { code: Some(3) });
        stop.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn input_frames_dropped_unless_allowed_and_answered_input_refused() {
        let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
        let owner = TerminalOwner::Lease(LeaseId::new());
        let (term, mut stdin) = register(&hub, &owner);
        let tid = term.id().to_string();
        let input = |bytes: &str| LeaseTerminalFrame {
            terminal_id: tid.clone(),
            frame: WireFrame::Input {
                bytes: STANDARD.encode(bytes),
            },
        };
        let (mut server, stop, task) = start(&hub, &owner, false);
        next_frame(&mut server).await; // snapshot
        server.input.send(input("rm -rf /\n")).unwrap();
        server.input.send(input("again\n")).unwrap();
        let reply = next_frame(&mut server).await;
        assert_eq!(
            reply.frame,
            WireFrame::Error {
                code: INPUT_REFUSED.into(),
                error: "remote input is not allowed for this lease".into()
            }
        );
        input_barrier(&mut server).await;
        assert!(
            server.frames.try_recv().is_err(),
            "refusal is answered once per terminal"
        );
        assert!(
            stdin.try_recv().is_err(),
            "refused input never reaches the agent"
        );
        stop.cancel();
        task.await.unwrap().unwrap();

        let (mut server, stop, task) = start(&hub, &owner, true);
        next_frame(&mut server).await;
        server.input.send(input("ls\n")).unwrap();
        let bytes = tokio::time::timeout(Duration::from_secs(5), stdin.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"ls\n");
        assert!(hub.has_input_holder(term.id()).unwrap());
        server
            .input
            .send(LeaseTerminalFrame {
                terminal_id: "term_nope".into(),
                frame: WireFrame::TakeControl,
            })
            .unwrap();
        assert_eq!(next_frame(&mut server).await.terminal_id, "term_nope");
        stop.cancel();
        task.await.unwrap().unwrap();
        assert!(
            !hub.has_input_holder(term.id()).unwrap(),
            "the grant is released with the relay"
        );
    }

    /// Input needs both sides: the lease's `remote_input` (`allow`) AND the
    /// worker's live `daemon.terminal.remoteInput` (the input policy).
    #[tokio::test]
    async fn input_needs_lease_remote_input_and_live_worker_policy() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
        let owner = TerminalOwner::Lease(LeaseId::new());
        let (term, mut stdin) = register(&hub, &owner);
        let tid = term.id().to_string();
        let input = |bytes: &str| LeaseTerminalFrame {
            terminal_id: tid.clone(),
            frame: WireFrame::Input {
                bytes: STANDARD.encode(bytes),
            },
        };
        let worker_allows = Arc::new(AtomicBool::new(false));
        let start_gated = |lease_allows: bool| {
            let (mut pipe, server) = pipe();
            let stop = CancellationToken::new();
            let policy = worker_allows.clone();
            let relay = TerminalRelay::new(hub.clone(), owner.clone(), lease_allows)
                .with_input_policy(Arc::new(move || policy.load(Ordering::SeqCst)));
            let task = {
                let stop = stop.clone();
                tokio::spawn(async move { relay.run(&mut pipe, stop).await })
            };
            (server, stop, task)
        };
        let refused = WireFrame::Error {
            code: INPUT_REFUSED.into(),
            error: "remote input is not allowed for this lease".into(),
        };
        for (lease_allows, worker) in [(true, false), (false, true), (false, false)] {
            worker_allows.store(worker, Ordering::SeqCst);
            let (mut server, stop, task) = start_gated(lease_allows);
            next_frame(&mut server).await; // snapshot
            server.input.send(input("nope\n")).unwrap();
            assert_eq!(
                next_frame(&mut server).await.frame,
                refused,
                "lease={lease_allows} worker={worker}"
            );
            input_barrier(&mut server).await;
            assert!(
                stdin.try_recv().is_err(),
                "lease={lease_allows} worker={worker}"
            );
            stop.cancel();
            task.await.unwrap().unwrap();
        }
        // Both allow; the worker policy is read live for every frame.
        worker_allows.store(true, Ordering::SeqCst);
        let (mut server, stop, task) = start_gated(true);
        next_frame(&mut server).await;
        server.input.send(input("ls\n")).unwrap();
        let bytes = tokio::time::timeout(Duration::from_secs(5), stdin.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"ls\n");
        worker_allows.store(false, Ordering::SeqCst);
        server.input.send(input("later\n")).unwrap();
        assert_eq!(next_frame(&mut server).await.frame, refused);
        assert!(stdin.try_recv().is_err(), "turned off mid-session");
        stop.cancel();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn wire_frames_base64_encode_bytes() {
        assert_eq!(
            wire_frame(TerminalFrame::Snapshot {
                has_output: false,
                cols: 1,
                rows: 2,
                screen: b"ab".to_vec()
            }),
            WireFrame::Snapshot {
                has_output: false,
                cols: 1,
                rows: 2,
                screen: STANDARD.encode("ab")
            }
        );
        assert_eq!(wire_frame(TerminalFrame::Lagged), WireFrame::Lagged);
    }
}
