//! `ApiServerCommandFrontend` impl for the TUI.

use async_trait::async_trait;

use crate::command::commands::api_server::{
    ApiKeyDisclosure, ApiServerCommandFrontend, ApiServerRuntime,
};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

#[async_trait]
impl ApiServerCommandFrontend for TuiCommandFrontend {
    async fn serve_until_shutdown(
        &mut self,
        _runtime: ApiServerRuntime,
    ) -> Result<(), CommandError> {
        Err(CommandError::NotImplemented(
            "API server cannot be started from the TUI",
        ))
    }

    /// No box here — the TUI draws its own frames, so box-drawing would sit
    /// inside another frame. The key is stated as text, the same choice the
    /// squad key disclosure makes.
    fn show_api_key(&mut self, disclosure: &ApiKeyDisclosure) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "awman API key (store this — it will not be shown again):\n\n  {}",
                disclosure.key()
            ),
        });
    }
}
