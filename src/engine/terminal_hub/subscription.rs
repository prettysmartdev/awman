//! One viewer's bounded stream. Resubscription and snapshots share the
//! registry lock with producers, so recovered output starts after the screen.

use std::collections::VecDeque;
use std::sync::{Mutex, Weak};

use tokio::sync::broadcast;

use super::registry::{HubMessage, HubState, TerminalEntry};
use crate::data::ids::TerminalId;
use crate::data::terminal::{TerminalFrame, TerminalSnapshot};

pub struct TerminalSubscription {
    id: TerminalId,
    /// At most a snapshot and an exit; independent of producer speed.
    pending: VecDeque<TerminalFrame>,
    rx: broadcast::Receiver<HubMessage>,
    state: Weak<Mutex<HubState>>,
    ended: bool,
}

impl std::fmt::Debug for TerminalSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSubscription")
            .field("id", &self.id)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

fn snapshot_frame(snapshot: TerminalSnapshot) -> TerminalFrame {
    TerminalFrame::Snapshot {
        has_output: snapshot.has_output,
        cols: snapshot.cols,
        rows: snapshot.rows,
        screen: snapshot.screen,
    }
}

impl TerminalSubscription {
    pub(super) fn new(id: TerminalId, entry: &TerminalEntry, state: Weak<Mutex<HubState>>) -> Self {
        let mut pending = VecDeque::from([snapshot_frame(entry.mirror.snapshot())]);
        let rx = if let Some(output) = &entry.output {
            output.subscribe()
        } else {
            pending.push_back(TerminalFrame::Exit {
                code: entry
                    .record
                    .exit_code
                    .expect("closed terminal has exit code"),
            });
            let (_, rx) = broadcast::channel(1);
            rx
        };
        Self {
            id,
            pending,
            rx,
            state,
            ended: false,
        }
    }

    pub fn id(&self) -> &TerminalId {
        &self.id
    }

    /// Snapshot first; lag is followed by an atomic fresh snapshot. Exit is
    /// returned once, then all calls return `None`, even for retained records.
    /// Cancellation while waiting for the next frame loses no frame.
    pub async fn next(&mut self) -> Option<TerminalFrame> {
        if self.ended {
            return None;
        }
        if let Some(frame) = self.pending.pop_front() {
            if matches!(frame, TerminalFrame::Exit { .. }) {
                self.ended = true;
            }
            return Some(frame);
        }
        match self.rx.recv().await {
            Ok(HubMessage::Frame(frame)) => Some(frame),
            Ok(HubMessage::Finished { code, .. }) => {
                self.ended = true;
                Some(TerminalFrame::Exit { code })
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.ended = true;
                None
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if self.recover() {
                    Some(TerminalFrame::Lagged)
                } else {
                    self.ended = true;
                    None
                }
            }
        }
    }

    fn recover(&mut self) -> bool {
        if let Some(state) = self.state.upgrade() {
            let state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Ok(entry) = state.entry(&self.id) {
                self.pending
                    .push_back(snapshot_frame(entry.mirror.snapshot()));
                if let Some(output) = &entry.output {
                    self.rx = output.subscribe();
                } else {
                    self.pending.push_back(TerminalFrame::Exit {
                        code: entry
                            .record
                            .exit_code
                            .expect("closed terminal has exit code"),
                    });
                }
                return true;
            }
        }
        // Forgetting (or dropping the hub after exit) removes the mirror.
        // The bounded queue's last message still carries the final screen.
        loop {
            match self.rx.try_recv() {
                Ok(HubMessage::Finished { code, snapshot }) => {
                    self.pending.push_back(snapshot_frame((*snapshot).clone()));
                    self.pending.push_back(TerminalFrame::Exit { code });
                    return true;
                }
                Ok(HubMessage::Frame(_)) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    }
}
