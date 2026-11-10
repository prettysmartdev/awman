//! Layer 1 worker engine: Work Server Protocol transport and the execution
//! primitives pushed and pulled jobs share.
//!
//! - [`session::WspSession`]: register, queue, long-poll claims, result,
//!   release, credentials refresh, terminal socket; token refresh with
//!   revocation latched as a cancellation signal.
//! - [`heartbeat::HeartbeatLoop`]: `heartbeat_ms` (15 s default) heartbeats,
//!   `cancel: true` and `409 lease_lost` turned into run cancellation.
//! - [`events::EventForwarder`]: the ordered execution-event log posted to
//!   the server, idempotent by `(attempt, sequence)`.
//! - [`terminal_relay::TerminalRelay`]: `awman.terminal.v1` multiplexing of
//!   hub terminals by `terminal_id`, with the two-sided input gate.
//! - [`executor_share::WorkerExecutorShare`]: push/pull alternation inside
//!   the worker feature's executor share.
//! - [`lease_workspace`]: `<workRoot>/leases/<lease_id>/`, the private inline
//!   work-item source and runtime eligibility (`read_only`, outputs).
//! - [`credential_helper`]: the lease Git transport and credential refresh.
//! - [`push::LeasePush`]: bounded, cancellable lease push retry.
//!
//! This module never imports `crate::command` or `crate::frontend`; worker
//! policy (claim loops, admission, release decisions) is Layer 2.
pub mod credential_helper;
pub mod error;
pub mod events;
pub mod executor_share;
pub mod heartbeat;
pub mod lease_workspace;
pub mod push;
pub mod session;
pub mod terminal_relay;

pub use error::WorkerEngineError;
pub use session::{LeaseApi, LeaseRef, WspSession};
