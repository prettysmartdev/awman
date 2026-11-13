//! Component tests: Layer 1 engines and transports (owner: slice-layer1-engine).
//!
//! Every test builds its sandbox with `crate::support::sandbox("engine::<module>::<test>")`.
//! `conformance` runs `awman::engine::conformance` (the shared runtime suite) against the
//! verified fakes modelled in `fake_world`; the boundary tier runs the same suite against
//! the real implementations.

mod acp;
mod agent;
mod agent_runtime;
mod auth;
mod conformance;
mod container;
mod credential_refresh;
mod credentials;
mod fake_world;
mod git;
mod git_credentials;
mod git_push_retry;
mod git_transport;
mod host_cli;
mod image_prep;
mod issue;
mod network;
mod oci;
mod oci_doubles;
mod overlay;
mod parity_engine;
mod process;
mod ready;
mod regressions;
mod remote;
mod sandbox;
mod terminal;
mod workflow;
