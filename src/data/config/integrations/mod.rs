//! Integration-specific config sections are introduced by their owner under
//! `integrations/<name>.rs`. The shared `enabled` switch is typed in `tui.rs`;
//! other `tui.integrations.<name>` fields remain opaque JSON until owned by
//! an integration.
