//! Layer 1: engine
//!
//! Built on top of Layer 0 (`src/data/`). Exposes typed objects that own
//! every concern Layer 2 commands need to compose: container runtime,
//! workflow execution, git operations, overlays, auth, agent management,
//! and the multi-phase `ready`/`init` engines.
//!
//! No upward calls. When an engine needs user I/O, it accepts a frontend
//! trait *defined here* and Layer 3 implements it.

pub mod acp;
pub mod agent;
pub mod agent_runtime;
pub mod aspec;
pub mod auth;
pub mod container;
pub mod context_prompt;
pub mod credential_refresh;
pub mod daemon;
pub mod error;
pub mod git;
pub mod init;
pub mod issue;
pub mod overlay;
pub mod ready;
pub mod remote;
pub mod sandbox;
pub mod squad;
pub mod workflow;

pub use error::EngineError;

/// The one lock that owns the process-wide `PATH` during tests.
///
/// `PATH` is process-global, so a test that prepends a directory of fake
/// binaries mutates state every other test thread can see. Two locks over one
/// variable is the same as no lock: the `dsbx` fake-`sbx` tests and the
/// `poll_ci` fake-`gh` tests each used to hold a mutex of their own, so they
/// could interleave — and whichever restored its *saved* `PATH` last wiped the
/// other's directory back off it. The victim then failed looking for a binary
/// it had just installed ("neither `gh` CLI (authenticated) nor GITHUB_TOKEN
/// env var is available"). It reproduces whenever the suite runs wide enough
/// for the two families to overlap, which is the norm in a container: libtest
/// sizes its thread pool from `available_parallelism`, and an uncapped
/// container reports the *host's* core count.
///
/// Every test that mutates `PATH` goes through [`test_path::PathGuard`].
#[cfg(test)]
pub(crate) mod test_path {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    /// Exclusive ownership of the process-wide `PATH` for the guard's
    /// lifetime, restoring the original value on drop.
    ///
    /// Restoring in `Drop` rather than at the end of the test body is what
    /// makes a panicking test safe: it cannot leak its fake-binary directory
    /// onto the `PATH` of whichever test runs next.
    pub(crate) struct PathGuard {
        /// Held for the guard's lifetime to keep the lock; never read.
        _lock: MutexGuard<'static, ()>,
        original: String,
    }

    impl PathGuard {
        /// Take the lock, leaving `PATH` as it is.
        ///
        /// For tests that only need to be sure no sibling's fake binary is on
        /// `PATH` while they run.
        pub(crate) fn acquire() -> Self {
            // Recover a poisoned lock rather than propagating the poison: if
            // one test panics while holding it, that panic is already that
            // test's failure — it must not cascade a `PoisonError` into every
            // other PATH test and bury the real cause.
            let _lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let original = std::env::var("PATH").unwrap_or_default();
            Self { _lock, original }
        }

        /// Take the lock and put `dir` first on `PATH`.
        pub(crate) fn prepending(dir: &Path) -> Self {
            let guard = Self::acquire();
            std::env::set_var("PATH", format!("{}:{}", dir.display(), guard.original));
            guard
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            std::env::set_var("PATH", &self.original);
        }
    }
}
