//! Docker Sandbox driver (`sbx` CLI).
//!
//! WI 0090 implementation. The concrete driver and its companions
//! (kit emitter, credential injector, session-config writer, spawn helper,
//! I/O bridge) are all internal to `src/engine/sandbox/`. Callers outside the
//! sandbox module see only `SandboxRuntime`.

mod auth;
mod backend;
mod io_bridge;
mod kit;
mod ready;
mod session_config;
mod spawn;

pub(in crate::engine::sandbox) use backend::{run_interactive, DSbxBackend};
pub(in crate::engine::sandbox) use ready::ready_agent_in;
