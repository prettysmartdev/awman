//! Remembered workflow prerequisite decisions, never credential values.

use crate::data::error::DataError;
use crate::data::fs::private_files::{create_private_dir, write_private_json_atomic};
use crate::data::ids::GrantId;
use crate::data::workflow_prerequisites::GitRemoteUrl;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PREREQUISITE_GRANTS_FILENAME: &str = "prerequisite-grants.json";
pub const PREREQUISITE_GRANTS_VERSION: u32 = 2;
pub const PREREQUISITE_GRANTS_MAX_BYTES: usize = 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RememberedKind {
    Ssh,
    GithubToken,
    GitIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantKey {
    pub git_root: PathBuf,
    pub remote_url: String,
    pub prerequisite: RememberedKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RememberedGrant {
    SshKey { key: PathBuf },
    SshDir,
    TokenEnv { var: String },
    Identity { name: String, email: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    #[serde(default)]
    pub id: GrantId,
    pub key: GrantKey,
    pub grant: RememberedGrant,
    pub recorded_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantStoreFile {
    pub version: u32,
    pub records: Vec<GrantRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantSelector {
    Id(GrantId),
    All,
}
#[derive(Debug, Clone)]
pub struct PrerequisiteGrantStore {
    path: PathBuf,
    file: GrantStoreFile,
    pub load_warning: Option<String>,
}

impl PrerequisiteGrantStore {
    pub fn path_for(data_home: &Path) -> PathBuf {
        data_home.join(PREREQUISITE_GRANTS_FILENAME)
    }
    pub fn load(data_home: &Path) -> Self {
        Self::load_inner(data_home, true)
    }
    fn load_without_persist(data_home: &Path) -> Self {
        Self::load_inner(data_home, false)
    }
    fn load_inner(data_home: &Path, persist_ids: bool) -> Self {
        let path = Self::path_for(data_home);
        let mut needs_persist = false;
        let loaded = (|| {
            use std::io::Read;
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
            }
            #[cfg(not(unix))]
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err("not a private regular file");
            }
            let opened = match options.open(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("unsafe or unreadable file"),
                Ok(file) => file,
            };
            // Check the descriptor actually read, so a concurrent replacement
            // cannot exchange the checked file for an unchecked one.
            let meta = opened.metadata().map_err(|_| "unreadable metadata")?;
            if !meta.is_file() {
                return Err("not a private regular file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.mode() & 0o777 != 0o600
                    || meta.nlink() != 1
                    || meta.uid() != nix::unistd::geteuid().as_raw()
                {
                    return Err("requires mode 0600, one link and ownership by this user");
                }
            }
            let mut bytes = Vec::new();
            opened
                .take(PREREQUISITE_GRANTS_MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "unreadable file")?;
            if bytes.len() > PREREQUISITE_GRANTS_MAX_BYTES {
                return Err("oversized file");
            }
            let mut file: GrantStoreFile =
                serde_json::from_slice(&bytes).map_err(|_| "corrupt file")?;
            if !matches!(file.version, 1 | PREREQUISITE_GRANTS_VERSION) {
                return Err("unsupported version");
            }
            let had_ids = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("records")
                        .and_then(serde_json::Value::as_array)
                        .cloned()
                })
                .is_some_and(|records| records.iter().all(|record| record.get("id").is_some()));
            needs_persist = file.version != PREREQUISITE_GRANTS_VERSION || !had_ids;
            file.version = PREREQUISITE_GRANTS_VERSION;
            for record in &mut file.records {
                if !record.key.git_root.is_absolute() || !valid_decision(&record.key, &record.grant)
                {
                    return Err("invalid decision record");
                }
                record.key.remote_url = GitRemoteUrl::parse(&record.key.remote_url).redacted;
            }
            Ok(Some(file))
        })();
        let (file, load_warning) = match loaded {
            Ok(file) => (file, None),
            Err(reason) => {
                let warning = format!(
                    "cannot load workflow prerequisite grants {}: {reason}; treating it as empty",
                    path.display()
                );
                tracing::warn!("{warning}");
                (None, Some(warning))
            }
        };
        if persist_ids && needs_persist {
            if let Some(file) = &file {
                let persisted = crate::data::fs::private_files::lock_private_exclusive(
                    &path.with_extension("lock"),
                    || Ok(()),
                )
                .and_then(|_lock| write_private_json_atomic(&path, file, || Ok(())));
                if let Err(error) = persisted {
                    tracing::warn!(%error, path = %path.display(), "could not persist migrated prerequisite grant IDs");
                }
            }
        }
        Self {
            path,
            file: file.unwrap_or(GrantStoreFile {
                version: PREREQUISITE_GRANTS_VERSION,
                records: vec![],
            }),
            load_warning,
        }
    }
    pub fn lookup(&self, key: &GrantKey) -> Option<&RememberedGrant> {
        let key = canonical_key(key).ok()?;
        self.file
            .records
            .iter()
            .find(|r| r.key == key)
            .map(|r| &r.grant)
    }
    pub fn remember(&mut self, key: GrantKey, grant: RememberedGrant) -> Result<(), DataError> {
        let key = canonical_key(&key)?;
        if !valid_decision(&key, &grant) {
            return Err(DataError::Other(
                "invalid workflow prerequisite grant decision".into(),
            ));
        }
        let parent = self.path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            create_private_dir(parent)?;
        }
        let _lock = crate::data::fs::private_files::lock_private_exclusive(
            &self.path.with_extension("lock"),
            || Ok(()),
        )?;
        let mut file = Self::load_without_persist(parent).file;
        self.preserve_assigned_ids(&mut file);
        let id = file
            .records
            .iter()
            .find(|record| record.key == key && record.grant == grant)
            .map(|record| record.id.clone())
            .unwrap_or_default();
        file.records.retain(|r| r.key != key);
        file.records.push(GrantRecord {
            id,
            key,
            grant,
            recorded_at: chrono::Utc::now().to_rfc3339(),
        });
        self.save(file)
    }
    pub fn revoke(&mut self, selector: &GrantSelector) -> Result<usize, DataError> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            create_private_dir(parent)?;
        }
        let _lock = crate::data::fs::private_files::lock_private_exclusive(
            &self.path.with_extension("lock"),
            || Ok(()),
        )?;
        let mut file = Self::load_without_persist(parent).file;
        self.preserve_assigned_ids(&mut file);
        let before = file.records.len();
        match selector {
            GrantSelector::All => file.records.clear(),
            GrantSelector::Id(id) => file.records.retain(|r| &r.id != id),
        }
        let removed = before - file.records.len();
        self.save(file)?;
        Ok(removed)
    }
    pub fn records(&self) -> &[GrantRecord] {
        &self.file.records
    }
    fn save(&mut self, file: GrantStoreFile) -> Result<(), DataError> {
        if serde_json::to_vec_pretty(&file)
            .map_err(|source| DataError::ConfigSerialize { source })?
            .len()
            > PREREQUISITE_GRANTS_MAX_BYTES
        {
            return Err(DataError::Other(
                "workflow prerequisite grants exceed the size limit".into(),
            ));
        }
        let parent = self.path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            create_private_dir(parent)?;
        }
        write_private_json_atomic(&self.path, &file, || Ok(()))?;
        self.file = file;
        self.load_warning = None;
        Ok(())
    }

    /// A v1 record gets an ephemeral ID when loaded. Preserve that ID when a
    /// subsequent write reloads the file under the lock, as long as the
    /// record itself is unchanged.
    fn preserve_assigned_ids(&self, file: &mut GrantStoreFile) {
        for record in &mut file.records {
            if let Some(previous) = self.file.records.iter().find(|previous| {
                previous.key == record.key
                    && previous.grant == record.grant
                    && previous.recorded_at == record.recorded_at
            }) {
                record.id = previous.id.clone();
            }
        }
    }
}
fn canonical_key(key: &GrantKey) -> Result<GrantKey, DataError> {
    Ok(GrantKey {
        git_root: std::fs::canonicalize(&key.git_root)
            .map_err(|e| DataError::io(&key.git_root, e))?,
        remote_url: GitRemoteUrl::parse(&key.remote_url).redacted,
        prerequisite: key.prerequisite,
    })
}
fn valid_decision(key: &GrantKey, grant: &RememberedGrant) -> bool {
    match (key.prerequisite, grant) {
        (RememberedKind::Ssh, RememberedGrant::SshKey { .. } | RememberedGrant::SshDir) => true,
        (RememberedKind::GithubToken, RememberedGrant::TokenEnv { var }) => {
            crate::data::config::workflows::valid_env_name(var)
        }
        (RememberedKind::GitIdentity, RememberedGrant::Identity { name, email }) => {
            !name.trim().is_empty()
                && email.contains('@')
                && !name.chars().chain(email.chars()).any(char::is_control)
        }
        _ => false,
    }
}
