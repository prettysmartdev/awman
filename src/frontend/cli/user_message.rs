//! `CliUserMessageQueue` — the queueing UserMessageSink used by the CLI
//! frontend.
//!
//! While a PTY-bound container owns the terminal, the frontend MUST NOT
//! splash status messages into the user's view. Instead the queue collects
//! them and `replay_queued` flushes once the container releases the
//! terminal (after `AgentExecution::wait` and after
//! `WorktreeLifecycle::finalize`).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

#[derive(Debug, Default)]
pub struct CliUserMessageQueue {
    pty_active: Arc<AtomicBool>,
    queue: Vec<UserMessage>,
    /// Messages written through a [`CliMessageHandle`] while a PTY owned the
    /// terminal; flushed by `replay_queued` together with `queue`.
    deferred: Arc<Mutex<Vec<UserMessage>>>,
}

/// A cloneable writer onto the same sink and the same PTY gate, for background
/// tasks (a status watcher) that cannot borrow the frontend. Writes go to
/// stderr at once unless a PTY owns the terminal, in which case they wait for
/// `replay_queued`.
#[derive(Debug, Clone)]
pub struct CliMessageHandle {
    pty_active: Arc<AtomicBool>,
    deferred: Arc<Mutex<Vec<UserMessage>>>,
}

impl CliMessageHandle {
    pub fn write_message(&self, msg: UserMessage) {
        if self.pty_active.load(Ordering::Relaxed) {
            if let Ok(mut q) = self.deferred.lock() {
                q.push(msg);
            }
        } else {
            write_to_stderr(&msg);
        }
    }
}

impl CliUserMessageQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle the PTY-active gate. While `true`, [`write_message`] queues
    /// instead of writing immediately.
    pub fn set_pty_active(&mut self, active: bool) {
        self.pty_active.store(active, Ordering::Relaxed);
    }

    /// A handle background tasks can write through; see [`CliMessageHandle`].
    pub fn handle(&self) -> CliMessageHandle {
        CliMessageHandle {
            pty_active: self.pty_active.clone(),
            deferred: self.deferred.clone(),
        }
    }
}

impl UserMessageSink for CliUserMessageQueue {
    fn write_message(&mut self, msg: UserMessage) {
        if self.pty_active.load(Ordering::Relaxed) {
            self.queue.push(msg);
        } else {
            write_to_stderr(&msg);
        }
    }

    fn replay_queued(&mut self) {
        // `mem::take` is safe here because we don't hold any borrows.
        let mut drained = std::mem::take(&mut self.queue);
        if let Ok(mut late) = self.deferred.lock() {
            drained.append(&mut late);
        }
        for msg in drained {
            write_to_stderr(&msg);
        }
    }
}

fn write_to_stderr(msg: &UserMessage) {
    // Multi-line messages (e.g. the API-key ASCII-art banner) are emitted
    // verbatim: a single-line prefix on a multi-line body misaligns the rest
    // of the lines, breaking box-drawing characters and other layout.
    if msg.text.contains('\n') {
        let _ = writeln!(std::io::stderr(), "{}", msg.text);
        let _ = std::io::stderr().flush();
        return;
    }

    let prefix = match msg.level {
        MessageLevel::Info => "awman:",
        MessageLevel::Warning => "awman warning:",
        MessageLevel::Error => "awman error:",
        MessageLevel::Success => "awman:",
    };
    let _ = writeln!(std::io::stderr(), "{prefix} {}", msg.text);
    let _ = std::io::stderr().flush();
}
