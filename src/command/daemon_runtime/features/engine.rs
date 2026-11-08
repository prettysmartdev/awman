//! The `engine` feature is a Layer 1 lifecycle primitive
//! (`engine::daemon::features::engine`, WI 0142 D5); Layer 2 wires its
//! `EngineInstanceHost` into it here.
pub use crate::engine::daemon::features::engine::*;

use std::sync::Arc;

use crate::data::config::daemon::EngineSettings;

/// Build the daemon's engine feature over `host` (the Layer 2 instance host).
pub fn engine_feature(settings: EngineSettings, host: Arc<dyn EngineHost>) -> Arc<EngineFeature> {
    EngineFeature::new(settings, host)
}

/// The daemon's Layer 2 instance host over the core's hub and executor,
/// opening instances with today's startup and `presentation`'s wording.
pub fn engine_host(
    core: &crate::engine::daemon::core::Core,
    presentation: Arc<dyn crate::command::instance::frontend::InstancePresentation>,
) -> Arc<crate::command::instance::host::EngineInstanceHost> {
    use crate::command::instance::host::{EngineInstanceHost, HostParts, StartupOpener};
    EngineInstanceHost::new(HostParts {
        hub: core.terminal_hub().clone(),
        catalogue: crate::command::dispatch::catalogue::CommandCatalogue::get(),
        registry: crate::command::integrations::IntegrationRegistry::compiled_in(),
        opener: StartupOpener::new(presentation),
        executor: Some(core.executor().clone()),
        build: env!("CARGO_PKG_VERSION").to_string(),
    })
}
