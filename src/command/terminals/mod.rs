//! Typed terminal operations and policy (WI 0141, Layer 2).
//!
//! The hub (`engine::terminal_hub`) keeps screens and fans out frames; this
//! module decides everything about *who*: viewer admission by the owner's
//! rules, the exclusive input grant, remote-input gating, owner lookup for
//! `awman attach cmd_…`/`task_…`, and the `terminal_control` /
//! `terminal_opened` / `terminal_closed` events written to the owner's log.
//! Layer 3 (`frontend::api::terminals`, the attach frontends) moves frames
//! and never decides any of this.

mod operations;
mod policy;

pub use operations::{
    OwnerEventLog, OwnerResolver, TerminalOperations, TerminalRefusal, TerminalSession,
};
pub use policy::{AccessPolicy, InputGrants};
