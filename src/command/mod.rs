//! Layer 2: command + dispatch.
//!
//! Built on top of Layer 0 (`src/data/`) and Layer 1 (`src/engine/`). Owns
//! the canonical command catalogue, the typed dispatch system, and one
//! `*Command` struct per amux command. No upward calls into Layer 3
//! (frontends) or Layer 4 (the binary entrypoints).

pub mod commands;
pub mod dispatch;
pub mod error;

pub use dispatch::catalogue::{CommandCatalogue, CommandSpec, FlagSpec, FrontendVisibility};
pub use dispatch::{
    BuiltCommand, CommandFrontend, CommandOutcome, Dispatch, DispatchFrontend, Engines,
    ParsedCommandBoxInput,
};
pub use error::CommandError;
