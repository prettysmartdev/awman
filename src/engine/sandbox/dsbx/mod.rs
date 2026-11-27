//! Docker Sandbox driver (`sbx` CLI), v3 kits (work item 0086).
//!
//! The concrete driver and its companions are all internal to
//! `src/engine/sandbox/`; callers outside the sandbox module see only
//! `SandboxRuntime`.
//!
//! | Module | Role |
//! |---|---|
//! | `cli` | every `sbx` argv, CLI admission, tolerant output parsers |
//! | `spawn` | the single choke point that runs headless `sbx` calls |
//! | `kit` | emits the agent's v3 kit directory |
//! | `identity` | host record of what an awman sandbox was created from |
//! | `lock` | per-sandbox launch lock |
//! | `session_config` | per-launch policy document and session environment |
//! | `auth` | sandbox-scoped secret registration |
//! | `ready` | `awman ready` |
//! | `backend` | the launch sequence and the execution |
//! | `io_bridge` | PTY / piped I/O of `sbx run` |
//!
//! The shared contract (support matrix, kit manifest and digest, apply wire
//! format) is `crate::engine::agent::sbx_contract`.

mod auth;
mod backend;
mod cli;
mod identity;
mod io_bridge;
mod kit;
mod lock;
mod ready;
mod session_config;
mod spawn;

pub(in crate::engine::sandbox) use backend::{run_interactive, DSbxBackend};
pub(in crate::engine::sandbox) use ready::ready_agent_in;
