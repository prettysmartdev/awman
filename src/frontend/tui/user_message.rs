//! The TUI's status log feed. The log and its sink are Layer 2
//! (`command::instance::feeds`) since WI 0142: the command thread writes them
//! and the tab draws them. Re-exported here under their established names.

pub use crate::command::instance::feeds::{
    SharedStatusLog, StatusLogEntry, StatusLogSink as TuiUserMessageSink,
};
