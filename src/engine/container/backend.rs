//! Internal `ContainerBackend` trait — NOT pub outside `src/engine/container/`.
//!
//! Implementations: `docker::DockerBackend`, `apple::AppleBackend`.

use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::instance::{ContainerInstance, ContainerStats};
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::error::EngineError;

/// What every container backend must support. The concrete type is hidden
/// behind `Box<dyn ContainerBackend>` and never escapes this module.
pub(super) trait ContainerBackend: Send + Sync {
    /// Build a `ContainerInstance` from resolved options. The image is NOT
    /// pulled or built here — that's a separate concern handled by
    /// higher-level engines (e.g. `AgentEngine::ensure_available`).
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn ContainerInstance>, EngineError>;

    fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError>;

    /// List all running amux containers without requiring a session.
    /// Default falls back to an empty list.
    fn list_running_all(&self) -> Result<Vec<ContainerHandle>, EngineError> {
        Ok(Vec::new())
    }

    fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError>;

    fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError>;

    /// Build the CLI arguments for `docker exec -it` (or equivalent) into a
    /// running container. Used by TUI re-attach.
    fn exec_args(
        &self,
        container_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String>;

    /// Static name used by `ContainerRuntime::runtime_name`.
    fn name(&self) -> &'static str;
}
