//! `<data>/daemon/api_keys.json` — the network listener's named API keys.
//!
//! The file holds only SHA-256 hashes of keys, never key material. It is a
//! private record (mode `0600`, owned by this user, not a symlink, single
//! link) replaced atomically on every write. A file that is unsafe or
//! malformed is an error, never "no keys": a daemon that silently fell back
//! to an empty store would hide a tampered file behind a 401 storm, and one
//! that fell back to anything else would fail open.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::private_files::{
    create_private_dir, read_private_json, write_private_json_atomic,
};
use crate::data::ids::KeyId;

/// The only file format version this build reads and writes.
pub const API_KEY_FILE_VERSION: u32 = 1;

/// One named key. `hash` is the lowercase hex SHA-256 of the plaintext key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: KeyId,
    pub name: String,
    pub hash: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
}

/// The whole key file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyFile {
    pub version: u32,
    #[serde(default)]
    pub keys: Vec<ApiKeyRecord>,
}

impl Default for ApiKeyFile {
    fn default() -> Self {
        Self {
            version: API_KEY_FILE_VERSION,
            keys: Vec::new(),
        }
    }
}

impl ApiKeyFile {
    /// Read the key file. Absent → an empty store. A symlink, a file other
    /// users can read, a foreign owner, an unknown version, a hash that is not
    /// 64 lowercase hex characters, or unparsable JSON is an error.
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let Some(file) = read_private_json::<ApiKeyFile>(path)? else {
            return Ok(Self::default());
        };
        file.validate(path)?;
        Ok(file)
    }

    /// Replace the key file atomically, owner-only (`0600`). The parent
    /// directory is created owner-only (`0700`) when missing and refused when
    /// it is accessible to other users.
    pub fn store(&self, path: &Path) -> Result<(), DataError> {
        self.validate(path)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }
        write_private_json_atomic(path, self, || Ok(()))
    }

    fn validate(&self, path: &Path) -> Result<(), DataError> {
        let invalid = |reason: String| DataError::InvalidPath {
            path: path.to_path_buf(),
            reason,
        };
        if self.version != API_KEY_FILE_VERSION {
            return Err(invalid(format!(
                "unsupported API key file version {} (expected {API_KEY_FILE_VERSION})",
                self.version
            )));
        }
        for record in &self.keys {
            if !is_sha256_hex(&record.hash) {
                return Err(invalid(format!(
                    "key {} has a malformed hash (expected 64 lowercase hex characters)",
                    record.id
                )));
            }
        }
        Ok(())
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str) -> ApiKeyRecord {
        ApiKeyRecord {
            id: KeyId::new(),
            name: name.to_string(),
            hash: "ab".repeat(32),
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    #[test]
    fn absent_file_loads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let file = ApiKeyFile::load(&tmp.path().join("d").join("api_keys.json")).unwrap();
        assert_eq!(file, ApiKeyFile::default());
        assert_eq!(file.version, 1);
    }

    #[test]
    fn store_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon").join("api_keys.json");
        let file = ApiKeyFile {
            version: 1,
            keys: vec![record("ci"), record("laptop")],
        };
        file.store(&path).unwrap();
        assert_eq!(ApiKeyFile::load(&path).unwrap(), file);
    }

    #[cfg(unix)]
    #[test]
    fn store_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon").join("api_keys.json");
        ApiKeyFile::default().store(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("api_keys.json");
        ApiKeyFile::default().store(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ApiKeyFile::load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("d").join("real.json");
        ApiKeyFile::default().store(&real).unwrap();
        let link = tmp.path().join("d").join("api_keys.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ApiKeyFile::load(&link).is_err());
    }

    #[test]
    fn malformed_json_is_an_error_not_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("api_keys.json");
        ApiKeyFile::default().store(&path).unwrap();
        // Overwrite in place, keeping the private mode.
        std::fs::write(&path, b"{not json").unwrap();
        assert!(ApiKeyFile::load(&path).is_err());
    }

    #[test]
    fn unknown_version_and_bad_hash_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("api_keys.json");
        let future = ApiKeyFile {
            version: 2,
            keys: vec![],
        };
        assert!(future.store(&path).is_err());

        let mut bad = record("ci");
        bad.hash = "not-a-hash".into();
        let file = ApiKeyFile {
            version: 1,
            keys: vec![bad],
        };
        assert!(file.store(&path).is_err());
    }
}
