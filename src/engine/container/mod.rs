//! `engine::container` — `ContainerRuntime`, `ContainerInstance`,
//! `ContainerExecution`, and the typed `ContainerOption` enum.
//!
//! The Docker and Apple backends are `pub(super)` and their concrete types
//! are invisible to callers outside this module. All callers go through
//! `ContainerRuntime::build`.

mod apple;
mod backend;
mod docker;
pub mod frontend;
pub mod instance;
pub mod naming;
pub mod options;
pub mod runtime;

pub use frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
pub use instance::{
    ContainerExecution, ContainerExitInfo, ContainerId, ContainerInstance, ContainerStats,
};
pub use naming::generate_container_name;
pub use options::{
    AgentSettings, AutoMode, ContainerName, ContainerOption, CpuLimit, EnvLiteral, EnvVar,
    Entrypoint, ImageRef, MemoryLimit, ModelFlagForm, OverlayPermission, OverlaySpec, PlanMode,
    YoloMode,
};
pub use runtime::ContainerRuntime;

