//! Layer 2: command + dispatch.
//!
//! Built on top of Layer 0 (`src/data/`) and Layer 1 (`src/engine/`). Owns
//! the canonical command catalogue, the typed dispatch system, and one
//! `*Command` struct per awman command. No upward calls into Layer 3
//! (frontends) or Layer 4 (the binary entrypoints).

pub mod commands;
pub mod daemon_api;
pub mod dispatch;
pub mod error;
pub mod git_summary;
pub mod headless;
pub mod host_output;
pub mod interaction;
pub mod program_status_projection;
pub mod prompts;
pub mod session_create;
pub mod session_setup;
pub mod startup;
pub mod stats_sampler;
pub mod terminals;

pub use dispatch::catalogue::{CommandCatalogue, CommandSpec, FlagSpec, FrontendVisibility};
pub use dispatch::{
    BuiltCommand, CommandFrontend, CommandOutcome, Dispatch, DispatchFrontend, Engines,
    ParsedCommandBoxInput,
};
pub use error::CommandError;
pub use session_create::{
    SessionCreatePlan, SessionCreatePolicy, SessionCreateRequest, DEFAULT_REPO_URL_SCHEMES,
};
pub use session_setup::{SessionSetup, SessionSetupPresenter};
pub use startup::{Startup, StartupOutcome};

pub mod daemon_runtime;
