//! Layer 0 cross-module integration tests (WI 0073).
//!
//! Hermetic — no Docker, no git daemon, no network. Uses tempfile for all
//! filesystem operations. Every test here MUST pass under `make test-fast`.

#[path = "../helpers/mod.rs"]
mod helpers;

mod config_session_roundtrip;
mod sqlite_upgrade_compat;
