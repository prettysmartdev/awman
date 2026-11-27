//! Per-sandbox launch lock (contract §6.5).
//!
//! Every awman launch into one deterministic sandbox name takes an exclusive,
//! non-blocking lock on `<data home>/sbx/locks/<name>.lock` before it looks
//! at the sandbox, and holds it until the agent exits. Because the name embeds
//! the workspace hash and the agent, this serialises exactly the launches that
//! share guest state, so one launch can never overwrite (or consume) the
//! managed policy files another launch just applied.
//!
//! The lock is an OS file lock (`File::try_lock`): it is released when the
//! process dies, so a crashed awman never leaves a sandbox locked. The file's
//! content (the holder's pid) is diagnostic only.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::engine::error::EngineError;

/// Directory, under the awman data home, of all SBX launch locks.
pub(super) fn locks_dir(data_home: &Path) -> PathBuf {
    data_home.join("sbx").join("locks")
}

/// A held launch lock. Dropping it releases the lock.
#[derive(Debug)]
pub(super) struct SandboxLock {
    _file: std::fs::File,
}

impl SandboxLock {
    /// Take the lock for `sandbox`, or fail with `SbxSandboxBusy` when
    /// another awman session holds it. Never waits.
    pub(super) fn acquire(data_home: &Path, sandbox: &str) -> Result<Self, EngineError> {
        let dir = locks_dir(data_home);
        create_private_dir(&dir)?;
        let path = dir.join(format!("{sandbox}.lock"));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| EngineError::io(&path, e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(EngineError::SbxSandboxBusy {
                    sandbox: sandbox.to_string(),
                })
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(EngineError::io(&path, e)),
        }
        // Diagnostic only; a failure to record the pid does not matter.
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Self { _file: file })
    }
}

/// Create `dir` (and parents) with owner-only permissions on Unix.
pub(super) fn create_private_dir(dir: &Path) -> Result<(), EngineError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|e| EngineError::io(dir, e))
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn a_second_launch_into_the_same_sandbox_is_refused_until_the_first_ends() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(
            "engine::sandbox::dsbx::lock::lock_tests::a_second_launch_into_the_same_sandbox_is_refused_until_the_first_ends",
        ));
        let home = sandbox.awman_home();
        let first = SandboxLock::acquire(&home, "awman-1-claude").unwrap();
        assert!(locks_dir(&home).join("awman-1-claude.lock").is_file());
        let busy = SandboxLock::acquire(&home, "awman-1-claude").unwrap_err();
        assert!(
            matches!(&busy, EngineError::SbxSandboxBusy { sandbox } if sandbox == "awman-1-claude"),
            "{busy:?}"
        );
        assert!(busy
            .to_string()
            .contains("already in use by another awman session"));
        // A different sandbox (other agent or workspace) is independent.
        let other = SandboxLock::acquire(&home, "awman-1-codex").unwrap();
        drop(first);
        let again = SandboxLock::acquire(&home, "awman-1-claude").unwrap();
        drop((again, other));
        sandbox.finish();
    }
}
