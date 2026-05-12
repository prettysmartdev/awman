//! TUI message sink — routes `UserMessage`s to the active tab's status log.

use std::sync::{Arc, Mutex};

use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

/// Status log entry stored per-tab.
#[derive(Debug, Clone)]
pub struct StatusLogEntry {
    pub level: MessageLevel,
    pub text: String,
}

/// Shared reference to a tab's status log. The command thread writes here;
/// the render loop reads.
pub type SharedStatusLog = Arc<Mutex<Vec<StatusLogEntry>>>;

/// TUI implementation of `UserMessageSink`. Constructed per-command invocation
/// and pointed at the active tab's shared status log.
pub struct TuiUserMessageSink {
    log: SharedStatusLog,
}

impl TuiUserMessageSink {
    pub fn new(log: SharedStatusLog) -> Self {
        Self { log }
    }
}

impl UserMessageSink for TuiUserMessageSink {
    fn write_message(&mut self, msg: UserMessage) {
        if let Ok(mut log) = self.log.lock() {
            log.push(StatusLogEntry {
                level: msg.level,
                text: msg.text,
            });
        }
    }

    fn replay_queued(&mut self) {
        // TUI renders live — no queuing needed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn make_sink() -> (TuiUserMessageSink, SharedStatusLog) {
        let log: SharedStatusLog = Arc::new(Mutex::new(Vec::new()));
        let sink = TuiUserMessageSink::new(log.clone());
        (sink, log)
    }

    #[test]
    fn write_message_appends_to_status_log() {
        let (mut sink, log) = make_sink();
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "hello".to_string(),
        });
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[0].level, MessageLevel::Info);
    }

    #[test]
    fn write_message_preserves_level() {
        let (mut sink, log) = make_sink();
        for level in [
            MessageLevel::Info,
            MessageLevel::Warning,
            MessageLevel::Error,
            MessageLevel::Success,
        ] {
            sink.write_message(UserMessage {
                level,
                text: format!("{level:?}"),
            });
        }
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].level, MessageLevel::Info);
        assert_eq!(entries[1].level, MessageLevel::Warning);
        assert_eq!(entries[2].level, MessageLevel::Error);
        assert_eq!(entries[3].level, MessageLevel::Success);
    }

    #[test]
    fn multiple_messages_append_in_order() {
        let (mut sink, log) = make_sink();
        for text in ["first", "second", "third"] {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: text.to_string(),
            });
        }
        let entries = log.lock().unwrap();
        assert_eq!(entries[0].text, "first");
        assert_eq!(entries[1].text, "second");
        assert_eq!(entries[2].text, "third");
    }

    #[test]
    fn replay_queued_is_a_noop() {
        let (mut sink, log) = make_sink();
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "msg".to_string(),
        });
        sink.replay_queued();
        // Message must still be in the log (replay_queued must not drain it).
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn convenience_info_writes_info_level() {
        let (mut sink, log) = make_sink();
        sink.info("test");
        let entries = log.lock().unwrap();
        assert_eq!(entries[0].level, MessageLevel::Info);
        assert_eq!(entries[0].text, "test");
    }

    #[test]
    fn convenience_error_msg_writes_error_level() {
        let (mut sink, log) = make_sink();
        sink.error_msg("boom");
        let entries = log.lock().unwrap();
        assert_eq!(entries[0].level, MessageLevel::Error);
    }

    #[test]
    fn convenience_success_writes_success_level() {
        let (mut sink, log) = make_sink();
        sink.success("ok");
        let entries = log.lock().unwrap();
        assert_eq!(entries[0].level, MessageLevel::Success);
    }
}
