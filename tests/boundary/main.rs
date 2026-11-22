//! `boundary` tier (WI 0144 design 8): one bounded proof per real boundary
//! (Docker Engine, registry, git transports, Apple Containers, sbx) plus the
//! shared conformance suite against the real implementations. Budget: 120 s.
//!
//! Real host resources are used only when named in
//! `AWMAN_TEST_HOST_RESOURCES`; a bare `cargo test` blocks these tests
//! (`BLOCKED[<capability>]`) before touching anything real.
//!
//! The module list is final; `soak` holds explicitly tier-`soak` cases.

mod support;

mod apple;
mod conformance;
mod docker;
mod git;
mod registry;
mod sbx;
mod soak;
mod templates;
