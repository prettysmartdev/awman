//! The bottom-row squad health indicator (WI 0112 Part 2).
//!
//! The poller is Layer 2 since WI 0142
//! (`command::commands::squad::indicator`); the TUI only reads the shared
//! slot and, in `render::command_box`, picks the colour. Re-exported here
//! under the established names.

pub use crate::command::commands::squad::indicator::{
    SharedSquadHealth as SharedSquadIndicator, SquadIndicatorPoller, SQUAD_INDICATOR_INTERVAL,
    SQUAD_INDICATOR_PROBE_TIMEOUT,
};
