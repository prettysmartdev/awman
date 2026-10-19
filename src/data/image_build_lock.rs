//! Cross-process locks that serialize local image builds per output.
//!
//! Two awman processes building the same reference into the same store would
//! retag it under each other, so a build (and the export and import that
//! follow it) holds a lock keyed by the store and the output reference. The
//! lock root is per user and independent of any runtime's state directory,
//! so processes with different private caches still serialize on the same
//! store. Different stores or references never contend.
//!
//! This module owns only the lock files. Which locks to take, in what order,
//! and for how long is the engine's policy.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;
use crate::data::fs::private_files::{
    create_private_dir, lock_private_exclusive, lock_private_timeout, LockMode,
};

/// Subdirectory of the awman data home that holds the build locks.
pub const IMAGE_BUILD_LOCK_SUBDIR: &str = "locks/image-build";

/// The image store a local build writes into. Holds no credentials and no
/// source-kind spelling, so aliases of one source map to one store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BuildStoreKey {
    /// A Docker Engine, by its normalized endpoint (e.g.
    /// `unix:///var/run/docker.sock`).
    Docker { endpoint: String },
    /// The local Apple Containers image store.
    AppleLocal,
}

impl BuildStoreKey {
    /// Stable, non-secret text naming the store.
    pub fn describe(&self) -> String {
        match self {
            Self::Docker { endpoint } => format!("docker {endpoint}"),
            Self::AppleLocal => "apple-containers".to_string(),
        }
    }
}

impl std::fmt::Display for BuildStoreKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// The per-user directory of build lock files.
#[derive(Debug, Clone)]
pub struct ImageBuildLockStore {
    root: PathBuf,
}

impl ImageBuildLockStore {
    /// Locks under `root`. Tests inject a temporary root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Locks under `<data home>/`[`IMAGE_BUILD_LOCK_SUBDIR`].
    pub fn under_data_home(data_home: &Path) -> Self {
        Self::new(data_home.join(IMAGE_BUILD_LOCK_SUBDIR))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The lock file for `reference` in `store`.
    pub fn lock_path(&self, store: &BuildStoreKey, reference: &str) -> PathBuf {
        let key = format!("v1\n{}\n{reference}", store.describe());
        self.root.join(format!("{}.lock", sha256_hex(&key)))
    }

    /// Wait for and take the lock on `reference` in `store`. `check` runs
    /// before every attempt; returning an error from it abandons the wait.
    /// The lock is held until the returned lease is dropped.
    pub fn lock(
        &self,
        store: &BuildStoreKey,
        reference: &str,
        check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<ImageBuildLease, DataError> {
        create_private_dir(&self.root)?;
        let path = self.lock_path(store, reference);
        let file = lock_private_exclusive(&path, check)?;
        Ok(ImageBuildLease {
            store: store.clone(),
            reference: reference.to_string(),
            _file: file,
        })
    }

    /// Take the lock only if it is free right now: `Ok(None)` means another
    /// holder has it, so a caller can say it is waiting before [`Self::lock`].
    pub fn try_lock(
        &self,
        store: &BuildStoreKey,
        reference: &str,
    ) -> Result<Option<ImageBuildLease>, DataError> {
        create_private_dir(&self.root)?;
        let path = self.lock_path(store, reference);
        Ok(lock_private_timeout(
            &path,
            LockMode::Exclusive,
            std::time::Duration::ZERO,
            || Ok(()),
        )?
        .map(|file| ImageBuildLease {
            store: store.clone(),
            reference: reference.to_string(),
            _file: file,
        }))
    }
}

/// A held build lock. Released when dropped, including on error and
/// cancellation paths.
#[derive(Debug)]
pub struct ImageBuildLease {
    store: BuildStoreKey,
    reference: String,
    _file: File,
}

impl ImageBuildLease {
    pub fn store(&self) -> &BuildStoreKey {
        &self.store
    }

    pub fn reference(&self) -> &str {
        &self.reference
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_second_lease_waits_and_can_cancel_without_stealing_the_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let locks = ImageBuildLockStore::new(dir.path());
        let store = BuildStoreKey::AppleLocal;
        let first = locks.lock(&store, "agent:latest", || Ok(())).unwrap();
        let checked = Arc::new(AtomicUsize::new(0));
        let checked_in_waiter = checked.clone();
        let waiting = locks.clone();
        let waiter = std::thread::spawn(move || {
            waiting.lock(&store, "agent:latest", || {
                if checked_in_waiter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err(DataError::InvalidPath {
                        path: PathBuf::from("cancelled"),
                        reason: "cancelled".into(),
                    })
                }
            })
        });
        assert!(waiter.join().unwrap().is_err());
        assert!(checked.load(Ordering::SeqCst) >= 2);
        drop(first);
        assert!(locks
            .lock(&BuildStoreKey::AppleLocal, "agent:latest", || Ok(()))
            .is_ok());
    }

    #[test]
    fn try_lock_reports_a_held_lock_without_waiting() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let locks = ImageBuildLockStore::new(dir.path());
        let store = BuildStoreKey::AppleLocal;
        let held = locks
            .try_lock(&store, "agent:latest")
            .unwrap()
            .expect("free");
        assert!(locks.try_lock(&store, "agent:latest").unwrap().is_none());
        assert!(locks.try_lock(&store, "other:latest").unwrap().is_some());
        drop(held);
        assert!(locks.try_lock(&store, "agent:latest").unwrap().is_some());
    }
}
