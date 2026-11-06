//! The one guard for process-global environment in the `engine` test binary.
//!
//! `daemon_supervisor`, `git_engine` and `overlay_engine` all compile into this
//! executable and all probe code that reads `HOME`, `PATH` or
//! `AWMAN_HOME` straight from the process. Three private locks over one
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
        self.set("PATH", path_with_first(dir));
    }

    /// `dir` followed by this process's `PATH`, read under the guard, for a
    /// child's `Command::env("PATH", ..)`. Without the lock the read could
    /// observe another test's temporary `prepend_path` and hand the child
    /// that test's fake binaries. Must not be called while this thread
    /// already holds a guard (the lock is not reentrant); use
    /// [`path_with_first`] there instead.
    pub fn child_path_with_first(dir: &std::path::Path) -> OsString {
        let _guard = Self::lock();
        path_with_first(dir)
    }

    fn remember(&mut self, key: &OsStr) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_owned(), std::env::var_os(key)));
        }
    }
}

/// `dir` followed by the current `PATH`. The caller must hold the guard.
fn path_with_first(dir: &std::path::Path) -> OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(old) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&old));
    }
    std::env::join_paths(paths).unwrap()
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

/// A child's PATH read through the guard waits for another test's temporary
/// override to be restored, so it never inherits that test's fake binaries.
#[test]
fn a_guarded_child_path_never_sees_another_tests_override() {
    let fake = std::path::Path::new("/nonexistent/awman-fake-bin");
    let own = std::path::Path::new("/nonexistent/awman-own-bin");
    let (overridden_tx, overridden_rx) = std::sync::mpsc::channel::<()>();
    let (restore_tx, restore_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let mut guard = ProcessEnvGuard::lock();
        guard.prepend_path(fake);
        overridden_tx.send(()).unwrap();
        restore_rx.recv().unwrap();
        drop(guard);
    });
    overridden_rx.recv().unwrap();
    let reader = std::thread::spawn(move || ProcessEnvGuard::child_path_with_first(own));
    // Give the reader time to block on the lock, then restore PATH.
    std::thread::sleep(std::time::Duration::from_millis(50));
    restore_tx.send(()).unwrap();
    holder.join().unwrap();
    let path = reader.join().unwrap();
    let entries: Vec<_> = std::env::split_paths(&path).collect();
    assert_eq!(entries.first().map(|p| p.as_path()), Some(own));
    assert!(
        !entries.iter().any(|p| p == fake),
        "the child PATH included another test's temporary entry"
    );
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
