//! Layer 0 cross-module integration tests (WI 0073).
//!
//! Hermetic — no Docker, no git daemon, no network. Uses tempfile for all
//! filesystem operations. Every test here MUST pass under `make test-fast`.

#[path = "../helpers/mod.rs"]
mod helpers;

mod home_layout;
mod key_table_docs_golden;

mod builtin_network;
mod config_session_roundtrip;
mod daemon_primitives;
mod db_migration;
mod docs_links;
mod sqlite_upgrade_compat;
mod squad_paths;
mod task_store;
