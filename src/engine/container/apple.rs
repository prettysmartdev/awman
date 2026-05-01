//! Apple Containers backend — `pub(super)`. Same shape as Docker; the CPU%
//! sampling and JSON `container stats` parsing land alongside the Docker
//! impl in a follow-on WI.

use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::instance::{
    handle_now, ContainerExecution, ContainerExitInfo, ContainerId, ContainerInstance,
    ContainerStats,
};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::error::EngineError;

#[derive(Debug, Default)]
pub(super) struct AppleBackend;

impl AppleBackend {
    pub(super) fn new() -> Self {
        Self
    }
}

impl ContainerBackend for AppleBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn ContainerInstance>, EngineError> {
        let image = options
            .image
            .clone()
            .ok_or_else(|| EngineError::ConflictingOptions("missing required Image option".into()))?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        Ok(Box::new(AppleContainerInstance {
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
        }))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<ContainerHandle>, EngineError> {
        Ok(Vec::new())
    }

    fn stats(&self, _handle: &ContainerHandle) -> Result<ContainerStats, EngineError> {
        Err(EngineError::NotImplemented(
            "AppleBackend::stats is not yet wired (lands with full backend in a later WI)",
        ))
    }

    fn stop(&self, _handle: &ContainerHandle) -> Result<(), EngineError> {
        Err(EngineError::NotImplemented(
            "AppleBackend::stop is not yet wired",
        ))
    }

    fn name(&self) -> &'static str {
        "apple-containers"
    }
}

struct AppleContainerInstance {
    id: ContainerId,
    name: ContainerName,
    image: ImageRef,
    options: ResolvedContainerOptions,
}

impl ContainerInstance for AppleContainerInstance {
    fn id(&self) -> &ContainerId {
        &self.id
    }
    fn name(&self) -> &ContainerName {
        &self.name
    }
    fn image(&self) -> &ImageRef {
        &self.image
    }

    fn run_with_frontend(
        self: Box<Self>,
        _frontend: Box<dyn crate::engine::container::frontend::ContainerFrontend>,
    ) -> Result<ContainerExecution, EngineError> {
        let handle = handle_now(&self.id, &self.name, &self.image);
        let info = ContainerExitInfo {
            exit_code: 0,
            signal: None,
            started_at: handle.started_at,
            ended_at: handle.started_at,
        };
        let _ = self.options;
        Ok(ContainerExecution::finished(handle, info))
    }
}
