//! Layer 1 engine integration tests (WI 0073).
//!
//! Tests that need Docker have "docker" in their name and are skipped by
//! `make test-fast` via `--skip docker`.
//! Tests that need real git have "real_git" in their name.

#[path = "../helpers/mod.rs"]
mod helpers;

mod container_docker;
mod git_engine;
mod overlay_engine;
mod workflow_end_to_end;
