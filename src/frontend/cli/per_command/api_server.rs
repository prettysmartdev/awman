//! `ApiServerCommandFrontend` impl for the CLI.

use async_trait::async_trait;

use crate::command::commands::api_server::{ApiServeConfig, ApiServerCommandFrontend};
use crate::command::error::CommandError;
use crate::frontend::cli::command_frontend::CliFrontend;

#[async_trait]
impl ApiServerCommandFrontend for CliFrontend {
    async fn serve_until_shutdown(&mut self, config: ApiServeConfig) -> Result<(), CommandError> {
        crate::frontend::api::serve(config).await
    }
}
