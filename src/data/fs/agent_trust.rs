//! Per-content approvals for repository definitions and global host pings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::private_files::{
    create_private_dir, read_private_approval_json as read_private_json_or_miss,
    write_private_json_atomic,
};

pub const TRUST_STORE_FILENAME: &str = "trusted-agents.json";
pub const TRUST_STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scope", rename_all = "lowercase")]
pub enum TrustEntry {
    Repo {
        git_root: PathBuf,
        name: String,
        sha256: String,
        approved_at: String,
    },
    Global {
        path: PathBuf,
        name: String,
        sha256: String,
        approved_at: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustStoreFile {
    pub version: u32,
    pub entries: Vec<TrustEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustKey {
    Repo { git_root: PathBuf, name: String },
    Global { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    file: TrustStoreFile,
    pub load_warning: Option<String>,
}

impl TrustStore {
    pub fn path_for(data_home: &Path) -> PathBuf {
        data_home.join(TRUST_STORE_FILENAME)
    }

    /// Unreadable, corrupt and unsupported records are empty, never trusted.
    pub fn load(data_home: &Path) -> TrustStore {
        let path = Self::path_for(data_home);
        let result = (|| {
            match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.to_string()),
                Ok(meta) => {
                    if !meta.is_file() || meta.file_type().is_symlink() {
                        return Err("not a private regular file".into());
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if meta.mode() & 0o777 != 0o600
                            || meta.nlink() != 1
                            || meta.uid() != nix::unistd::geteuid().as_raw()
                        {
                            return Err("trust store must be owned by this user with mode 0600 and one link".into());
                        }
                    }
                }
            }
            let file = read_private_json_or_miss::<TrustStoreFile>(&path, 1024 * 1024)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "unsafe, oversized or corrupt trust store".to_string())?;
            if file.version != TRUST_STORE_VERSION {
                return Err(format!("unsupported trust store version {}", file.version));
            }
            Ok(Some(file))
        })();
        let (file, load_warning) = match result {
            Ok(file) => (file, None),
            Err(reason) => {
                let warning = format!(
                    "cannot load agent trust store {}: {reason}; treating it as empty",
                    path.display()
                );
                tracing::warn!("{warning}");
                (None, Some(warning))
            }
        };
        TrustStore {
            path,
            file: file.unwrap_or(TrustStoreFile {
                version: TRUST_STORE_VERSION,
                entries: Vec::new(),
            }),
            load_warning,
        }
    }

    pub fn is_trusted(&self, key: &TrustKey, sha256: &str) -> bool {
        let key = canonical_key(key);
        self.file.entries.iter().any(|entry| {
            entry_matches(entry, &key)
                && match entry {
                    TrustEntry::Repo {
                        sha256: approved, ..
                    }
                    | TrustEntry::Global {
                        sha256: approved, ..
                    } => approved == sha256,
                }
        })
    }

    /// Replace any old hash for the key, committing memory only after the atomic
    /// write succeeds. Concurrent writers can lose approvals, never gain trust.
    pub fn approve(&mut self, key: TrustKey, name: &str, sha256: &str) -> Result<(), DataError> {
        let key = canonical_key(&key);
        let parent = self.path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            create_private_dir(parent)?;
        }
        let _lock = crate::data::fs::private_files::lock_private_exclusive(
            &self.path.with_extension("lock"),
            || Ok(()),
        )?;
        let mut file = Self::load(parent).file;
        file.entries.retain(|entry| !entry_matches(entry, &key));
        let approved_at = chrono::Utc::now().to_rfc3339();
        let entry = match key {
            TrustKey::Repo { git_root, name } => TrustEntry::Repo {
                git_root,
                name,
                sha256: sha256.into(),
                approved_at,
            },
            TrustKey::Global { path } => TrustEntry::Global {
                path,
                name: name.into(),
                sha256: sha256.into(),
                approved_at,
            },
        };
        file.entries.push(entry);
        let parent = self.path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            create_private_dir(parent)?;
        }
        write_private_json_atomic(&self.path, &file, || Ok(()))?;
        self.file = file;
        self.load_warning = None;
        Ok(())
    }

    pub fn entries(&self) -> &[TrustEntry] {
        &self.file.entries
    }
}

pub fn canonical_key_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn canonical_key(key: &TrustKey) -> TrustKey {
    match key {
        TrustKey::Repo { git_root, name } => TrustKey::Repo {
            git_root: canonical_key_path(git_root),
            name: name.clone(),
        },
        TrustKey::Global { path } => TrustKey::Global {
            path: canonical_key_path(path),
        },
    }
}

fn entry_matches(entry: &TrustEntry, key: &TrustKey) -> bool {
    match (entry, key) {
        (
            TrustEntry::Repo { git_root, name, .. },
            TrustKey::Repo {
                git_root: root,
                name: key_name,
            },
        ) => git_root == root && name == key_name,
        (TrustEntry::Global { path, .. }, TrustKey::Global { path: key_path }) => path == key_path,
        _ => false,
    }
}
