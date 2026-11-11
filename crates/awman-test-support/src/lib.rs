//! Test infrastructure for awman (WI 0144).
//!
//! Everything a test needs in order to be isolated by construction:
//!
//! - [`TestSandbox`]: a private root per test with its own HOME, XDG
//!   directories, awman data root, temp dir, short socket directory and a
//!   complete child environment ([`EnvMap`]) built from nothing.
//! - The process supervisor ([`OwnedCommand`], [`OwnedChild`], [`PtyCommand`]):
//!   own process group (job object on Windows), empty environment plus the
//!   sandbox allowlist, readiness handshake, deadline, kill-and-reap on drop,
//!   panic and timeout, and a leak check when the test ends.
//! - Owned network endpoints ([`Ports`]): listeners are bound to port 0 and
//!   handed over; nothing returns a "free port number".
//! - Typed capabilities ([`TestSandbox::require`]): a capability the test's
//!   tier guarantees but the host lacks fails the test as `BLOCKED[...]`;
//!   it is never skipped.
//! - Event-based waits ([`wait::wait_for`]) with diagnostics on expiry.
//! - Deterministic fixture builders ([`fixtures`]), verified fakes ([`fakes`],
//!   [`conformance`]), golden files ([`golden`]) and failure evidence
//!   ([`evidence`]).
//!
//! Two rules hold for everything here:
//!
//! - It never depends on `awman`, `awman-cli` or `awman-host-shims`, so the
//!   library's inline unit tests can use it. Glue that needs awman types lives
//!   in each tier binary's `support` module.
//! - It is the only code that reads the test process environment, and only
//!   through [`runtime_env`]. Nothing here sets process environment variables
//!   or changes the current directory.
//!
//! See `handoff/build-test-support-crate.md` in the WI 0144 workflow context
//! for a worked example.

// Unsafe code is allowed only where an operating-system API leaves no safe
// alternative; each such module opts in explicitly with a comment per block.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
mod config;
pub mod conformance;
pub mod env;
pub mod evidence;
pub mod exe;
pub mod fakes;
pub mod fixtures;
pub mod golden;
mod http;
pub mod leak;
pub mod net;
pub mod process;
pub mod pty;
pub mod runtime_env;
pub mod sandbox;
mod sys;
pub mod tier;
pub mod wait;

pub use capability::{Capability, Granted, ProbeContext};
pub use env::EnvMap;
pub use leak::Leak;
pub use net::Ports;
pub use process::{Exit, Output, OwnedChild, OwnedCommand, Ready, Stdin};
pub use pty::{PtyChild, PtyCommand};
pub use sandbox::TestSandbox;
pub use tier::{TestCase, Tier};
pub use wait::{wait_for, wait_for_async, wait_for_with};

/// The text of a caught panic (`&str` or `String` payload).
#[cfg(test)]
pub(crate) fn panic_text(err: &Box<dyn std::any::Any + Send>) -> String {
    err.downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}
