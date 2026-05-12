//! `HeadlessCommandFrontend` impl for the TUI.

use async_trait::async_trait;

use crate::command::commands::headless::{HeadlessCommandFrontend, HeadlessServeConfig};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

#[async_trait]
impl HeadlessCommandFrontend for TuiCommandFrontend {
    async fn serve_until_shutdown(
        &mut self,
        _config: HeadlessServeConfig,
    ) -> Result<(), CommandError> {
        Err(CommandError::NotImplemented(
            "headless server cannot be started from the TUI",
        ))
    }
}
