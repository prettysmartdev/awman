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
