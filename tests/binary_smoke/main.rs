//! Binary-level smoke tests (WI 0073).
//!
//! These tests invoke the real `amux` binary as a subprocess and verify
//! exit codes, stdout shapes, and basic CLI behaviour.
//!
//! All tests here run under `make test-fast` because they don't need Docker.
//! Tests that need a real server or real git include those keywords.

#[path = "../helpers/mod.rs"]
mod helpers;

mod cli_subprocess;
