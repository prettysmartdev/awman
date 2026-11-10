//! `<data>/daemon/browser_grants.json` — browser grant metadata (WI 0159).
//!
//! Same private-file rules as `api_keys.json`: owner-only (`0600`), owned by
//! this user, not a symlink, replaced atomically. The file holds SHA-256
//! hashes of grant tokens, never the tokens. An unsafe or malformed file is
//! an error, never "no grants".

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::browser_auth::BrowserScope;
use crate::data::error::DataError;
use crate::data::fs::private_files::{
    create_private_dir, read_private_json, write_private_json_atomic,
};
use crate::data::ids::BrowserGrantId;

/// The only file format version this build reads and writes.
pub const BROWSER_GRANT_FILE_VERSION: u32 = 1;

/// Longest stored `label`.
pub const MAX_LABEL_CHARS: usize = 64;

/// One grant. `hash` is the lowercase hex SHA-256 of the bearer token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserGrantRecord {
    pub id: BrowserGrantId,
    pub hash: String,
    pub origin: String,
    pub listener: String,
    pub scope: BrowserScope,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The whole grant file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserGrantFile {
    pub version: u32,
    #[serde(default)]
    pub grants: Vec<BrowserGrantRecord>,
}

impl Default for BrowserGrantFile {
    fn default() -> Self {
        Self {
            version: BROWSER_GRANT_FILE_VERSION,
            grants: Vec::new(),
        }
    }
}

impl BrowserGrantFile {
    /// Read the grant file. Absent → empty. A symlink, a file other users can
    /// read, a foreign owner, an unknown version, a malformed hash or label,
    /// or unparsable JSON is an error.
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let Some(file) = read_private_json::<BrowserGrantFile>(path)? else {
            return Ok(Self::default());
        };
        file.validate(path)?;
        Ok(file)
    }

    /// Replace the file atomically, owner-only. The parent directory is
    /// created `0700` when missing and refused when other users can reach it.
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
        if self.version != BROWSER_GRANT_FILE_VERSION {
            return Err(invalid(format!(
                "unsupported browser grant file version {} (expected {BROWSER_GRANT_FILE_VERSION})",
                self.version
            )));
        }
        for record in &self.grants {
            if !is_sha256_hex(&record.hash) {
                return Err(invalid(format!(
                    "grant {} has a malformed hash (expected 64 lowercase hex characters)",
                    record.id
                )));
            }
            if record.label.as_ref().is_some_and(|l| {
                l.chars().count() > MAX_LABEL_CHARS || l.chars().any(char::is_control)
            }) {
                return Err(invalid(format!(
                    "grant {} has a malformed label",
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

    fn record() -> BrowserGrantRecord {
        BrowserGrantRecord {
            id: BrowserGrantId::new(),
            hash: "cd".repeat(32),
            origin: "https://awman.prettysmart.dev".into(),
            listener: "loopback".into(),
            scope: BrowserScope::Engine,
            created_at: Utc::now(),
            expires_at: Utc::now(),
            last_used_at: None,
            label: Some("Firefox".into()),
        }
    }

    #[test]
    fn absent_file_loads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let file = BrowserGrantFile::load(&tmp.path().join("d").join("g.json")).unwrap();
        assert_eq!(file, BrowserGrantFile::default());
    }

    #[test]
    fn store_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon").join("browser_grants.json");
        let file = BrowserGrantFile {
            version: 1,
            grants: vec![record(), record()],
        };
        file.store(&path).unwrap();
        assert_eq!(BrowserGrantFile::load(&path).unwrap(), file);
    }

    #[cfg(unix)]
    #[test]
    fn store_writes_mode_0600_and_refuses_group_readable_or_symlinked_files() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon").join("browser_grants.json");
        BrowserGrantFile::default().store(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let link = tmp.path().join("daemon").join("linked.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(BrowserGrantFile::load(&link).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(BrowserGrantFile::load(&path).is_err());
    }

    #[test]
    fn malformed_json_version_hash_and_label_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("browser_grants.json");
        BrowserGrantFile::default().store(&path).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(BrowserGrantFile::load(&path).is_err());

        let future = BrowserGrantFile {
            version: 2,
            grants: vec![],
        };
        assert!(future.store(&path).is_err());
        let mut bad = record();
        bad.hash = "nope".into();
        assert!(BrowserGrantFile {
            version: 1,
            grants: vec![bad]
        }
        .store(&path)
        .is_err());
        let mut bad = record();
        bad.label = Some("a\u{1b}[31m".into());
        assert!(BrowserGrantFile {
            version: 1,
            grants: vec![bad]
        }
        .store(&path)
        .is_err());
    }

    #[test]
    fn the_file_never_contains_a_token_field() {
        let value = serde_json::to_value(record()).unwrap();
        assert!(value.get("token").is_none());
    }
}
