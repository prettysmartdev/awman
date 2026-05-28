//! `CliUserMessageQueue` — the queueing UserMessageSink used by the CLI
//! frontend.
//!
//! While a PTY-bound container owns the terminal, the frontend MUST NOT
//! splash status messages into the user's view. Instead the queue collects
//! them and `replay_queued` flushes once the container releases the
//! terminal (after `ContainerExecution::wait` and after
//! `WorktreeLifecycle::finalize`).

use std::io::Write;

use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

#[derive(Debug, Default)]
pub struct CliUserMessageQueue {
    pty_active: bool,
    queue: Vec<UserMessage>,
}

impl CliUserMessageQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle the PTY-active gate. While `true`, [`write_message`] queues
    /// instead of writing immediately.
    pub fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }

    pub fn pty_active(&self) -> bool {
        self.pty_active
    }
}

impl UserMessageSink for CliUserMessageQueue {
    fn write_message(&mut self, msg: UserMessage) {
        if self.pty_active {
            self.queue.push(msg);
        } else {
            write_to_stderr(&msg);
        }
    }

    fn replay_queued(&mut self) {
        // `mem::take` is safe here because we don't hold any borrows.
        let drained = std::mem::take(&mut self.queue);
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

#[cfg(test)]
impl CliUserMessageQueue {
    /// Returns the number of messages currently held in the queue.
    /// Test-only introspection helper.
    pub(crate) fn pending_count(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::message::MessageLevel;

    fn info(text: &str) -> UserMessage {
        UserMessage {
            level: MessageLevel::Info,
            text: text.to_string(),
        }
    }

    fn warning(text: &str) -> UserMessage {
        UserMessage {
            level: MessageLevel::Warning,
            text: text.to_string(),
        }
    }

    fn error_msg(text: &str) -> UserMessage {
        UserMessage {
            level: MessageLevel::Error,
            text: text.to_string(),
        }
    }

    // ─── PTY active → messages are queued ─────────────────────────────────────

    #[test]
    fn write_message_queues_when_pty_active() {
        let mut q = CliUserMessageQueue::new();
        q.set_pty_active(true);
        q.write_message(info("hello"));
        assert_eq!(q.pending_count(), 1);
    }

    #[test]
    fn write_message_queues_multiple_when_pty_active() {
        let mut q = CliUserMessageQueue::new();
        q.set_pty_active(true);
        q.write_message(info("first"));
        q.write_message(warning("second"));
        q.write_message(error_msg("third"));
        assert_eq!(q.pending_count(), 3);
    }

    #[test]
    fn write_message_does_not_queue_when_pty_inactive() {
        let mut q = CliUserMessageQueue::new();
        // pty_active defaults to false
        assert!(!q.pty_active());
        q.write_message(info("immediate"));
        assert_eq!(q.pending_count(), 0);
    }

    // ─── replay_queued drains the queue ───────────────────────────────────────

    #[test]
    fn replay_queued_drains_queue_to_empty() {
        let mut q = CliUserMessageQueue::new();
        q.set_pty_active(true);
        q.write_message(info("queued message"));
        q.write_message(warning("another"));
        assert_eq!(q.pending_count(), 2);

        // Deactivate PTY so replay goes to stderr (real test just checks drain).
        q.set_pty_active(false);
        q.replay_queued();

        assert_eq!(q.pending_count(), 0);
    }

    #[test]
    fn replay_queued_on_empty_queue_is_no_op() {
        let mut q = CliUserMessageQueue::new();
        // Should not panic or error.
        q.replay_queued();
        assert_eq!(q.pending_count(), 0);
    }

    #[test]
    fn replay_queued_called_twice_stays_empty() {
        let mut q = CliUserMessageQueue::new();
        q.set_pty_active(true);
        q.write_message(info("msg"));
        q.set_pty_active(false);
        q.replay_queued();
        q.replay_queued(); // second call must not panic
        assert_eq!(q.pending_count(), 0);
    }

    // ─── PTY toggle behavior ──────────────────────────────────────────────────

    #[test]
    fn pty_active_toggle_changes_observable_behavior() {
        let mut q = CliUserMessageQueue::new();

        // Inactive → message goes directly to stderr (queue stays 0).
        q.write_message(info("immediate"));
        assert_eq!(q.pending_count(), 0);

        // Activate → subsequent messages are queued.
        q.set_pty_active(true);
        q.write_message(info("queued"));
        assert_eq!(q.pending_count(), 1);

        // Deactivate again → new messages go directly to stderr.
        q.set_pty_active(false);
        q.write_message(info("immediate again"));
        assert_eq!(q.pending_count(), 1); // still 1 (only the earlier queued one)
    }

    #[test]
    fn pty_active_accessor_matches_set_state() {
        let mut q = CliUserMessageQueue::new();
        assert!(!q.pty_active());
        q.set_pty_active(true);
        assert!(q.pty_active());
        q.set_pty_active(false);
        assert!(!q.pty_active());
    }
}
