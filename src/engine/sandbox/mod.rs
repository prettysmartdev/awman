//! `engine::sandbox` — `SandboxRuntime`, the sandbox-class
//! `AgentRuntimeEngine` impl for microVM-per-session runtimes.
//!
//! The concrete sandbox drivers are `pub(super)`-style internals: callers
//! outside this module see only `SandboxRuntime`, the option types it
//! consumes, and `SandboxRuntime::ready_agent`, the entry point the ready flow
//! drives. The first driver, `DSbxBackend` (Docker Sandboxes), is implemented
//! in WI 0090: kit emission, lifecycle, credential injection, session config.

mod backend;
mod dsbx;
pub mod naming;
pub mod options;
pub mod runtime;

pub use naming::{generate_sandbox_name, sandbox_name_for};
pub use options::{ResolvedSandboxOptions, SandboxOption};
pub use runtime::SandboxRuntime;
