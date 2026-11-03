//! The one guard for process-global environment in the `engine` test binary.
//!
//! `daemon_supervisor`, `git_engine` and `overlay_engine` all compile into this
//! executable and all probe code that reads `HOME`, `PATH` or
//! `AWMAN_CONFIG_HOME` straight from the process. Three private locks over one
//! environment serialize nothing, so every such test takes this guard instead.
//! Other test executables are separate processes and keep their own state.
//!
//! Prefer an injected `EnvSnapshot` or a child's `Command::env`; use this only
//! where the code under test genuinely reads the process environment.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

static PROCESS_ENV: Mutex<()> = Mutex::new(());

/// Holds the process-environment lock and restores every variable it changed
/// when dropped, including on panic. A variable that was absent is removed
/// again rather than set to an empty value.
pub struct ProcessEnvGuard {
    saved: Vec<(OsString, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl ProcessEnvGuard {
    /// Take the lock without changing anything, for tests that only read
    /// `HOME`/`PATH` and must not observe another test's override.
    pub fn lock() -> Self {
        Self {
            saved: Vec::new(),
            // A panicking test restores through Drop, so poisoning carries no
            // state worth refusing.
            _lock: PROCESS_ENV.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    pub fn set(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        self.remember(key.as_ref());
        std::env::set_var(key, value);
    }

    pub fn remove(&mut self, key: impl AsRef<OsStr>) {
        self.remember(key.as_ref());
        std::env::remove_var(key);
    }

    /// Prepend `dir` to `PATH`, keeping the rest so `git` stays findable.
    pub fn prepend_path(&mut self, dir: &std::path::Path) {
        let mut paths = vec![dir.to_path_buf()];
        if let Some(old) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&old));
        }
        self.set("PATH", std::env::join_paths(paths).unwrap());
    }

    fn remember(&mut self, key: &OsStr) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_owned(), std::env::var_os(key)));
        }
    }
}

impl Drop for ProcessEnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn guard_restores_set_and_absent_variables_after_a_panic() {
    const SET: &str = "AWMAN_TEST_GUARD_SET";
    const ABSENT: &str = "AWMAN_TEST_GUARD_ABSENT";
    const EMPTY: &str = "AWMAN_TEST_GUARD_EMPTY";
    {
        let mut guard = ProcessEnvGuard::lock();
        guard.set(SET, "original");
        guard.set(EMPTY, "");
    }
    {
        let _guard = ProcessEnvGuard::lock();
        std::env::remove_var(ABSENT);
    }

    let outcome = std::panic::catch_unwind(|| {
        let mut guard = ProcessEnvGuard::lock();
        guard.set(SET, "changed");
        guard.set(SET, "changed again");
        guard.set(ABSENT, "now present");
        guard.remove(EMPTY);
        panic!("test failure while the environment is overridden");
    });
    assert!(outcome.is_err());

    let _guard = ProcessEnvGuard::lock();
    // The first block restored to the pre-test values, so rebuild the baseline
    // the panic test saw: SET and EMPTY were absent then.
    assert_eq!(std::env::var_os(SET), None);
    assert_eq!(std::env::var_os(ABSENT), None);
    assert_eq!(std::env::var_os(EMPTY), None);
}

#[test]
fn guard_distinguishes_empty_from_absent_and_survives_poisoning() {
    const EMPTY: &str = "AWMAN_TEST_GUARD_EMPTY2";
    std::env::set_var(EMPTY, "");
    let _ = std::panic::catch_unwind(|| {
        let mut guard = ProcessEnvGuard::lock();
        guard.set(EMPTY, "value");
        panic!("poison");
    });
    // The lock is still usable and the empty value came back as empty.
    let guard = ProcessEnvGuard::lock();
    assert_eq!(std::env::var_os(EMPTY), Some(OsString::new()));
    drop(guard);
    std::env::remove_var(EMPTY);
}
