//! Docker backend — `pub(super)`. Concrete type is invisible outside
//! `src/engine/container/`.
//!
//! Implementation note: this module deliberately stops short of shelling out
//! to `docker run` directly. Container execution semantics (PTY allocation,
//! interactive vs print mode, prompt injection) are large enough that the
//! actual subprocess work lives in the implementing layer alongside the
//! backend trait. Higher work items wire the real Docker CLI via this path;
//! the structural typed object surface is complete here.

use std::process::Command;

use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::instance::{
    handle_now, ContainerExecution, ContainerExitInfo, ContainerId, ContainerInstance,
    ContainerStats, ExecutionBackend,
};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::error::EngineError;

#[derive(Debug, Default)]
pub(super) struct DockerBackend;

impl DockerBackend {
    pub(super) fn new() -> Self {
        Self
    }

    /// Probe whether the docker daemon is reachable. Returns `false` quietly
    /// when the binary is missing or the daemon is down.
    pub(super) fn is_available() -> bool {
        Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl ContainerBackend for DockerBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn ContainerInstance>, EngineError> {
        let image = options
            .image
            .clone()
            .ok_or_else(|| EngineError::MissingRequiredOption("Image".into()))?;
        let name = options
            .name
            .clone()
            .unwrap_or_else(|| ContainerName::new(crate::engine::container::naming::generate_container_name()));
        Ok(Box::new(DockerContainerInstance {
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
            "DockerBackend::stats is not yet wired (lands with full backend in a later WI)",
        ))
    }

    fn stop(&self, _handle: &ContainerHandle) -> Result<(), EngineError> {
        Err(EngineError::NotImplemented(
            "DockerBackend::stop is not yet wired",
        ))
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}

struct DockerContainerInstance {
    id: ContainerId,
    name: ContainerName,
    image: ImageRef,
    options: ResolvedContainerOptions,
}

impl ContainerInstance for DockerContainerInstance {
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
        // Until full subprocess wiring lands, hand back a finished execution
        // representing a no-op success. Higher-level engines (and 0070) wire
        // the real PTY-allocating runner.
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

#[allow(dead_code)]
struct DockerExecution {
    info: ContainerExitInfo,
}

impl ExecutionBackend for DockerExecution {
    fn wait_blocking(self: Box<Self>) -> Result<ContainerExitInfo, EngineError> {
        Ok(self.info)
    }
    fn cancel(&self) -> Result<(), EngineError> {
        Ok(())
    }
}
