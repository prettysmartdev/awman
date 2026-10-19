//! Receipts for images that were built locally but not yet installed.
//!
//! After a successful source-matched build, the engine records what the
//! build produced before it starts the import. Until an install of exactly
//! that content succeeds and removes the receipt, the next `awman ready`
//! must re-acquire from the source and check the content against it, rather
//! than reuse an older cached copy. The receipt lives on disk because the
//! process that built the image may not be the one that retries the import.
//!
//! This module owns only the record format and its private, atomic
//! persistence; when to write, enforce and clear a receipt is the engine's
//! policy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;
use crate::data::fs::private_files::{
    create_private_dir, read_private_json, remove_private_file, write_private_json_atomic,
};
use crate::data::image_build_lock::BuildStoreKey;
use crate::data::oci_identity::{BuiltImageIdentity, OciPlatform};

/// Subdirectory of awman's data home that holds pending receipts.
pub const PENDING_BUILDS_SUBDIR: &str = "pending-builds";

/// What a pending receipt is for: the image built at `reference` in `store`
/// for `platform`, to be installed as `install_tag`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PendingBuildKey {
    pub store: BuildStoreKey,
    pub reference: String,
    pub platform: OciPlatform,
    pub install_tag: String,
}

impl PendingBuildKey {
    fn file_name(&self) -> String {
        let key = format!(
            "v1\n{}\n{}\n{}\n{}",
            self.store.describe(),
            self.reference,
            self.platform,
            self.install_tag
        );
        format!("{}.json", sha256_hex(&key))
    }
}

/// A built image awaiting a verified install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingBuiltImage {
    pub key: PendingBuildKey,
    /// What the build produced; the imported content must match it.
    pub identity: BuiltImageIdentity,
}

/// The directory of pending receipts for one runtime.
#[derive(Debug, Clone)]
pub struct PendingBuiltImageStore {
    root: PathBuf,
}

impl PendingBuiltImageStore {
    /// Receipts under `root`. Tests inject a temporary root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Receipts under the supplied root and [`PENDING_BUILDS_SUBDIR`].
    pub fn under_data_home(data_home: &Path) -> Self {
        Self::new(data_home.join(PENDING_BUILDS_SUBDIR))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &PendingBuildKey) -> PathBuf {
        self.root.join(key.file_name())
    }

    /// The receipt for `key`, if one is pending. A receipt that is unsafe,
    /// unreadable, or recorded for a different key is an error, so a damaged
    /// receipt can never silently allow a stale image.
    pub fn load(&self, key: &PendingBuildKey) -> Result<Option<PendingBuiltImage>, DataError> {
        let path = self.path(key);
        let Some(record) = read_private_json::<PendingBuiltImage>(&path)? else {
            return Ok(None);
        };
        if record.key != *key {
            return Err(DataError::InvalidPath {
                path,
                reason: "pending build receipt belongs to a different image; remove it and retry"
                    .into(),
            });
        }
        Ok(Some(record))
    }

    /// Record `receipt`, replacing any earlier one for the same key. `check`
    /// runs just before the receipt is published; an error from it leaves
    /// the previous receipt in place.
    pub fn save(
        &self,
        receipt: &PendingBuiltImage,
        mut check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<(), DataError> {
        check()?;
        create_private_dir(&self.root)?;
        write_private_json_atomic(&self.path(&receipt.key), receipt, check)
    }

    /// Clear the receipt for `key` after a verified install. Absent is
    /// success.
    pub fn remove(&self, key: &PendingBuildKey) -> Result<(), DataError> {
        remove_private_file(&self.path(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::oci_identity::Digest;

    #[test]
    fn receipt_for_another_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = PendingBuiltImageStore::new(dir.path());
        let key = PendingBuildKey {
            store: BuildStoreKey::AppleLocal,
            reference: "agent:latest".into(),
            platform: OciPlatform::host_linux(),
            install_tag: "agent:latest".into(),
        };
        let receipt = PendingBuiltImage {
            key: key.clone(),
            identity: BuiltImageIdentity::new(
                None,
                Some(Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap()),
                key.platform.clone(),
            )
            .unwrap(),
        };
        store.save(&receipt, || Ok(())).unwrap();
        assert_eq!(store.load(&key).unwrap(), Some(receipt.clone()));
        let mut other = receipt;
        other.key.reference = "other:latest".into();
        std::fs::write(store.path(&key), serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(store.load(&key).is_err());
    }
}
