//! Docker Sandbox driver (`sbx` CLI).
//!
//! WI 0090 implementation. The concrete driver and its companions
//! (kit emitter, credential injector, session-config writer, spawn helper,
//! I/O bridge) are all internal to `src/engine/sandbox/`. Callers outside the
//! sandbox module see only `SandboxRuntime`.

mod auth;
mod backend;
mod io_bridge;
mod kit;
mod ready;
mod session_config;
mod spawn;

pub(in crate::engine::sandbox) use backend::{run_interactive, DSbxBackend};
pub(in crate::engine::sandbox) use ready::ready_agent;

/// Fake-`sbx` PATH plumbing shared by the dsbx test modules. One lock
/// serialises every PATH mutation so parallel tests never see each other's
/// fake binary.
#[cfg(all(test, unix))]
pub(super) mod test_support {
    use std::sync::Mutex;

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    fn write_fake_sbx(dir: &std::path::Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("sbx");
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    /// Run `f` with a mock `sbx` script first on PATH.
    pub fn with_fake_sbx<F: FnOnce()>(script: &str, f: F) {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_sbx(tmp.path(), script);
        // Recover a poisoned lock instead of propagating the poison: if one
        // test panics while holding it, the panic itself is that test's
        // failure — it must not cascade a `PoisonError` into every other dsbx
        // test and bury the real cause.
        let _guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", tmp.path().display()));
        // Restore PATH even if `f()` panics, so the global mutation never leaks
        // into a later test; then re-raise so the panic is still the failure.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::env::set_var("PATH", orig);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
