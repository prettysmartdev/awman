#![cfg(unix)]
//! Builtin (embedded Microsandbox) runtime tests, runtime half of WI 0119.
//!
//! Three tiers, all under one target so `cargo test --test builtin_runtime`
//! runs everything that can run on the current host:
//!
//! * hermetic tests (run in `make test-fast`, default build, no hypervisor,
//!   real HOME never touched): worker dispatch, `--version`, refusals, socket
//!   budgets, dependency-graph and artifact scans, gate wiring, the promoted
//!   fixture assertion inventory;
//! * `builtin_hw_*` tests, skipped by `make test-fast` and opt-in with
//!   `AWMAN_TEST_BUILTIN=1` (`make test-builtin`). They boot real guests and
//!   report SKIP/BLOCKED — never PASS — when KVM (Linux) or Hypervisor.framework
//!   support (macOS) is missing;
//! * the fake-driver lifecycle/compatibility tests live beside the code they
//!   cover (`src/engine/container/builtin/**`, feature `builtin-runtime`).

#[path = "../binary_smoke/awman_binary.rs"]
mod awman_binary;

mod acquisition;
mod apple_store;
mod binary;
mod fixture_inventory;
mod gate;
mod gates;
mod guest_compat;
mod hardware;
mod lifecycle;
mod measure;
mod network_resources;
mod runtime_refusal;
mod sqlite;
mod state_dir;
