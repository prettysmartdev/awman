//! API HTTP frontend — router, listener, signal handling, and serving only.

pub mod command_frontend;
pub mod error;
pub mod local;
pub mod outputs;
pub mod routes;
pub mod workflow_view;

use crate::command::commands::api_server::queue_worker::ApiCommandFrontendFactory;
use crate::data::fs::api_command_log::CommandStateWriter;

/// Layer 3 construction of the concrete API presentation adapter.
#[derive(Clone, Copy, Default)]
pub struct ApiFrontendFactory;

impl ApiCommandFrontendFactory for ApiFrontendFactory {
    type Frontend = command_frontend::ApiDispatchFrontend;

    fn frontend_for(
        &self,
        command: &crate::data::fs::api_db::CommandRecord,
        event_bus: crate::command::commands::api_server::event_bus::EventBusSender,
        state_writer: CommandStateWriter,
    ) -> Self::Frontend {
        let args: Vec<String> = serde_json::from_str(&command.args).unwrap_or_default();
        command_frontend::ApiDispatchFrontend::for_frontend(
            &command.subcommand,
            &args,
            event_bus,
            crate::command::dispatch::catalogue::FrontendKind::Daemon {
                feature: crate::data::daemon_state::FeatureName::Worker,
                listener: if command.listener == "network" {
                    crate::data::daemon_state::CommandListener::Network
                } else {
                    crate::data::daemon_state::CommandListener::Local
                },
            },
        )
        .with_interaction(
            if args
                .iter()
                .take_while(|s| s.as_str() != "--")
                .any(|s| s == "--headless=false")
            {
                crate::data::config::daemon::Interaction::Interactive
            } else {
                crate::data::config::daemon::Interaction::Headless
            },
        )
        .with_state_writer(state_writer)
        .deferring_done()
    }
}
