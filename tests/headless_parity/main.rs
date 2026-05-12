//! Headless parity tests (WI 0073).
//!
//! routes.rs       — verifies the route table and headless paths.
//! auth_modes.rs   — auth-mode-related path/type smoke checks.
//! live_server.rs  — boots the real Axum router on an ephemeral port and
//!                   hits it with reqwest. Tests are prefixed `real_network_`
//!                   so `make test-fast` can skip them.
//!
//! SSE wire-format and WebSocket tests against a fully booted `amux headless
//! start` subprocess are deferred to WI 0076.

#[path = "../helpers/mod.rs"]
mod helpers;

mod auth_modes;
mod live_server;
mod routes;
