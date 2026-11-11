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

impl Drop for ImageBuildLease {
    fn drop(&mut self) {
        // A fork can inherit this open file description. Closing our handle
        // alone would leave its flock held until the child closes its copy.
        let _ = self._file.unlock();
    }
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

    #[test]
    /// Guards: e6a2d429 lock ownership is released even when a descriptor is duplicated.
    fn duplicated_lock_descriptor_does_not_keep_the_lease_owned() {
        let dir = tempfile::tempdir().unwrap();
        let locks = ImageBuildLockStore::new(dir.path().join("locks"));
        let store = BuildStoreKey::AppleLocal;
        let held = locks.try_lock(&store, "agent:latest").unwrap().unwrap();
        let duplicated = held._file.try_clone().unwrap();
        assert!(locks.try_lock(&store, "agent:latest").unwrap().is_none());
        drop(held);
        assert!(locks.try_lock(&store, "agent:latest").unwrap().is_some());
        drop(duplicated);
    }

    #[test]
    /// Guards: e6322996 independent image outputs never share a build lock.
    fn image_lock_identity_separates_store_and_reference() {
        let dir = tempfile::tempdir().unwrap();
        let locks = ImageBuildLockStore::new(dir.path().join("locks"));
        let apple = BuildStoreKey::AppleLocal;
        let docker = BuildStoreKey::Docker {
            endpoint: "unix:///run/docker.sock".into(),
        };
        let lease = locks.try_lock(&apple, "agent:latest").unwrap().unwrap();
        assert!(locks.try_lock(&apple, "agent:latest").unwrap().is_none());
        assert!(locks.try_lock(&apple, "other:latest").unwrap().is_some());
        assert!(locks.try_lock(&docker, "agent:latest").unwrap().is_some());
        drop(lease);
    }

    #[test]
    /// Guards: f5b724fc lock directories and files remain owner private.
    fn lock_directory_is_private_and_lease_releases_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("locks");
        let locks = ImageBuildLockStore::new(&root);
        let lease = locks
            .try_lock(&BuildStoreKey::AppleLocal, "agent:latest")
            .unwrap()
            .unwrap();
        let metadata = std::fs::symlink_metadata(&root).unwrap();
        assert!(metadata.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
        drop(lease);
        assert!(locks
            .try_lock(&BuildStoreKey::AppleLocal, "agent:latest")
            .unwrap()
            .is_some());
    }
}
