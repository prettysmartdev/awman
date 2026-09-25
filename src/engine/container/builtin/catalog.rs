//! Refuse unknown catalog contracts before asking the SDK to run migrations.
use super::{naming::PROTOCOL, paths::BuiltinPaths};
use crate::engine::error::EngineError;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
};
pub fn check(paths: &BuiltinPaths) -> Result<(), EngineError> {
    let _lock = paths.lock()?;
    let path = paths.home.join("awman-runtime-version");
    if !path.exists()
        && std::fs::read_dir(paths.home.join("db"))
            .map_err(|e| EngineError::io(paths.home.join("db"), e))?
            .next()
            .is_some()
    {
        return Err(EngineError::WorkerProtocolMismatch {
            expected: PROTOCOL.into(),
            found: "unversioned existing catalog".into(),
        });
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(PROTOCOL.as_bytes())
                .map_err(|e| EngineError::io(&path, e))?;
            file.sync_all().map_err(|e| EngineError::io(&path, e))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut found = String::new();
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|e| EngineError::io(&path, e))?
                .take(128)
                .read_to_string(&mut found)
                .map_err(|e| EngineError::io(&path, e))?;
            if found != PROTOCOL {
                return Err(EngineError::WorkerProtocolMismatch {
                    expected: PROTOCOL.into(),
                    found,
                });
            }
        }
        Err(e) => return Err(EngineError::io(&path, e)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incompatible_catalog_is_preserved_without_migration() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        let marker = paths.home.join("awman-runtime-version");
        std::fs::write(&marker, b"future-protocol").unwrap();
        assert!(matches!(
            check(&paths),
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
        check(&paths).unwrap();
        check(&paths).unwrap();
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
        assert!(check(&paths).is_err());
        assert!(!paths.home.join("awman-runtime-version").exists());
    }
}

#[cfg(test)]
mod hardening_tests;
