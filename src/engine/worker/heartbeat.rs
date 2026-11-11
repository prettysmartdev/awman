//! Lease heartbeats.
//!
//! [`HeartbeatLoop::spawn`] posts `POST /v1/leases/{id}/heartbeat` every
//! `heartbeat_ms` (the server's value; [`DEFAULT_HEARTBEAT`] when absent) and
//! immediately whenever the run's state changes. A `cancel: true` answer, a
//! `409 lease_lost` or a revoked link fires [`HeartbeatControl::cancelled`];
//! after a lost lease [`HeartbeatControl::may_report`] is false, so the run's
//! result is discarded instead of posted.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use awman_wsp::{HeartbeatRequest, HeartbeatState};
use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use super::session::{LeaseApi, LeaseRef};

/// The heartbeat interval when the server sends none (15 s).
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

/// `heartbeat_ms` from registration, or [`DEFAULT_HEARTBEAT`].
pub fn heartbeat_interval(heartbeat_ms: Option<u64>) -> Duration {
    match heartbeat_ms {
        Some(ms) if ms > 0 => Duration::from_millis(ms),
        _ => DEFAULT_HEARTBEAT,
    }
}

/// What the next heartbeat reports.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatStatus {
    pub state: HeartbeatState,
    pub step: Option<String>,
    pub progress: Option<serde_json::Value>,
}

impl HeartbeatStatus {
    pub fn new(state: HeartbeatState) -> Self {
        Self {
            state,
            step: None,
            progress: None,
        }
    }
}

/// How the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatEnd {
    /// The server answered `cancel: true`.
    Cancelled { reason: Option<String> },
    /// `409 lease_lost`: stop the run and discard its result.
    LeaseLost,
    /// The link was revoked.
    LinkRevoked,
    /// The owner stopped the loop (the run finished).
    Stopped,
}

/// The run's side of a heartbeat loop.
#[derive(Clone)]
pub struct HeartbeatControl {
    status: Arc<watch::Sender<HeartbeatStatus>>,
    cancel: CancellationToken,
    stop: CancellationToken,
    lost: Arc<std::sync::atomic::AtomicBool>,
    fenced: CancellationToken,
    expires_at: watch::Receiver<DateTime<Utc>>,
    cancel_reason: Arc<Mutex<Option<String>>>,
    /// How many [`AwaitingInputGuard`]s are held (pending dialogs).
    waiting: Arc<watch::Sender<usize>>,
}

impl HeartbeatControl {
    /// Report a new state (sent at once, not at the next tick).
    pub fn set_status(&self, status: HeartbeatStatus) {
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    }

    pub fn set_state(&self, state: HeartbeatState) {
        let mut status = self.status.borrow().clone();
        status.state = state;
        self.set_status(status);
    }

    /// Fires on `cancel: true`, a lost lease or a revoked link: the run must
    /// stop and its containers be cancelled.
    pub fn cancelled(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// False once the lease was lost: never post the result or a release.
    pub fn may_report(&self) -> bool {
        if self.expires_at() <= Utc::now() {
            self.fence();
        }
        !self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// One fencing latch shared by heartbeats, events, credentials and reporting.
    pub fn fence(&self) {
        self.lost.store(true, std::sync::atomic::Ordering::SeqCst);
        self.fenced.cancel();
        self.cancel.cancel();
    }

    /// Unlike ordinary cancellation, fencing forbids further events.
    pub fn fenced(&self) -> CancellationToken {
        self.fenced.clone()
    }

    pub fn cancel_reason(&self) -> Option<String> {
        self.cancel_reason
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The lease expiry from the latest heartbeat answer.
    pub fn expires_at(&self) -> DateTime<Utc> {
        *self.expires_at.borrow()
    }

    pub fn expiry_watch(&self) -> watch::Receiver<DateTime<Utc>> {
        self.expires_at.clone()
    }

    /// End the loop (the run finished; the result follows).
    pub fn stop(&self) {
        self.stop.cancel();
    }

    /// The signal a higher layer holds while a step dialog waits for a
    /// person (an interactive, non-yolo run): while any guard is held, a
    /// `running` lease reports `awaiting_input`, sent at once.
    pub fn awaiting_input(&self) -> AwaitingInputSignal {
        AwaitingInputSignal {
            waiting: self.waiting.clone(),
        }
    }
}

/// Marks a lease as waiting for a person while an [`AwaitingInputGuard`] is
/// held. Cheap to clone; usable from synchronous code.
#[derive(Clone)]
pub struct AwaitingInputSignal {
    waiting: Arc<watch::Sender<usize>>,
}

impl AwaitingInputSignal {
    /// Report `awaiting_input` until the returned guard is dropped.
    pub fn hold(&self) -> AwaitingInputGuard {
        self.waiting.send_modify(|n| *n += 1);
        AwaitingInputGuard {
            waiting: self.waiting.clone(),
        }
    }

    pub fn is_waiting(&self) -> bool {
        *self.waiting.borrow() > 0
    }
}

/// Held while a dialog is pending; dropping it clears the signal.
pub struct AwaitingInputGuard {
    waiting: Arc<watch::Sender<usize>>,
}

impl Drop for AwaitingInputGuard {
    fn drop(&mut self) {
        self.waiting.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// The state a beat reports: a pending dialog turns `running` into
/// `awaiting_input`; preparing and pushing are reported as they are.
fn reported_state(state: HeartbeatState, waiting: usize) -> HeartbeatState {
    match state {
        HeartbeatState::Running if waiting > 0 => HeartbeatState::AwaitingInput,
        state => state,
    }
}

pub struct HeartbeatLoop;

impl HeartbeatLoop {
    /// Start heartbeating `lease` at `interval`, first beat immediately.
    pub fn spawn(
        api: Arc<dyn LeaseApi>,
        lease: LeaseRef,
        interval: Duration,
        initial: HeartbeatStatus,
        expires_at: DateTime<Utc>,
    ) -> (HeartbeatControl, JoinHandle<HeartbeatEnd>) {
        let (status_tx, mut status_rx) = watch::channel(initial);
        let (expires_tx, expires_rx) = watch::channel(expires_at);
        let control = HeartbeatControl {
            status: Arc::new(status_tx),
            cancel: CancellationToken::new(),
            stop: CancellationToken::new(),
            lost: Arc::default(),
            fenced: CancellationToken::new(),
            expires_at: expires_rx,
            cancel_reason: Arc::default(),
            waiting: Arc::new(watch::channel(0).0),
        };
        let task = {
            let control = control.clone();
            tokio::spawn(async move {
                let mut waiting_rx = control.waiting.subscribe();
                let mut deadline = tokio::time::Instant::now()
                    + (expires_at - Utc::now()).to_std().unwrap_or_default();
                let mut retry_soon = false;
                let mut first = true;
                // Set once the server answers `cancel: true`. The loop keeps
                // beating after that, so the lease stays valid while the run
                // stops and its partial result is posted.
                let mut cancelled: Option<Option<String>> = None;
                let stopped = |cancelled: &Option<Option<String>>| match cancelled {
                    Some(reason) => HeartbeatEnd::Cancelled {
                        reason: reason.clone(),
                    },
                    None => HeartbeatEnd::Stopped,
                };
                loop {
                    if tokio::time::Instant::now() >= deadline {
                        control.fence();
                        return HeartbeatEnd::LeaseLost;
                    }
                    if !first {
                        let wait = if retry_soon { interval / 3 } else { interval };
                        tokio::select! {
                            biased;
                            _ = tokio::time::sleep_until(deadline) => {
                                control.fence(); return HeartbeatEnd::LeaseLost;
                            },
                            _ = control.stop.cancelled() => return stopped(&cancelled),
                            changed = status_rx.changed() => {
                                if changed.is_err() {
                                    return stopped(&cancelled);
                                }
                            }
                            _ = waiting_rx.changed() => {}
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }
                    first = false;
                    let status = status_rx.borrow_and_update().clone();
                    let request = HeartbeatRequest {
                        state: reported_state(status.state, *waiting_rx.borrow_and_update()),
                        step: status.step,
                        progress: status.progress,
                    };
                    let answer = tokio::select! {
                        biased;
                        _ = tokio::time::sleep_until(deadline) => {
                            control.fence(); return HeartbeatEnd::LeaseLost;
                        },
                        _ = control.stop.cancelled() => return stopped(&cancelled),
                        answer = api.heartbeat(&lease, &request) => answer,
                    };
                    match answer {
                        Ok(response) => {
                            if tokio::time::Instant::now() >= deadline
                                || control.expires_at() <= Utc::now()
                                || response.expires_at <= Utc::now()
                            {
                                control.fence();
                                return HeartbeatEnd::LeaseLost;
                            }
                            deadline = tokio::time::Instant::now()
                                + (response.expires_at - Utc::now())
                                    .to_std()
                                    .unwrap_or_default();
                            retry_soon = false;
                            let _ = expires_tx.send(response.expires_at);
                            if response.cancel && cancelled.is_none() {
                                *control
                                    .cancel_reason
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner()) =
                                    response.cancel_reason.clone();
                                control.cancel.cancel();
                                cancelled = Some(response.cancel_reason);
                            }
                        }
                        Err(WorkerEngineError::LeaseLost) => {
                            control.fence();
                            return HeartbeatEnd::LeaseLost;
                        }
                        Err(WorkerEngineError::LinkRevoked) => {
                            control.fence();
                            return HeartbeatEnd::LinkRevoked;
                        }
                        Err(error) => {
                            tracing::warn!(lease = %lease.server_lease_id, %error, "lease heartbeat failed; retrying");
                            retry_soon = true;
                        }
                    }
                }
            })
        };
        (control, task)
    }
}
