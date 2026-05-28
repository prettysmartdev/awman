//! `ApiServerCommandFrontend` impl for the TUI.

use async_trait::async_trait;

use crate::command::commands::api_server::{ApiServeConfig, ApiServerCommandFrontend};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

#[async_trait]
impl ApiServerCommandFrontend for TuiCommandFrontend {
    async fn serve_until_shutdown(&mut self, _config: ApiServeConfig) -> Result<(), CommandError> {
        Err(CommandError::NotImplemented(
            "API server cannot be started from the TUI",
        ))
    }
}
