//! `DaemonCommandFrontend` for the TUI.
use async_trait::async_trait;

use crate::command::commands::daemon::lifecycle::DaemonCommandFrontend;
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

#[async_trait]
impl DaemonCommandFrontend for TuiCommandFrontend {
    /// The TUI cannot become the daemon process; `daemon start --background`
    /// hands it to the OS service manager instead.
    async fn host_daemon(
        &mut self,
        _env: crate::data::config::env::EnvSnapshot,
        _set: crate::data::config::overrides::ConfigOverrides,
    ) -> Result<(), CommandError> {
        Err(CommandError::Other(
            "the TUI cannot host the daemon; run `daemon start --background`".into(),
        ))
    }
}
