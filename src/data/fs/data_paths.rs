//! `DataPaths` — the awman home layout and the shared data root.
//!
//! Every awman path derives from two homes:
//!
//! | Need | `AWMAN_HOME` set | unset, XDG set | unset, no XDG |
//! |---|---|---|---|
//! | config home (`config.json`) | `$AWMAN_HOME` | `$XDG_CONFIG_HOME/awman` | `~/.awman` |
//! | data home (everything else) | `$AWMAN_HOME` | `$XDG_DATA_HOME/awman` | `~/.awman` |
//!
//! Under the data home: `data/awman.db`, `data/repos/<hash>/state.json`,
//! `api/`, `squad/`, `attach/`, `builtin/`, and the other data dirs.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;

/// The one process variable that relocates every awman path.
pub const AWMAN_HOME: &str = "AWMAN_HOME";

/// Subdirectory of `$HOME` used when neither `AWMAN_HOME` nor XDG is set.
pub const HOME_SUBDIR: &str = ".awman";

/// `AWMAN_HOME`, when set and non-empty.
pub fn awman_home(env: &EnvSnapshot) -> Option<PathBuf> {
    env.get(AWMAN_HOME)
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// Where `config.json` lives: `$AWMAN_HOME` → `$XDG_CONFIG_HOME/awman` → `~/.awman`.
pub fn config_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    if let Some(home) = awman_home(env) {
        return Ok(home);
    }
    if let Some(xdg) = env.xdg_config_home() {
        return Ok(xdg.join("awman"));
    }
    user_home(env)
}

/// Where everything else lives: `$AWMAN_HOME` → `$XDG_DATA_HOME/awman` → `~/.awman`.
pub fn data_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    if let Some(home) = awman_home(env) {
        return Ok(home);
    }
    if let Some(xdg) = env.xdg_data_home() {
        return Ok(xdg.join("awman"));
    }
    user_home(env)
}

fn user_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    let home = env
        .home_dir()
        .or_else(dirs::home_dir)
        .ok_or(DataError::HomeNotFound)?;
    Ok(home.join(HOME_SUBDIR))
}

/// Subdirectory under the data home that hosts the shared database.
pub const DATA_SUBDIR: &str = "data";

/// Filename of the shared sqlite database. Identical to
/// `api_paths::API_DB_FILENAME`.
pub const DB_FILENAME: &str = "awman.db";

const MIGRATION_LOCK_FILENAME: &str = ".migrating";
const STALE_MIGRATION_LOCK_AGE: Duration = Duration::from_secs(60);

/// Result of relocating the legacy API-owned database into the shared data
/// directory. Presentation layers decide whether and how to report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    FreshInstall,
    AlreadyMigrated,
    Migrated {
        copied_bytes: u64,
        sidecars: Vec<PathBuf>,
        backup_kept: bool,
    },
    RecoveredFromInterrupted {
        copied_bytes: u64,
    },
}

/// Resolves paths under the shared data root (`<data_home>/data`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    root: PathBuf,
}

impl DataPaths {
    /// Build `DataPaths` rooted at an explicit `<data_home>/data` directory.
    pub fn at_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Resolve from a supplied env snapshot: `<data_home>/data`.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        let data_home = data_home(env)?;
        Ok(Self::at_root(data_home.join(DATA_SUBDIR)))
    }

    /// Where `config.json` lives for `env` (see [`config_home`]).
    pub fn config_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
        config_home(env)
    }

    /// The shared data root (`<data_home>/data`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The data home this root sits in (`<data_home>`).
    pub fn home(&self) -> &Path {
        self.root.parent().unwrap_or(&self.root)
    }

    /// Per-repo state directory: `<root>/repos/<first 16 hex of sha256(git_root)>`.
    pub fn repo_state_dir(&self, git_root: &Path) -> PathBuf {
        let hash = crate::data::fs::hash::sha256_hex(git_root.to_string_lossy().as_bytes());
        self.root.join("repos").join(&hash[..16])
    }

    /// Path to the shared sqlite database (`<root>/awman.db`).
    pub fn db_path(&self) -> PathBuf {
        self.root.join(DB_FILENAME)
    }

    /// Create the data root (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }

    /// Relocate the database from a pre-squad API root, preserving SQLite's WAL
    /// sidecars. The original is renamed only after all copies verify, making
    /// an interrupted migration safely resumable.
    pub fn migrate_legacy_db(&self, legacy_root: &Path) -> Result<MigrationOutcome, DataError> {
        self.ensure_root()?;
        let _lock = MigrationLock::acquire(&self.root)?;

        let legacy = legacy_root.join(DB_FILENAME);
        let target = self.db_path();
        let target_sidecars = sidecars_for(&target);
        let legacy_sidecars = sidecars_for(&legacy);

        let recovered = match (legacy.exists(), target.exists()) {
            (false, false) => return Ok(MigrationOutcome::FreshInstall),
            (false, true) => return Ok(MigrationOutcome::AlreadyMigrated),
            (true, true) => {
                // The legacy source still has its name, so any target was
                // written before verification/rename and cannot be trusted.
                remove_targets(&target, &target_sidecars)?;
                true
            }
            (true, false) => false,
        };

        let copied_bytes = match copy_and_verify(&legacy, &target, &legacy_sidecars) {
            Ok(bytes) => bytes,
            Err(error) => {
                // Copy/verification failure must leave recognisable legacy
                // files intact. Only the untrusted destination is removed.
                let _ = remove_targets(&target, &target_sidecars);
                return Err(error);
            }
        };
        let (sidecars, backup_kept) = rename_legacy_aside(&legacy, &legacy_sidecars)?;
        if recovered {
            return Ok(MigrationOutcome::RecoveredFromInterrupted { copied_bytes });
        }
        Ok(MigrationOutcome::Migrated {
            copied_bytes,
            sidecars,
            backup_kept,
        })
    }
}

/// A best-effort process lock for a short filesystem migration. It uses the
/// same `O_CREAT | O_EXCL` idiom as daemon pidfile claims and cleans itself up
/// on every return path.
struct MigrationLock {
    path: PathBuf,
}

impl MigrationLock {
    fn acquire(root: &Path) -> Result<Self, DataError> {
        let path = root.join(MIGRATION_LOCK_FILENAME);
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(std::process::id().to_string().as_bytes())
                        .map_err(|e| DataError::io(&path, e))?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > STALE_MIGRATION_LOCK_AGE);
                    if stale {
                        match std::fs::remove_file(&path) {
                            Ok(()) => continue,
                            Err(remove_error)
                                if remove_error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                continue;
                            }
                            Err(remove_error) => return Err(DataError::io(&path, remove_error)),
                        }
                    }
                    return Err(DataError::Other(format!(
                        "database migration is already in progress ({})",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(DataError::Other(format!(
                        "database migration is already in progress ({})",
                        path.display()
                    )));
                }
                Err(error) => return Err(DataError::io(&path, error)),
            }
        }
        unreachable!("migration lock retries are bounded")
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sidecars_for(db_path: &Path) -> [PathBuf; 2] {
    [
        path_with_suffix(db_path, "-wal"),
        path_with_suffix(db_path, "-shm"),
    ]
}

fn remove_targets(target: &Path, sidecars: &[PathBuf; 2]) -> Result<(), DataError> {
    for path in std::iter::once(target).chain(sidecars.iter().map(PathBuf::as_path)) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DataError::io(path, error)),
        }
    }
    Ok(())
}

fn copy_and_verify(
    legacy: &Path,
    target: &Path,
    legacy_sidecars: &[PathBuf; 2],
) -> Result<u64, DataError> {
    let mut copied_bytes = copy_and_verify_one(legacy, target)?;
    for (source, destination) in legacy_sidecars.iter().zip(sidecars_for(target)) {
        if source.exists() {
            copied_bytes += copy_and_verify_one(source, &destination)?;
        }
    }
    Ok(copied_bytes)
}

fn copy_and_verify_one(source: &Path, destination: &Path) -> Result<u64, DataError> {
    let copied = std::fs::copy(source, destination).map_err(|e| DataError::io(destination, e))?;
    let source_len = std::fs::metadata(source)
        .map_err(|e| DataError::io(source, e))?
        .len();
    let destination_len = std::fs::metadata(destination)
        .map_err(|e| DataError::io(destination, e))?
        .len();
    if copied != source_len
        || destination_len != source_len
        || sha256(source)? != sha256(destination)?
    {
        return Err(DataError::Other(format!(
            "database migration verification failed for {}",
            source.display()
        )));
    }
    Ok(source_len)
}

fn sha256(path: &Path) -> Result<[u8; 32], DataError> {
    let bytes = std::fs::read(path).map_err(|e| DataError::io(path, e))?;
    Ok(Sha256::digest(bytes).into())
}

fn pre_migration_path(path: &Path) -> PathBuf {
    path_with_suffix(path, ".pre-migration")
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn rename_legacy_aside(
    legacy: &Path,
    legacy_sidecars: &[PathBuf; 2],
) -> Result<(Vec<PathBuf>, bool), DataError> {
    let mut sidecars = Vec::new();
    let mut backup_kept = true;
    for source in std::iter::once(legacy).chain(legacy_sidecars.iter().map(PathBuf::as_path)) {
        if !source.exists() {
            continue;
        }
        let backup = pre_migration_path(source);
        if backup.exists() {
            // An older backup is more conservative to retain. The verified
            // target is now authoritative, so remove only this old-name copy.
            std::fs::remove_file(source).map_err(|e| DataError::io(source, e))?;
            // `backup_kept` describes the *database* backup specifically: a
            // stale sidecar backup must not make the outcome claim this run's
            // main backup was discarded.
            if source == legacy {
                backup_kept = false;
            }
        } else {
            std::fs::rename(source, &backup).map_err(|e| DataError::io(source, e))?;
        }
        if source != legacy {
            sidecars.push(backup);
        }
    }
    Ok((sidecars, backup_kept))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{XDG_CONFIG_HOME, XDG_DATA_HOME};

    #[test]
    fn from_env_uses_awman_home_data_subdir() {
        let env = EnvSnapshot::with_overrides([(AWMAN_HOME, "/cfg/home")]);
        let paths = DataPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/cfg/home/data"));
        assert_eq!(paths.db_path(), PathBuf::from("/cfg/home/data/awman.db"));
    }

    #[test]
    fn from_env_uses_xdg_data_home() {
        let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
        let paths = DataPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/xdg/data/awman/data"));
    }

    #[test]
    fn awman_home_wins_over_xdg_for_both_homes() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_HOME, "/aw"),
            (XDG_CONFIG_HOME, "/xc"),
            (XDG_DATA_HOME, "/xd"),
        ]);
        assert_eq!(config_home(&env).unwrap(), PathBuf::from("/aw"));
        assert_eq!(data_home(&env).unwrap(), PathBuf::from("/aw"));
    }

    #[test]
    fn xdg_splits_config_and_data_homes_when_awman_home_is_unset() {
        let env = EnvSnapshot::with_overrides([(XDG_CONFIG_HOME, "/xc"), (XDG_DATA_HOME, "/xd")]);
        assert_eq!(config_home(&env).unwrap(), PathBuf::from("/xc/awman"));
        assert_eq!(data_home(&env).unwrap(), PathBuf::from("/xd/awman"));
    }

    #[test]
    fn removed_path_variables_are_ignored() {
        let env = EnvSnapshot::with_overrides([
            ("AWMAN_CONFIG_HOME", "/old"),
            ("AWMAN_API_ROOT", "/old"),
            ("AWMAN_SQUAD_ROOT", "/old"),
            ("HOME", "/h"),
        ]);
        assert_eq!(config_home(&env).unwrap(), PathBuf::from("/h/.awman"));
        assert_eq!(data_home(&env).unwrap(), PathBuf::from("/h/.awman"));
    }

    #[test]
    fn repo_state_dir_is_stable_per_git_root() {
        let paths = DataPaths::at_root("/aw/data");
        let dir = paths.repo_state_dir(Path::new("/repo"));
        assert!(dir.starts_with("/aw/data/repos"));
        assert_eq!(dir.file_name().unwrap().len(), 16);
        assert_eq!(dir, paths.repo_state_dir(Path::new("/repo")));
        assert_eq!(paths.home(), Path::new("/aw"));
    }

    #[test]
    fn db_path_is_awman_db() {
        let paths = DataPaths::at_root("/some/root/data");
        assert_eq!(paths.db_path(), PathBuf::from("/some/root/data/awman.db"));
    }

    #[test]
    fn migration_copies_main_db_and_sqlite_sidecars_before_renaming_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_root = tmp.path().join("api");
        let data = DataPaths::at_root(tmp.path().join("data"));
        std::fs::create_dir_all(&legacy_root).unwrap();
        std::fs::write(legacy_root.join(DB_FILENAME), b"main").unwrap();
        std::fs::write(legacy_root.join("awman.db-wal"), b"wal").unwrap();
        std::fs::write(legacy_root.join("awman.db-shm"), b"shm").unwrap();

        let outcome = data.migrate_legacy_db(&legacy_root).unwrap();
        assert!(matches!(
            outcome,
            MigrationOutcome::Migrated {
                copied_bytes: 10,
                backup_kept: true,
                ..
            }
        ));
        assert_eq!(std::fs::read(data.db_path()).unwrap(), b"main");
        assert_eq!(
            std::fs::read(data.root().join("awman.db-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(data.root().join("awman.db-shm")).unwrap(),
            b"shm"
        );
        assert_eq!(
            std::fs::read(legacy_root.join("awman.db.pre-migration")).unwrap(),
            b"main"
        );
        assert!(!legacy_root.join(DB_FILENAME).exists());
    }

    #[test]
    fn migration_discards_interrupted_target_and_restarts_from_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_root = tmp.path().join("api");
        let data = DataPaths::at_root(tmp.path().join("data"));
        std::fs::create_dir_all(&legacy_root).unwrap();
        data.ensure_root().unwrap();
        std::fs::write(legacy_root.join(DB_FILENAME), b"authoritative").unwrap();
        std::fs::write(data.db_path(), b"partial").unwrap();

        assert!(matches!(
            data.migrate_legacy_db(&legacy_root).unwrap(),
            MigrationOutcome::RecoveredFromInterrupted { copied_bytes: 13 }
        ));
        assert_eq!(std::fs::read(data.db_path()).unwrap(), b"authoritative");
    }

    #[test]
    fn existing_backup_is_preserved_without_failing_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_root = tmp.path().join("api");
        let data = DataPaths::at_root(tmp.path().join("data"));
        std::fs::create_dir_all(&legacy_root).unwrap();
        std::fs::write(legacy_root.join(DB_FILENAME), b"new-main").unwrap();
        std::fs::write(legacy_root.join("awman.db.pre-migration"), b"old-main").unwrap();

        assert!(matches!(
            data.migrate_legacy_db(&legacy_root).unwrap(),
            MigrationOutcome::Migrated {
                backup_kept: false,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(legacy_root.join("awman.db.pre-migration")).unwrap(),
            b"old-main"
        );
        assert!(!legacy_root.join(DB_FILENAME).exists());
        assert_eq!(std::fs::read(data.db_path()).unwrap(), b"new-main");
    }

    #[test]
    fn fresh_and_already_migrated_are_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_root = tmp.path().join("api");
        let data = DataPaths::at_root(tmp.path().join("data"));
        std::fs::create_dir_all(&legacy_root).unwrap();
        assert_eq!(
            data.migrate_legacy_db(&legacy_root).unwrap(),
            MigrationOutcome::FreshInstall
        );
        std::fs::write(data.db_path(), b"live").unwrap();
        assert_eq!(
            data.migrate_legacy_db(&legacy_root).unwrap(),
            MigrationOutcome::AlreadyMigrated
        );
    }

    #[test]
    fn failed_copy_preserves_legacy_and_removes_target() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_root = tmp.path().join("api");
        let data = DataPaths::at_root(tmp.path().join("data"));
        std::fs::create_dir_all(legacy_root.join(DB_FILENAME)).unwrap();
        assert!(data.migrate_legacy_db(&legacy_root).is_err());
        assert!(legacy_root.join(DB_FILENAME).exists());
        assert!(!data.db_path().exists());
    }
}
