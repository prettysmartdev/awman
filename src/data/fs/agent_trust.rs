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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unsafe_private_records_grant_no_trust_and_warn() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let key = TrustKey::Repo {
            git_root: dir.path().into(),
            name: "pi".into(),
        };
        let mut store = TrustStore::load(dir.path());
        store.approve(key.clone(), "pi", "hash").unwrap();
        let path = TrustStore::path_for(dir.path());
        let saved = dir.path().join("saved.json");
        std::fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        let check = || {
            let store = TrustStore::load(dir.path());
            assert!(store.entries().is_empty());
            assert!(store.load_warning.is_some());
            assert!(!store.is_trusted(&key, "hash"));
        };
        check();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(saved, &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        check();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        check();
    }

    #[test]
    fn approval_roundtrip_and_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let key = TrustKey::Repo {
            git_root: tmp.path().into(),
            name: "pi".into(),
        };
        let mut store = TrustStore::load(&home);
        assert!(!store.is_trusted(&key, "old"));
        store.approve(key.clone(), "pi", "old").unwrap();
        store.approve(key.clone(), "pi", "new").unwrap();
        let store = TrustStore::load(&home);
        assert_eq!(store.entries().len(), 1);
        assert!(store.is_trusted(&key, "new"));
        assert!(!store.is_trusted(&key, "old"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(TrustStore::path_for(&home))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn canonical_roots_share_repo_approval_but_names_remain_part_of_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let alias = tmp.path().join("repo-alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        #[cfg(not(unix))]
        let alias = root.clone();
        let key = TrustKey::Repo {
            git_root: root.clone(),
            name: "pi".into(),
        };
        let alias_key = TrustKey::Repo {
            git_root: alias,
            name: "pi".into(),
        };
        let mut store = TrustStore::load(&tmp.path().join("home"));
        store.approve(key.clone(), "pi", "hash").unwrap();
        assert!(store.is_trusted(&alias_key, "hash"));
        assert!(!store.is_trusted(
            &TrustKey::Repo {
                git_root: root,
                name: "other".into()
            },
            "hash"
        ));
    }

    #[test]
    fn corrupt_store_is_empty_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(TrustStore::path_for(tmp.path()), b"not json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                TrustStore::path_for(tmp.path()),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let store = TrustStore::load(tmp.path());
        assert!(store.entries().is_empty());
        assert!(store
            .load_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("treating it as empty")));
    }
}

#[cfg(test)]
mod concurrent_review_tests {
    use super::*;
    #[test]
    fn unrelated_stale_approval_does_not_restore_an_old_hash() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("state");
        let key = |name: &str| TrustKey::Repo {
            git_root: root.path().into(),
            name: name.into(),
        };
        let mut first = TrustStore::load(&home);
        first.approve(key("one"), "one", "old").unwrap();
        let mut stale = TrustStore::load(&home);
        first.approve(key("one"), "one", "new").unwrap();
        stale.approve(key("two"), "two", "other").unwrap();
        let result = TrustStore::load(&home);
        assert!(result.is_trusted(&key("one"), "new"));
        assert!(!result.is_trusted(&key("one"), "old"));
    }
}
