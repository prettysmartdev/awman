//! `HeadlessCommandFrontend` impl for the CLI.

use async_trait::async_trait;

use crate::command::commands::headless::{HeadlessCommandFrontend, HeadlessServeConfig};
use crate::command::error::CommandError;
use crate::frontend::cli::command_frontend::CliFrontend;

#[async_trait]
impl HeadlessCommandFrontend for CliFrontend {
    async fn serve_until_shutdown(
        &mut self,
        config: HeadlessServeConfig,
    ) -> Result<(), CommandError> {
        crate::frontend::headless::serve(config).await
    }
}
