//! Work-queue presentation for the TUI (WI 0143).
//!
//! The TUI only *presents* queue data and returns a typed selection. Every
//! worker decision (registration, claim, eligibility, release) lives in the
//! Layer 2 worker controller, exposed through a [`QueueSource`] adapter. Nothing here
//! talks to a work server.

pub use crate::command::worker::queue_source::*;
use std::collections::BTreeMap;

/// Status-bar badge state, derived from the source each tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueBadge {
    pub for_you: usize,
    pub new: usize,
}

impl QueueBadge {
    pub fn is_empty(&self) -> bool {
        self.for_you == 0 && self.new == 0
    }

    /// `"3 new work items"`, `"1 for you"`, or both; `None` when empty.
    pub fn text(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.new > 0 {
            parts.push(format!(
                "{} new work item{}",
                self.new,
                if self.new == 1 { "" } else { "s" }
            ));
        }
        if self.for_you > 0 {
            parts.push(format!("{} for you", self.for_you));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" \u{00b7} "))
        }
    }
}

/// State of the open `Ctrl-Q` panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuePanelState {
    pub rows: Vec<QueueEntry>,
    pub selected: usize,
    pub showing_body: bool,
    pub error: Option<String>,
    pub loaded: bool,
    /// A one-line message (e.g. "item no longer eligible").
    pub message: Option<String>,
}

impl QueuePanelState {
    pub fn from_snapshot(snapshot: QueueSnapshot) -> Self {
        Self {
            rows: snapshot.items,
            error: snapshot.error,
            loaded: snapshot.loaded,
            ..Self::default()
        }
    }

    /// Replace the rows, keeping the selection on the same item when it is
    /// still listed.
    pub fn apply(&mut self, snapshot: QueueSnapshot) {
        let keep = self
            .selected_entry()
            .map(|e| (e.server.clone(), e.id.clone()));
        self.rows = snapshot.items;
        self.error = snapshot.error;
        self.loaded = snapshot.loaded;
        self.selected = keep
            .and_then(|(server, id)| {
                self.rows
                    .iter()
                    .position(|e| e.server == server && e.id == id)
            })
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        if self.selected_entry().is_none() {
            self.showing_body = false;
        }
    }

    pub fn selected_entry(&self) -> Option<&QueueEntry> {
        self.rows.get(self.selected)
    }

    pub fn move_by(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (self.selected + delta as usize).min(max)
        };
    }
}

/// State of the close-tab prompt for a tab with a live lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseCloseState {
    pub lease: TabLease,
}

/// `"<title> · <server>"`, the tab label of a picked item.
pub fn tab_label(title: &str, server: &str) -> String {
    format!("{title} \u{00b7} {server}")
}

/// `45s`, `12m`, `3h`, `2d`.
pub fn format_age(age_ms: u64) -> String {
    let secs = age_ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `k=v,k=v` with the `awman/` prefix dropped, or `-`.
pub fn format_labels(labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return "-".into();
    }
    labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}
