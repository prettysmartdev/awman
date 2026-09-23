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

/// Fake-`sbx` PATH plumbing shared by the dsbx test modules. The mutation goes
/// through [`crate::engine::test_path::PathGuard`], the single lock over the
/// process-wide `PATH`: a lock private to this module would serialise the dsbx
/// tests against each other only, leaving them free to clobber the fake `gh`
/// that the `poll_ci` tests install.
#[cfg(all(test, unix))]
pub(super) mod test_support {
    use crate::engine::test_path::PathGuard;

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
        // The guard restores PATH when it drops, so a panic inside `f` unwinds
        // straight through as that test's own failure without leaking the fake
        // `sbx` directory onto a later test's PATH.
        let _guard = PathGuard::prepending(tmp.path());
        f();
    }
}
