//! Opening the shared session store, relocating a legacy database first.
//!
//! Before the shared data root existed the database lived at
//! `<data home>/api/awman.db`. Every producer that needs the on-disk store
//! (foreground automation, the daemon core) opens it through
//! [`DataPaths::open_session_store`], which, under one OS-held lock:
//!
//! 1. reads the migration journal, if any, and resumes or discards it;
//! 2. otherwise decides from what is on disk: nothing to move, a fresh
//!    migration, or two independent databases (a conflict, touched by nobody);
//! 3. copies the legacy files into a private staging directory, fsyncs and
//!    verifies them against hashes recorded in the journal before any copy;
//! 4. renames them into place (sidecars first, `awman.db` last), renames the
//!    legacy files aside as backups, and removes the staging directory and
//!    the journal last;
//! 5. opens the database, still holding the lock.
//!
//! A destination is trusted as the product of a migration only when the
//! journal names the same legacy source and every destination file present
//! equals its journal hash, in every phase including `published` (the phase
//! alone says nothing about what is on disk after a crash). Anything else is
//! a [`DataError::StoreMigrationConflict`]: nothing is copied, merged,
//! renamed or deleted on either side, WAL and SHM included.
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{DataPaths, MigrationOutcome, DB_FILENAME};
use crate::data::error::DataError;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::private_files::{self, LockMode};

/// OS-held exclusive lock serialising migration and opening.
pub const MIGRATION_LOCK_FILENAME: &str = "awman.db.migrate.lock";
/// The private migration journal in the data root.
pub const MIGRATION_JOURNAL_FILENAME: &str = "awman.db.migration.json";
/// Staging directories are `<data root>/.awman.db.migrating-<migration_id>`.
pub const MIGRATION_STAGING_PREFIX: &str = ".awman.db.migrating-";
/// How long an opener waits for another process holding the lock.
pub const MIGRATION_LOCK_WAIT: Duration = Duration::from_secs(30);
/// The journal format this build reads and writes.
pub const MIGRATION_JOURNAL_SCHEMA: u32 = 1;

/// Database files in publish order: sidecars first, the database last.
const PUBLISH_ORDER: [&str; 3] = ["awman.db-wal", "awman.db-shm", DB_FILENAME];

/// Where a migration has got to, as recorded in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MigrationPhase {
    Staging,
    Verified,
    Publishing,
    Published,
}

/// One legacy file a migration moves, as hashed before any copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationFileRecord {
    pub name: String,
    pub len: u64,
    pub sha256: String,
}

/// `<data root>/awman.db.migration.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationJournal {
    pub schema: u32,
    pub migration_id: String,
    /// Absolute path of the legacy `awman.db`.
    pub source: PathBuf,
    pub files: Vec<MigrationFileRecord>,
    pub phase: MigrationPhase,
    pub started_at: String,
}

impl MigrationJournal {
    fn file(&self, name: &str) -> Option<&MigrationFileRecord> {
        self.files.iter().find(|f| f.name == name)
    }

    fn copied_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.len).sum()
    }
}

/// A step after which a fault-injection test can abandon a migration as if
/// the process had died there (no cleanup runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationStep {
    JournalWritten,
    Staged,
    Verified,
    FirstFilePublished,
    Published,
    LegacyRetired,
}

/// Failure injection for migration tests. Production callers use none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationFault {
    /// Stop after the step, leaving everything on disk as a crash would.
    CrashAfter(MigrationStep),
    /// Writing a staged copy fails as a full disk does (normal error
    /// handling runs).
    StagingWriteFails,
}

/// Options of [`DataPaths::open_session_store_with`].
#[derive(Debug, Clone)]
pub struct StoreOpenOptions {
    /// Maximum wait for another process holding the migration lock.
    pub lock_wait: Duration,
    pub fault: Option<MigrationFault>,
}

impl Default for StoreOpenOptions {
    fn default() -> Self {
        Self {
            lock_wait: MIGRATION_LOCK_WAIT,
            fault: None,
        }
    }
}

/// Why a migration step stopped: a real failure, or an injected crash that
/// must leave its on-disk state behind untouched.
enum Halt {
    Failed(DataError),
    Crashed(MigrationStep),
}

impl From<DataError> for Halt {
    fn from(error: DataError) -> Self {
        Halt::Failed(error)
    }
}

impl From<Halt> for DataError {
    fn from(halt: Halt) -> Self {
        match halt {
            Halt::Failed(error) => error,
            Halt::Crashed(step) => DataError::Other(format!(
                "database migration abandoned after {step:?} (injected fault)"
            )),
        }
    }
}

type Step<T> = Result<T, Halt>;

impl DataPaths {
    /// Open the shared on-disk session store, relocating a legacy database
    /// from `legacy_root` first. This is the only on-disk opener for
    /// foreground and daemon producers; a failure is returned, never
    /// replaced by another database.
    pub fn open_session_store(
        &self,
        legacy_root: &Path,
    ) -> Result<Arc<SqliteSessionStore>, DataError> {
        let (store, outcome) =
            self.open_session_store_with(legacy_root, &StoreOpenOptions::default())?;
        tracing::info!(
            db = %self.db_path().display(),
            migration = ?outcome,
            "opened the shared session store"
        );
        Ok(store)
    }

    /// [`Self::open_session_store`] with an explicit lock wait and fault
    /// injection; returns what the migration step did.
    pub fn open_session_store_with(
        &self,
        legacy_root: &Path,
        options: &StoreOpenOptions,
    ) -> Result<(Arc<SqliteSessionStore>, MigrationOutcome), DataError> {
        let _lock = self.lock_store(options.lock_wait)?;
        let outcome = migrate_locked(self, legacy_root, options.fault)?;
        let store = SqliteSessionStore::open_at_unlocked(&self.db_path())?;
        Ok((Arc::new(store), outcome))
    }

    /// Relocate a legacy database without opening the result. Prefer
    /// [`Self::open_session_store`], which also opens under the lock.
    pub fn migrate_legacy_db(&self, legacy_root: &Path) -> Result<MigrationOutcome, DataError> {
        let _lock = self.lock_store(MIGRATION_LOCK_WAIT)?;
        migrate_locked(self, legacy_root, None)
    }

    /// The migration journal path.
    pub fn migration_journal_path(&self) -> PathBuf {
        self.root().join(MIGRATION_JOURNAL_FILENAME)
    }

    /// The migration lock path.
    pub fn migration_lock_path(&self) -> PathBuf {
        self.root().join(MIGRATION_LOCK_FILENAME)
    }

    fn lock_store(&self, wait: Duration) -> Result<std::fs::File, DataError> {
        self.ensure_root()?;
        let path = self.migration_lock_path();
        private_files::lock_private_timeout(&path, LockMode::Exclusive, wait, || Ok(()))?
            .ok_or_else(|| {
                DataError::Other(format!(
                    "the awman database in {} is being opened or migrated by another awman \
                     process (lock {} still held after {}s); retry when it finishes",
                    self.root().display(),
                    path.display(),
                    wait.as_secs()
                ))
            })
    }
}

fn migrate_locked(
    paths: &DataPaths,
    legacy_root: &Path,
    fault: Option<MigrationFault>,
) -> Result<MigrationOutcome, DataError> {
    let target = paths.db_path();
    if same_location(legacy_root, paths.root()) {
        // An explicit-root layout keeps the database where the legacy one
        // was: there is nothing to move.
        return Ok(present_outcome(&target));
    }
    let source = std::path::absolute(legacy_root.join(DB_FILENAME))
        .map_err(|e| DataError::io(legacy_root, e))?;
    let journal_path = paths.migration_journal_path();
    let outcome = match private_files::read_private_json::<MigrationJournal>(&journal_path)? {
        Some(journal) => resume(paths, &source, journal, fault),
        None => decide(paths, &source, fault),
    };
    outcome.map_err(DataError::from)
}

fn present_outcome(target: &Path) -> MigrationOutcome {
    if target.exists() {
        MigrationOutcome::AlreadyMigrated
    } else {
        MigrationOutcome::FreshInstall
    }
}

fn same_location(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => match (std::path::absolute(a), std::path::absolute(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        },
    }
}

/// Destination database files that exist, in publish order.
fn destination_files(paths: &DataPaths) -> Vec<&'static str> {
    PUBLISH_ORDER
        .into_iter()
        .filter(|name| paths.root().join(name).symlink_metadata().is_ok())
        .collect()
}

fn legacy_dir(source: &Path) -> &Path {
    source.parent().unwrap_or(Path::new("."))
}

fn conflict(source: &Path, paths: &DataPaths) -> Halt {
    Halt::Failed(DataError::StoreMigrationConflict {
        legacy: source.to_path_buf(),
        current: paths.db_path(),
    })
}

fn crash_point(fault: Option<MigrationFault>, step: MigrationStep) -> Step<()> {
    if fault == Some(MigrationFault::CrashAfter(step)) {
        return Err(Halt::Crashed(step));
    }
    Ok(())
}

/// No journal: choose from what is on disk.
fn decide(
    paths: &DataPaths,
    source: &Path,
    fault: Option<MigrationFault>,
) -> Step<MigrationOutcome> {
    let legacy_present = source.is_file();
    let destination = destination_files(paths);
    match (legacy_present, destination.is_empty()) {
        (false, _) => Ok(present_outcome(&paths.db_path())),
        (true, true) => migrate_fresh(paths, source, fault),
        // Both databases exist and no journal explains the destination: it
        // was written independently (for example by a foreground run).
        (true, false) => Err(conflict(source, paths)),
    }
}

fn migrate_fresh(
    paths: &DataPaths,
    source: &Path,
    fault: Option<MigrationFault>,
) -> Step<MigrationOutcome> {
    let id = uuid::Uuid::new_v4();
    let mut files = Vec::new();
    for name in PUBLISH_ORDER {
        let path = legacy_dir(source).join(name);
        if path.is_file() {
            let (len, sha256) = hash_file(&path)?;
            files.push(MigrationFileRecord {
                name: name.to_owned(),
                len,
                sha256,
            });
        }
    }
    let mut journal = MigrationJournal {
        schema: MIGRATION_JOURNAL_SCHEMA,
        migration_id: id.hyphenated().to_string(),
        source: source.to_path_buf(),
        files,
        phase: MigrationPhase::Staging,
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    write_journal(paths, &journal)?;
    crash_point(fault, MigrationStep::JournalWritten)?;
    let staging = staging_dir(paths, &id);
    if let Err(halt) = stage_and_verify(paths, &mut journal, &staging, fault) {
        if let Halt::Failed(_) = halt {
            // Nothing was published: the legacy files are untouched and
            // the staged copies are ours to drop.
            let _ = discard(paths, &staging);
        }
        return Err(halt);
    }
    let (sidecars, backup_kept) = publish_and_finish(paths, &mut journal, &staging, fault)?;
    Ok(MigrationOutcome::Migrated {
        copied_bytes: journal.copied_bytes(),
        sidecars,
        backup_kept,
    })
}

fn stage_and_verify(
    paths: &DataPaths,
    journal: &mut MigrationJournal,
    staging: &Path,
    fault: Option<MigrationFault>,
) -> Step<()> {
    private_files::create_private_dir_new(staging)?;
    let fail_writes = fault == Some(MigrationFault::StagingWriteFails);
    for file in &journal.files {
        let src = legacy_dir(&journal.source).join(&file.name);
        let (len, sha256) = copy_hashed(&src, &staging.join(&file.name), fail_writes)?;
        if len != file.len || sha256 != file.sha256 {
            return Err(source_changed(&src));
        }
    }
    private_files::sync_private_dir(staging);
    crash_point(fault, MigrationStep::Staged)?;
    for file in &journal.files {
        let staged = staging.join(&file.name);
        let (len, sha256) = hash_file(&staged)?;
        if len != file.len || sha256 != file.sha256 {
            return Err(Halt::Failed(DataError::Other(format!(
                "database migration verification failed for {}; the legacy database was not changed",
                staged.display()
            ))));
        }
    }
    journal.phase = MigrationPhase::Verified;
    write_journal(paths, journal)?;
    crash_point(fault, MigrationStep::Verified)
}

fn source_changed(src: &Path) -> Halt {
    Halt::Failed(DataError::Other(format!(
        "the legacy database {} changed while it was being migrated; stop every awman process \
         (including older versions) and retry",
        src.display()
    )))
}

/// Rename verified staged files into place, retire the legacy files and
/// remove the staging directory and journal. Destination files that already
/// exist must equal their journal hash (a resumed publish).
fn publish_and_finish(
    paths: &DataPaths,
    journal: &mut MigrationJournal,
    staging: &Path,
    fault: Option<MigrationFault>,
) -> Step<(Vec<PathBuf>, bool)> {
    if journal.phase != MigrationPhase::Published {
        journal.phase = MigrationPhase::Publishing;
        write_journal(paths, journal)?;
        let mut renamed = 0;
        for name in PUBLISH_ORDER {
            let Some(record) = journal.file(name).cloned() else {
                continue;
            };
            let destination = paths.root().join(name);
            if destination.symlink_metadata().is_ok() {
                if !matches_record(&destination, &record)? {
                    return Err(conflict(&journal.source, paths));
                }
                continue;
            }
            let staged = staging.join(name);
            if !staged.is_file() || !matches_record(&staged, &record)? {
                restage(journal, &record, &staged)?;
            }
            std::fs::rename(&staged, &destination).map_err(|e| DataError::io(&destination, e))?;
            private_files::sync_private_dir(paths.root());
            renamed += 1;
            if renamed == 1 {
                crash_point(fault, MigrationStep::FirstFilePublished)?;
            }
        }
        journal.phase = MigrationPhase::Published;
        write_journal(paths, journal)?;
        crash_point(fault, MigrationStep::Published)?;
    }
    let retired = retire_legacy(journal)?;
    crash_point(fault, MigrationStep::LegacyRetired)?;
    discard(paths, staging)?;
    Ok(retired)
}

/// Re-copy a file whose staged copy is missing, from a legacy file that
/// still equals its journal hash.
fn restage(journal: &MigrationJournal, record: &MigrationFileRecord, staged: &Path) -> Step<()> {
    let src = legacy_dir(&journal.source).join(&record.name);
    if !src.is_file() || !matches_record(&src, record)? {
        return Err(Halt::Failed(DataError::Other(format!(
            "an interrupted database migration cannot be completed: {} is missing from staging and \
             the legacy copy {} no longer matches; both databases were left as they are",
            record.name,
            src.display()
        ))));
    }
    if staged.symlink_metadata().is_ok() {
        std::fs::remove_file(staged).map_err(|e| DataError::io(staged, e))?;
    }
    if let Some(dir) = staged.parent() {
        if dir.symlink_metadata().is_err() {
            private_files::create_private_dir_new(dir)?;
        }
    }
    let (len, sha256) = copy_hashed(&src, staged, false)?;
    if len != record.len || sha256 != record.sha256 {
        return Err(source_changed(&src));
    }
    Ok(())
}

/// A journal is present: resume it only when it explains what is on disk.
fn resume(
    paths: &DataPaths,
    source: &Path,
    mut journal: MigrationJournal,
    fault: Option<MigrationFault>,
) -> Step<MigrationOutcome> {
    let id = validate_journal(paths, &journal)?;
    let staging = staging_dir(paths, &id);
    let destination = destination_files(paths);
    if journal.source != source {
        if !journal.source.exists() && destination.is_empty() {
            discard(paths, &staging)?;
            return decide(paths, source, fault);
        }
        return Err(Halt::Failed(DataError::Other(format!(
            "the migration journal {} records a migration from {}, not from {}; awman left every \
             database as it is. Stop awman, back up the data directory, and move the journal to \
             another directory once you have confirmed which database to keep",
            paths.migration_journal_path().display(),
            journal.source.display(),
            source.display()
        ))));
    }
    let legacy_present = source.is_file();
    if !legacy_present && destination.is_empty() {
        // Stale: neither the source nor anything published remains.
        discard(paths, &staging)?;
        return Ok(MigrationOutcome::FreshInstall);
    }
    if journal.phase == MigrationPhase::Published && !destination.contains(&DB_FILENAME) {
        // The published database is gone (moved away by the user): the
        // journal explains nothing on disk any more.
        discard(paths, &staging)?;
        return decide(paths, source, fault);
    }
    // Every destination file must be one this migration published, byte for
    // byte, whatever the phase. A `published` phase alone proves nothing
    // about what is at the destination now: the database could have been
    // replaced after the crash, and retiring the legacy files on its word
    // would bless a foreign store. Nothing opens the destination between
    // publication and the end of the migration (both run under the lock),
    // so a genuine resume always matches.
    for name in &destination {
        let explained = match journal.file(name) {
            Some(record) => matches_record(&paths.root().join(name), record)?,
            None => false,
        };
        if !explained {
            return Err(conflict(source, paths));
        }
    }
    if journal.phase != MigrationPhase::Published && destination.is_empty() {
        // Interrupted before anything was published: start over from the
        // untouched legacy files.
        discard(paths, &staging)?;
        return Ok(match migrate_fresh(paths, source, fault)? {
            MigrationOutcome::Migrated { copied_bytes, .. } => {
                MigrationOutcome::RecoveredFromInterrupted { copied_bytes }
            }
            other => other,
        });
    }
    publish_and_finish(paths, &mut journal, &staging, fault)?;
    Ok(MigrationOutcome::RecoveredFromInterrupted {
        copied_bytes: journal.copied_bytes(),
    })
}

fn validate_journal(paths: &DataPaths, journal: &MigrationJournal) -> Step<uuid::Uuid> {
    let unusable = |why: &str| {
        Halt::Failed(DataError::Other(format!(
            "the migration journal {} cannot be used ({why}); awman left every database as it is. \
             Stop awman, back up the data directory, and move the journal to another directory",
            paths.migration_journal_path().display()
        )))
    };
    if journal.schema != MIGRATION_JOURNAL_SCHEMA {
        return Err(unusable(&format!("unsupported schema {}", journal.schema)));
    }
    let id = uuid::Uuid::parse_str(&journal.migration_id)
        .map_err(|_| unusable("invalid migration_id"))?;
    if journal.source.file_name().and_then(|n| n.to_str()) != Some(DB_FILENAME)
        || !journal.source.is_absolute()
    {
        return Err(unusable("invalid source"));
    }
    for (i, file) in journal.files.iter().enumerate() {
        if !PUBLISH_ORDER.contains(&file.name.as_str())
            || journal.files[..i].iter().any(|f| f.name == file.name)
        {
            return Err(unusable("invalid file list"));
        }
    }
    if journal.file(DB_FILENAME).is_none() {
        return Err(unusable("no database file"));
    }
    Ok(id)
}

/// Rename the legacy files aside. A byte-identical copy of the verified
/// destination is removed only when an older backup already holds the
/// backup name; anything else is kept under a migration-specific name.
fn retire_legacy(journal: &MigrationJournal) -> Step<(Vec<PathBuf>, bool)> {
    let mut sidecars = Vec::new();
    let mut backup_kept = true;
    for name in PUBLISH_ORDER {
        let source = legacy_dir(&journal.source).join(name);
        if !source.is_file() {
            continue;
        }
        let backup = with_suffix(&source, ".pre-migration");
        let kept = if backup.symlink_metadata().is_err() {
            std::fs::rename(&source, &backup).map_err(|e| DataError::io(&source, e))?;
            Some(backup)
        } else if match journal.file(name) {
            Some(record) => matches_record(&source, record)?,
            None => false,
        } {
            std::fs::remove_file(&source).map_err(|e| DataError::io(&source, e))?;
            None
        } else {
            let unique = with_suffix(&backup, &format!(".{}", journal.migration_id));
            std::fs::rename(&source, &unique).map_err(|e| DataError::io(&source, e))?;
            Some(unique)
        };
        match kept {
            Some(path) if name != DB_FILENAME => sidecars.push(path),
            None if name == DB_FILENAME => backup_kept = false,
            _ => {}
        }
    }
    private_files::sync_private_dir(legacy_dir(&journal.source));
    Ok((sidecars, backup_kept))
}

/// Remove the staging directory, then the journal.
fn discard(paths: &DataPaths, staging: &Path) -> Step<()> {
    match std::fs::remove_dir_all(staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(DataError::io(staging, e).into()),
    }
    let journal = paths.migration_journal_path();
    match std::fs::remove_file(&journal) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(DataError::io(&journal, e).into()),
    }
    private_files::sync_private_dir(paths.root());
    Ok(())
}

fn write_journal(paths: &DataPaths, journal: &MigrationJournal) -> Step<()> {
    private_files::write_private_json_atomic(&paths.migration_journal_path(), journal, || Ok(()))?;
    Ok(())
}

fn staging_dir(paths: &DataPaths, id: &uuid::Uuid) -> PathBuf {
    paths
        .root()
        .join(format!("{MIGRATION_STAGING_PREFIX}{}", id.hyphenated()))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn matches_record(path: &Path, record: &MigrationFileRecord) -> Result<bool, DataError> {
    let (len, sha256) = hash_file(path)?;
    Ok(len == record.len && sha256 == record.sha256)
}

const COPY_BUFFER: usize = 64 * 1024;

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hash_file(path: &Path) -> Result<(u64, String), DataError> {
    let mut file = std::fs::File::open(path).map_err(|e| DataError::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buffer).map_err(|e| DataError::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        len += n as u64;
    }
    Ok((len, hex(&hasher.finalize())))
}

/// Copy `src` into a new private file at `dst`, fsync it, and return the
/// length and hash of the bytes read from `src`.
fn copy_hashed(src: &Path, dst: &Path, fail_writes: bool) -> Result<(u64, String), DataError> {
    let mut input = std::fs::File::open(src).map_err(|e| DataError::io(src, e))?;
    let mut output = private_files::create_private_file_new(dst)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut len = 0u64;
    loop {
        let n = input.read(&mut buffer).map_err(|e| DataError::io(src, e))?;
        if n == 0 {
            break;
        }
        if fail_writes {
            return Err(DataError::io(
                dst,
                std::io::Error::from(std::io::ErrorKind::StorageFull),
            ));
        }
        output
            .write_all(&buffer[..n])
            .map_err(|e| DataError::io(dst, e))?;
        hasher.update(&buffer[..n]);
        len += n as u64;
    }
    output.sync_all().map_err(|e| DataError::io(dst, e))?;
    Ok((len, hex(&hasher.finalize())))
}
