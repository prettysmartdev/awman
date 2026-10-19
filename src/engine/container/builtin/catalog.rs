//! Refuse unknown catalog contracts before asking the SDK to run migrations.
//!
//! The marker file, its lock and the "unversioned catalog" rule live in
//! Layer 0 (`BuiltinState::check_runtime_marker`); the engine supplies the
//! protocol it speaks and turns a mismatch into its error.
use super::paths::BuiltinPaths;
use crate::data::builtin_state::RuntimeMarker;
use crate::engine::error::EngineError;

impl BuiltinPaths {
    /// Check that the state directory's catalog uses this worker protocol.
    pub fn check_catalog(&self) -> Result<(), EngineError> {
        match self.check_runtime_marker(super::naming::PROTOCOL)? {
            RuntimeMarker::Created | RuntimeMarker::Matches => Ok(()),
            RuntimeMarker::Mismatch { found } => Err(EngineError::WorkerProtocolMismatch {
                expected: super::naming::PROTOCOL.into(),
                found,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    #[test]
    fn incompatible_catalog_is_preserved_without_migration() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        let marker = paths.home.join("awman-runtime-version");
        std::fs::write(&marker, b"future-protocol").unwrap();
        assert!(matches!(
            paths.check_catalog(),
            Err(EngineError::WorkerProtocolMismatch { .. })
        ));
        assert_eq!(std::fs::read(&marker).unwrap(), b"future-protocol");
        assert_eq!(std::fs::read_dir(paths.home.join("db")).unwrap().count(), 0);
    }
    #[test]
    fn matching_catalog_reopens_and_released_locks_are_reusable() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        paths.check_catalog().unwrap();
        paths.check_catalog().unwrap();
        let guard = paths.lock().unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.home.join("awman.lock"))
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(guard);
        assert!(other.try_lock().is_ok());
    }
    #[test]
    fn unversioned_catalog_is_not_adopted() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        std::fs::write(paths.home.join("db/catalog.sqlite"), b"existing data").unwrap();
        assert!(paths.check_catalog().is_err());
        assert!(!paths.home.join("awman-runtime-version").exists());
    }
}

#[cfg(test)]
mod hardening_tests;
