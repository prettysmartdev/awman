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
