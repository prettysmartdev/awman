//! Layer 1: engine
//!
//! Built on top of Layer 0 (`src/data/`). Exposes typed objects that own
//! every concern Layer 2 commands need to compose: container runtime,
//! workflow execution, git operations, overlays, auth, agent management,
//! and the multi-phase `ready`/`init` engines.
//!
//! No upward calls. When an engine needs user I/O, it accepts a frontend
//! trait *defined here* and Layer 3 implements it.

pub mod acp;
pub mod agent;
pub mod agent_output;
pub mod agent_runtime;
pub mod aspec;
pub mod auth;
pub mod conformance;
pub mod container;
pub mod context_prompt;
pub mod credential_refresh;
pub mod daemon;
pub mod error;
pub mod git;
pub mod host_cli;
pub mod init;
pub mod issue;
pub mod oci;
pub mod overlay;
pub mod ready;
pub mod remote;
pub mod sandbox;
pub mod squad;
pub mod terminal_hub;
pub mod worker;
pub mod workflow;

pub use error::EngineError;

pub mod image_prep;
