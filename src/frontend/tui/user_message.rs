//! The TUI's status log feed. The log and its sink are Layer 2
//! (`command::instance::feeds`) since WI 0142: the command thread writes them
//! and the tab draws them. Re-exported here under their established names.

#[cfg(test)]
use crate::data::message::{UserMessage, UserMessageSink};

pub use crate::command::instance::feeds::{
    SharedStatusLog, StatusLogEntry, StatusLogSink as TuiUserMessageSink,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::MessageLevel;
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
