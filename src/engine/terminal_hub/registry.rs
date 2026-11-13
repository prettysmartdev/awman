//! The hub's table, protected by one mutex. Finished records and tombstones
//! are bounded independently; subscribers never retain this table.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::broadcast;

use super::{ScreenMirror, TerminalHubError, TerminalInput};
use crate::data::ids::TerminalId;
use crate::data::terminal::{
    TerminalFrame, TerminalOwner, TerminalRecord, TerminalSnapshot, TerminalState,
};

/// A final snapshot travels with the exit internally, so even a subscriber
/// overtaken by eviction can recover before reporting exit. It is shared by
/// all receivers, and is never sent as an extra wire frame on a healthy stream.
#[derive(Clone)]
pub(super) enum HubMessage {
    Frame(TerminalFrame),
    Finished {
        code: i32,
        snapshot: Arc<TerminalSnapshot>,
    },
}

pub(super) struct TerminalEntry {
    pub record: TerminalRecord,
    pub mirror: ScreenMirror,
    /// Dropped on exit, closing receivers even while the record remains.
    pub output: Option<broadcast::Sender<HubMessage>>,
    pub input: Option<TerminalInput>,
    pub default_size: (u16, u16),
    pub holder: Option<u64>,
}

impl TerminalEntry {
    pub fn resize(&mut self, cols: u16, rows: u16, notify_pty: bool) {
        let requested_size = (cols, rows);
        let (cols, rows) = super::mirror::normalize_size(cols, rows);
        let changed = (self.record.cols, self.record.rows) != (cols, rows);
        // Even a bridge's already-applied resize must be corrected when it
        // would leave the real PTY smaller than the parser's safe minimum.
        if (notify_pty && changed) || requested_size != (cols, rows) {
            if let Some(resize) = self.input.as_ref().and_then(|input| input.resize.as_ref()) {
                resize(cols, rows);
            }
        }
        if !changed {
            return;
        }
        self.mirror.resize(cols, rows);
        self.record.cols = cols;
        self.record.rows = rows;
        if let Some(output) = &self.output {
            let _ = output.send(HubMessage::Frame(TerminalFrame::Resize { cols, rows }));
        }
    }
}

#[derive(Default)]
pub(super) struct HubState {
    pub terminals: HashMap<TerminalId, TerminalEntry>,
    pub by_owner: HashMap<TerminalOwner, Vec<TerminalId>>,
    pub finished: VecDeque<TerminalId>,
    pub gone: HashSet<TerminalId>,
    gone_order: VecDeque<TerminalId>,
    pub next_grant: u64,
}

impl HubState {
    pub fn entry(&self, id: &TerminalId) -> Result<&TerminalEntry, TerminalHubError> {
        self.terminals.get(id).ok_or_else(|| self.missing(id))
    }

    pub fn entry_mut(&mut self, id: &TerminalId) -> Result<&mut TerminalEntry, TerminalHubError> {
        let error = self.missing(id);
        self.terminals.get_mut(id).ok_or(error)
    }

    fn missing(&self, id: &TerminalId) -> TerminalHubError {
        if self.gone.contains(id) {
            TerminalHubError::Gone
        } else {
            TerminalHubError::NotFound
        }
    }

    pub fn open_entry_mut(
        &mut self,
        id: &TerminalId,
    ) -> Result<&mut TerminalEntry, TerminalHubError> {
        let entry = self.entry_mut(id)?;
        if entry.record.state == TerminalState::Exited {
            return Err(TerminalHubError::Closed);
        }
        Ok(entry)
    }

    pub fn close(&mut self, id: &TerminalId, code: i32) -> bool {
        let Ok(entry) = self.open_entry_mut(id) else {
            return false;
        };
        entry.record.state = TerminalState::Exited;
        entry.record.exit_code = Some(code);
        entry.input = None;
        entry.holder = None;
        if let Some(output) = entry.output.take() {
            if output.receiver_count() > 0 {
                let _ = output.send(HubMessage::Finished {
                    code,
                    snapshot: Arc::new(entry.mirror.snapshot()),
                });
            }
        }
        self.finished.push_back(id.clone());
        true
    }

    pub fn forget(&mut self, id: &TerminalId, tombstone_limit: usize) -> bool {
        if !self
            .terminals
            .get(id)
            .is_some_and(|entry| entry.record.state == TerminalState::Exited)
        {
            return false;
        }
        let entry = self.terminals.remove(id).expect("entry checked above");
        if let Some(ids) = self.by_owner.get_mut(&entry.record.owner_id) {
            ids.retain(|other| other != id);
            if ids.is_empty() {
                self.by_owner.remove(&entry.record.owner_id);
            }
        }
        self.finished.retain(|other| other != id);
        self.gone.insert(id.clone());
        self.gone_order.push_back(id.clone());
        while self.gone_order.len() > tombstone_limit.max(1) {
            if let Some(oldest) = self.gone_order.pop_front() {
                self.gone.remove(&oldest);
            }
        }
        true
    }
}
