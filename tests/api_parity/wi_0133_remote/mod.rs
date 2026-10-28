//! Remote API session tests (WI 0133): base ref, merge refs, push-back, and
//! dynamic workflows in a remote session's clone.
//!
//! units.rs  — `push_branch` translation and session-request validation,
//!             through the public Layer 1/2 functions. No git, no network.
//! setup.rs  — session setup against an origin served by `git daemon`:
//!             branch creation from `base_ref`, `merge_refs`, their failures,
//!             commit identity, restore after a server restart.
//! push.rs   — the push stage after `exec prompt` / `exec workflow --dynamic`
//!             against that origin, and the `push_branch` step's real git
//!             semantics.
//!
//! The setup and push tests run the real `awman api start` binary with a fake
//! `docker` (see `helpers/api_process.rs`), so no container runtime is used.
//! They carry `real_git` in their names so `make test-fast` skips them; the
//! one that needs only loopback HTTP carries `real_network`.

#![cfg(unix)]

use crate::{api_process, git_daemon};

mod push;
mod setup;
mod units;
