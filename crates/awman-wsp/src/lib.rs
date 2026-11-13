//! Work Server Protocol v1. This crate contains wire types and an outbound
//! client; worker policy and execution remain in awman's command layer.

pub mod affinity;
pub mod client;
pub mod context;
pub use context::*;
pub mod conformance;
pub mod labels;
#[cfg(feature = "mock")]
pub mod mock;
pub mod mock_worker;
pub mod terminal;
pub mod types;
// Vendored 0139 helpers; tests guard byte identity with awman's copy.
pub mod wire;

pub use affinity::{Affinity, Match, MatchOp, Preferred};
pub use client::{Client, Error};
pub use types::*;

pub mod repo_url;
