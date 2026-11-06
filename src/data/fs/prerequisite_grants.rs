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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_round_trip_use_canonical_roots_and_redacted_remotes() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("state");
        let key = GrantKey {
            git_root: root.path().into(),
            remote_url: "https://user:password@ghe.example/o/r".into(),
            prerequisite: RememberedKind::GithubToken,
        };
        let grant = RememberedGrant::TokenEnv {
            var: "GH_TOKEN".into(),
        };
        let mut store = PrerequisiteGrantStore::load(&home);
        store.remember(key.clone(), grant.clone()).unwrap();
        let text = std::fs::read_to_string(PrerequisiteGrantStore::path_for(&home)).unwrap();
        assert!(!text.contains("password"));
        let mut store = PrerequisiteGrantStore::load(&home);
        assert_eq!(store.lookup(&key), Some(&grant));
        let id = store.records()[0].id.clone();
        assert_eq!(store.revoke(&GrantSelector::Id(id)).unwrap(), 1);
        assert!(store.lookup(&key).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn v1_records_receive_stable_grant_ids_on_first_load() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = PrerequisiteGrantStore::path_for(root.path());
        std::fs::write(&path, r#"{"version":1,"records":[{"key":{"git_root":"/repo","remote_url":"https://example.com/o/r","prerequisite":"github_token"},"grant":{"type":"token_env","var":"GH_TOKEN"},"recorded_at":"2026-01-01T00:00:00Z"}]}"#).unwrap();
        let mut legacy: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        legacy["records"][0]["key"]["git_root"] = serde_json::json!(root.path());
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut store = PrerequisiteGrantStore::load(root.path());
        let assigned = store.records()[0].id.clone();
        let persisted: GrantStoreFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.records[0].id, assigned);
        assert_eq!(persisted.version, PREREQUISITE_GRANTS_VERSION);
        assert!(assigned.as_str().starts_with("grant_"));
        let key = store.records()[0].key.clone();
        let grant = store.records()[0].grant.clone();
        store.remember(key, grant).unwrap();
        let reopened = PrerequisiteGrantStore::load(root.path());
        assert_eq!(reopened.records()[0].id, store.records()[0].id);
        assert_eq!(
            reopened.records()[0].id,
            GrantId::parse(assigned.as_str()).unwrap()
        );
        assert_eq!(reopened.file.version, 2);
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_or_unsafe_files_grant_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = PrerequisiteGrantStore::path_for(root.path());
        for (text, mode) in [
            ("bad json", 0o600),
            (r#"{"version":1,"records":[]}"#, 0o644),
        ] {
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let store = PrerequisiteGrantStore::load(root.path());
            assert!(store.records().is_empty());
            assert!(store.load_warning.is_some());
        }
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    fn key(root: &Path, url: &str, kind: RememberedKind) -> GrantKey {
        GrantKey {
            git_root: root.into(),
            remote_url: url.into(),
            prerequisite: kind,
        }
    }

    fn ssh_grant() -> RememberedGrant {
        RememberedGrant::SshKey {
            key: "~/.ssh/id_ed25519".into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn remember_then_lookup_roundtrip_atomic_0600() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let k = key(root.path(), "git@github.com:o/r.git", RememberedKind::Ssh);
        let mut store = PrerequisiteGrantStore::load(&state);
        assert!(store.load_warning.is_none());
        store.remember(k.clone(), ssh_grant()).unwrap();
        let path = PrerequisiteGrantStore::path_for(&state);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Atomic write: no temp siblings are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers.len(), 2);
        assert!(leftovers.contains(&PREREQUISITE_GRANTS_FILENAME.to_string()));
        assert!(leftovers.contains(&"prerequisite-grants.lock".to_string()));
        let reloaded = PrerequisiteGrantStore::load(&state);
        assert_eq!(reloaded.lookup(&k), Some(&ssh_grant()));
        assert_eq!(reloaded.records().len(), 1);
        assert!(reloaded.load_warning.is_none());
    }

    #[test]
    fn remembering_again_replaces_the_record_last_writer_wins() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let k = key(root.path(), "git@github.com:o/r.git", RememberedKind::Ssh);
        let mut store = PrerequisiteGrantStore::load(&state);
        store.remember(k.clone(), ssh_grant()).unwrap();
        store.remember(k.clone(), RememberedGrant::SshDir).unwrap();
        assert_eq!(store.records().len(), 1);
        assert_eq!(store.lookup(&k), Some(&RememberedGrant::SshDir));
        // A second writer that loaded before the first write still replaces by key.
        let mut stale = PrerequisiteGrantStore::load(&state);
        stale.remember(k.clone(), ssh_grant()).unwrap();
        assert_eq!(
            PrerequisiteGrantStore::load(&state).lookup(&k),
            Some(&ssh_grant())
        );
    }

    #[test]
    fn changed_remote_url_misses() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let mut store = PrerequisiteGrantStore::load(&state);
        store
            .remember(
                key(root.path(), "git@github.com:o/r.git", RememberedKind::Ssh),
                ssh_grant(),
            )
            .unwrap();
        for other in [
            "git@github.com:o/other.git",
            "git@evil.example:o/r.git",
            "https://github.com/o/r.git",
        ] {
            assert!(store
                .lookup(&key(root.path(), other, RememberedKind::Ssh))
                .is_none());
        }
        // A different prerequisite kind on the same remote also misses.
        assert!(store
            .lookup(&key(
                root.path(),
                "git@github.com:o/r.git",
                RememberedKind::GithubToken
            ))
            .is_none());
        // And a different repo root misses.
        let other_root = tempfile::tempdir().unwrap();
        assert!(store
            .lookup(&key(
                other_root.path(),
                "git@github.com:o/r.git",
                RememberedKind::Ssh
            ))
            .is_none());
        // Credentials embedded in the lookup URL do not change the key.
        assert!(store
            .lookup(&key(
                root.path(),
                "ssh://git@github.com/o/r.git",
                RememberedKind::Ssh
            ))
            .is_none());
    }

    #[test]
    fn revoke_by_id_and_all() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let mut store = PrerequisiteGrantStore::load(&state);
        let a = key(root.path(), "git@github.com:o/a.git", RememberedKind::Ssh);
        let b = key(
            root.path(),
            "https://github.com/o/b",
            RememberedKind::GithubToken,
        );
        let c = key(
            root.path(),
            "https://github.com/o/c",
            RememberedKind::GitIdentity,
        );
        store.remember(a.clone(), ssh_grant()).unwrap();
        store
            .remember(
                b.clone(),
                RememberedGrant::TokenEnv {
                    var: "GH_TOKEN".into(),
                },
            )
            .unwrap();
        store
            .remember(
                c.clone(),
                RememberedGrant::Identity {
                    name: "N".into(),
                    email: "n@x.test".into(),
                },
            )
            .unwrap();
        let a_id = store
            .records()
            .iter()
            .find(|r| r.key == canonical_key(&a).unwrap())
            .unwrap()
            .id
            .clone();
        assert_eq!(store.revoke(&GrantSelector::Id(a_id)).unwrap(), 1);
        assert!(store.lookup(&a).is_none());
        let b_id = store
            .records()
            .iter()
            .find(|r| r.key == canonical_key(&b).unwrap())
            .unwrap()
            .id
            .clone();
        assert_eq!(store.revoke(&GrantSelector::Id(b_id)).unwrap(), 1);
        assert!(store.lookup(&b).is_none());
        assert!(store.lookup(&c).is_some());
        assert_eq!(store.revoke(&GrantSelector::All).unwrap(), 1);
        assert!(PrerequisiteGrantStore::load(&state).records().is_empty());
        assert_eq!(store.revoke(&GrantSelector::All).unwrap(), 0);
    }

    #[test]
    fn invalid_decisions_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let mut store = PrerequisiteGrantStore::load(&root.path().join("state"));
        let r = root.path();
        // Mismatched kind/decision.
        assert!(store
            .remember(
                key(r, "u", RememberedKind::Ssh),
                RememberedGrant::TokenEnv {
                    var: "GH_TOKEN".into()
                }
            )
            .is_err());
        // Bad env var name, so a secret pasted by mistake cannot be stored as a "name".
        assert!(store
            .remember(
                key(r, "u", RememberedKind::GithubToken),
                RememberedGrant::TokenEnv {
                    var: "ghp_abc123".into()
                }
            )
            .is_err());
        for (name, email) in [("", "a@b"), ("N", "nobody"), ("N\n", "a@b"), ("N", "a@b\n")] {
            assert!(store
                .remember(
                    key(r, "u", RememberedKind::GitIdentity),
                    RememberedGrant::Identity {
                        name: name.into(),
                        email: email.into()
                    }
                )
                .is_err());
        }
        // Nonexistent git root cannot be canonicalised.
        assert!(store
            .remember(
                key(&r.join("missing"), "u", RememberedKind::Ssh),
                ssh_grant()
            )
            .is_err());
        assert!(store.records().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_oversized_or_world_readable_store_grants_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = PrerequisiteGrantStore::path_for(root.path());
        let valid_record = format!(
            r#"{{"key":{{"git_root":"{}","remote_url":"u","prerequisite":"ssh"}},"grant":{{"type":"ssh_dir"}},"recorded_at":"t"}}"#,
            root.path().display()
        );
        let oversized = format!(
            r#"{{"version":1,"records":[],"pad":"{}"}}"#,
            "a".repeat(PREREQUISITE_GRANTS_MAX_BYTES + 10)
        );
        let cases: Vec<(String, u32)> = vec![
            ("not json".into(), 0o600),
            (r#"{"version":1,"records":[]}"#.into(), 0o644),
            (r#"{"version":1,"records":[]}"#.into(), 0o660),
            (r#"{"version":3,"records":[]}"#.into(), 0o600),
            (oversized, 0o600),
            // Relative git root.
            (
                r#"{"version":1,"records":[{"key":{"git_root":"rel","remote_url":"u","prerequisite":"ssh"},"grant":{"type":"ssh_dir"},"recorded_at":"t"}]}"#.into(),
                0o600,
            ),
            // Decision does not match its kind.
            (
                format!(
                    r#"{{"version":1,"records":[{{"key":{{"git_root":"{}","remote_url":"u","prerequisite":"github_token"}},"grant":{{"type":"ssh_dir"}},"recorded_at":"t"}}]}}"#,
                    root.path().display()
                ),
                0o600,
            ),
            // One good record and one bad: the whole file grants nothing.
            (
                format!(
                    r#"{{"version":1,"records":[{valid_record},{{"key":1}}]}}"#
                ),
                0o600,
            ),
        ];
        for (text, mode) in cases {
            std::fs::write(&path, &text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let store = PrerequisiteGrantStore::load(root.path());
            assert!(store.records().is_empty(), "granted from {text:.60}");
            assert!(store.load_warning.is_some(), "no warning for {text:.60}");
        }
        // The valid file alone loads.
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"records":[{valid_record}]}}"#),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let store = PrerequisiteGrantStore::load(root.path());
        assert_eq!(store.records().len(), 1);
        assert!(store.load_warning.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_or_hardlinked_store_grants_nothing() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let path = PrerequisiteGrantStore::path_for(root.path());
        let real = root.path().join("real.json");
        std::fs::write(&real, r#"{"version":1,"records":[]}"#).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&real, &path).unwrap();
        let store = PrerequisiteGrantStore::load(root.path());
        assert!(store.load_warning.is_some());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&real, &path).unwrap();
        let store = PrerequisiteGrantStore::load(root.path());
        assert!(
            store.load_warning.is_some(),
            "hard-linked file must be refused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_malformed_store_is_replaced_cleanly_by_the_next_remember() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = PrerequisiteGrantStore::path_for(root.path());
        std::fs::write(&path, "garbage").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut store = PrerequisiteGrantStore::load(root.path());
        assert!(store.load_warning.is_some());
        let k = key(root.path(), "u", RememberedKind::Ssh);
        store.remember(k.clone(), RememberedGrant::SshDir).unwrap();
        assert!(store.load_warning.is_none());
        assert_eq!(
            PrerequisiteGrantStore::load(root.path()).lookup(&k),
            Some(&RememberedGrant::SshDir)
        );
    }

    #[test]
    fn stored_file_holds_decisions_only_never_secrets() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let mut store = PrerequisiteGrantStore::load(&state);
        store
            .remember(
                key(
                    root.path(),
                    "https://x-access-token:ghp_SECRET99@github.com/o/r",
                    RememberedKind::GithubToken,
                ),
                RememberedGrant::TokenEnv {
                    var: "GH_TOKEN".into(),
                },
            )
            .unwrap();
        store
            .remember(
                key(root.path(), "git@github.com:o/r", RememberedKind::Ssh),
                RememberedGrant::SshKey {
                    key: "~/.ssh/id_ed25519".into(),
                },
            )
            .unwrap();
        let text = std::fs::read_to_string(PrerequisiteGrantStore::path_for(&state)).unwrap();
        assert!(!text.contains("ghp_SECRET99"));
        assert!(!text.contains("x-access-token"));
        assert!(!text.contains("PRIVATE KEY"));
        assert!(text.contains("GH_TOKEN"));
        assert!(text.contains("~/.ssh/id_ed25519"));
    }
}

#[cfg(test)]
mod concurrent_review_tests {
    use super::*;
    #[test]
    fn stale_remember_does_not_resurrect_revoked_grants_or_lose_other_keys() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("state");
        let key = |kind| GrantKey {
            git_root: root.path().into(),
            remote_url: "https://github.com/o/r".into(),
            prerequisite: kind,
        };
        let mut first = PrerequisiteGrantStore::load(&home);
        first
            .remember(
                key(RememberedKind::GithubToken),
                RememberedGrant::TokenEnv {
                    var: "GH_TOKEN".into(),
                },
            )
            .unwrap();
        let mut stale = PrerequisiteGrantStore::load(&home);
        first.revoke(&GrantSelector::All).unwrap();
        stale
            .remember(key(RememberedKind::Ssh), RememberedGrant::SshDir)
            .unwrap();
        assert!(PrerequisiteGrantStore::load(&home)
            .lookup(&key(RememberedKind::GithubToken))
            .is_none());
        first
            .remember(
                key(RememberedKind::GitIdentity),
                RememberedGrant::Identity {
                    name: "Bot".into(),
                    email: "bot@x.test".into(),
                },
            )
            .unwrap();
        assert_eq!(PrerequisiteGrantStore::load(&home).records().len(), 2);
    }
}
