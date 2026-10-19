//! `engine::container` — `ContainerRuntime`, the container-class
//! `AgentRuntimeEngine` impl, and the typed `ContainerOption` enum.
//!
//! The Docker and Apple backends are `pub(super)` and their concrete types
//! are invisible to callers outside this module. All callers go through
//! `ContainerRuntime::build`.
//!
//! This module re-exports only container-paradigm-specific items.
//! Cross-paradigm types (`AgentInstance`, `AgentExecution`, `AgentFrontend`,
//! `AgentHandle`, …) live in `src/engine/agent_runtime/`.

mod agent_argv;
mod apple;
pub(crate) mod attach_protocol;
mod attach_socket;
mod backend;
pub mod background;
mod builtin;
pub mod display;
mod docker;
mod host_cli_backend;
pub mod instance;
pub mod io_bridge;
pub mod naming;
pub mod options;
pub mod phase_step;
mod process;
pub mod runtime;
#[cfg(test)]
pub(crate) mod test_support;
pub mod timing;

pub use background::BackgroundContainer;
#[cfg(all(test, unix))]
pub(crate) use builtin::{FakeBuiltin, LaunchedVm};
pub use instance::ContainerId;
pub use naming::generate_container_name;
pub use options::{
    AgentSettings, AutoMode, ContainerName, ContainerOption, CpuLimit, Entrypoint, EnvLiteral,
    EnvVar, ImageRef, MemoryLimit, ModelFlagForm, OverlayPermission, OverlaySpec, PlanMode,
    YoloMode,
};
pub use phase_step::PhaseStepContainerSpec;
pub use runtime::ContainerRuntime;

/// Cancellation and deadline for one host build step. Part of the
/// `image_prep::builder::LocalImageBuilder` seam, so it is public; it carries
/// no way to run anything.
pub use host_cli_backend::BuildControl;

/// The host build helpers `image_prep::builder::HostLocalImageBuilder` drives
/// (WI 0125's source-matched builder). `host_cli_backend` itself stays private
/// to the CLI backends: this facade exposes only probe/inspect/build plumbing,
/// so agent execution is unreachable from the build handle.
pub(crate) mod host_build {
    pub(crate) use super::host_cli_backend::{
        tail, BuildArgv, HostCommandRunner, ProcessHostCommandRunner, PROBE_TIMEOUT,
    };
}

mod build_request;
pub use build_request::{BuildRequest, BuildTarget};
