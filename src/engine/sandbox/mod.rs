//! `engine::sandbox` — `SandboxRuntime`, the sandbox-class
//! `AgentRuntimeEngine` impl for microVM-per-session runtimes.
//!
//! The concrete sandbox drivers are `pub(super)`-style internals: callers
//! outside this module see only `SandboxRuntime` plus the option types it
//! consumes. The first driver, `DSbxBackend` (Docker Sandboxes), is stubbed
//! in this work item — every backend method returns
//! `EngineError::NotImplemented` until WI 0090 lands the real
//! implementation.

mod backend;
mod dsbx;
pub mod naming;
pub mod options;
pub mod runtime;

pub use naming::generate_sandbox_name;
pub use options::{ResolvedSandboxOptions, SandboxOption};
pub use runtime::SandboxRuntime;
