//! The one migration-aware store opener: journaled, verified relocation of a
//! legacy database, conflict preservation, interruption, lock contention and
//! storage failures, all on private sandbox state.
use crate::support;
use awman::data::error::DataError;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::data_paths::{
    DataPaths, MigrationFault, MigrationJournal, MigrationOutcome, MigrationPhase, MigrationStep,
    StoreOpenOptions, DB_FILENAME, MIGRATION_STAGING_PREFIX,
};
use awman::data::ids::WorkerSessionId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FILES: [&str; 3] = ["awman.db", "awman.db-wal", "awman.db-shm"];

/// A database at `dir` holding one session row in an uncheckpointed WAL,
/// so the fixture has a live `-wal` and `-shm` beside `awman.db`.
fn database_with_live_wal(scratch: &Path, dir: &Path, workdir: &str) -> String {
    std::fs::create_dir_all(scratch).unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let id = WorkerSessionId::new().as_str().to_owned();
    let store = SqliteSessionStore::open(scratch).unwrap();
    store
        .with_conn(|conn| {
            conn.execute_batch("PRAGMA wal_autocheckpoint=0;")?;
            Ok(())
        })
        .unwrap();
    store
        .insert_session(&id, workdir, "2026-11-20T00:00:00Z")
        .unwrap();
    for name in FILES {
        let from = scratch.join(name);
        assert!(from.is_file(), "fixture is missing {name}");
        std::fs::copy(&from, dir.join(name)).unwrap();
    }
    drop(store);
    id
}

fn snapshot(dir: &Path) -> BTreeMap<&'static str, Vec<u8>> {
    FILES
        .into_iter()
        .filter_map(|name| std::fs::read(dir.join(name)).ok().map(|b| (name, b)))
        .collect()
}

fn staging_dirs(paths: &DataPaths) -> Vec<PathBuf> {
    std::fs::read_dir(paths.root())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(MIGRATION_STAGING_PREFIX))
        })
        .collect()
}

fn journal(paths: &DataPaths) -> Option<MigrationJournal> {
    std::fs::read(paths.migration_journal_path())
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap())
}

fn open(
    paths: &DataPaths,
    legacy: &Path,
) -> Result<(SqliteSessionStore, MigrationOutcome), DataError> {
    paths
        .open_session_store_with(legacy, &StoreOpenOptions::default())
        .map(|(store, outcome)| {
            (
                std::sync::Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store is shared")),
                outcome,
            )
        })
}

fn faulted(paths: &DataPaths, legacy: &Path, fault: MigrationFault) -> DataError {
    paths
        .open_session_store_with(
            legacy,
            &StoreOpenOptions {
                fault: Some(fault),
                ..Default::default()
            },
        )
        .err()
        .expect("the injected fault stops the open")
}

struct Layout {
    _sandbox: awman_test_support::TestSandbox,
    tmp: PathBuf,
    legacy: PathBuf,
    paths: DataPaths,
}

fn layout(name: &'static str) -> Layout {
    let sandbox = support::sandbox(name);
    let tmp = sandbox.tmp().to_path_buf();
    Layout {
        legacy: tmp.join("home/api"),
        paths: DataPaths::at_root(tmp.join("home/data")),
        tmp,
        _sandbox: sandbox,
    }
}

#[test]
fn fresh_migration_publishes_verified_copies_and_keeps_legacy_backups() {
    let l = layout(
        "data::store_migration::fresh_migration_publishes_verified_copies_and_keeps_legacy_backups",
    );
    let session = database_with_live_wal(&l.tmp.join("scratch"), &l.legacy, "/legacy/repo");
    let before = snapshot(&l.legacy);

    let (store, outcome) = open(&l.paths, &l.legacy).unwrap();
    let MigrationOutcome::Migrated {
        copied_bytes,
        sidecars,
        backup_kept,
    } = outcome
    else {
        panic!("expected a fresh migration, got {outcome:?}");
    };
    assert_eq!(
        copied_bytes,
        before.values().map(|b| b.len() as u64).sum::<u64>()
    );
    assert!(backup_kept);
    assert_eq!(sidecars.len(), 2);
    assert_eq!(
        store.get_session(&session).unwrap().unwrap().workdir,
        "/legacy/repo"
    );
    // The legacy files are renamed aside byte-for-byte, never deleted.
    for (name, bytes) in &before {
        assert!(!l.legacy.join(name).exists());
        assert_eq!(
            &std::fs::read(l.legacy.join(format!("{name}.pre-migration"))).unwrap(),
            bytes
        );
    }
    assert!(journal(&l.paths).is_none());
    assert!(staging_dirs(&l.paths).is_empty());
    drop(store);

    // Every later open finds the migrated store and moves nothing.
    let (store, outcome) = open(&l.paths, &l.legacy).unwrap();
    assert_eq!(outcome, MigrationOutcome::AlreadyMigrated);
    assert!(store.get_session(&session).unwrap().is_some());
}

#[test]
fn independently_populated_stores_conflict_and_every_file_is_preserved() {
    let l = layout(
        "data::store_migration::independently_populated_stores_conflict_and_every_file_is_preserved",
    );
    let legacy_session = database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    let current_session = database_with_live_wal(&l.tmp.join("b"), l.paths.root(), "/current");
    let legacy_before = snapshot(&l.legacy);
    let current_before = snapshot(l.paths.root());
    assert_eq!(legacy_before.len(), 3);
    assert_eq!(current_before.len(), 3);

    for _ in 0..2 {
        let error = open(&l.paths, &l.legacy).err().expect("conflict");
        let DataError::StoreMigrationConflict { legacy, current } = &error else {
            panic!("expected a conflict, got {error}");
        };
        assert_eq!(
            legacy,
            &std::path::absolute(l.legacy.join(DB_FILENAME)).unwrap()
        );
        assert_eq!(current, &l.paths.db_path());
        let message = error.to_string();
        assert!(message.contains(&legacy.display().to_string()), "{message}");
        assert!(
            message.contains(&current.display().to_string()),
            "{message}"
        );
        assert!(message.contains("Both were left untouched"), "{message}");
        assert!(message.contains("back up both"), "{message}");
        assert!(message.contains("do not delete it"), "{message}");
        assert_eq!(snapshot(&l.legacy), legacy_before);
        assert_eq!(snapshot(l.paths.root()), current_before);
        assert!(journal(&l.paths).is_none());
        assert!(staging_dirs(&l.paths).is_empty());
    }

    // Following the guidance (move the unwanted store aside) resolves it,
    // and the kept store still has its own records.
    let aside = l.tmp.join("moved-aside");
    std::fs::create_dir_all(&aside).unwrap();
    for name in FILES {
        std::fs::rename(l.legacy.join(name), aside.join(name)).unwrap();
    }
    let (store, outcome) = open(&l.paths, &l.legacy).unwrap();
    assert_eq!(outcome, MigrationOutcome::AlreadyMigrated);
    assert!(store.get_session(&current_session).unwrap().is_some());
    assert!(store.get_session(&legacy_session).unwrap().is_none());
    assert_eq!(snapshot(&aside), legacy_before);
}

#[test]
fn interruption_at_every_step_resumes_to_one_complete_store() {
    let steps = [
        MigrationStep::JournalWritten,
        MigrationStep::Staged,
        MigrationStep::Verified,
        MigrationStep::FirstFilePublished,
        MigrationStep::Published,
        MigrationStep::LegacyRetired,
    ];
    let l =
        layout("data::store_migration::interruption_at_every_step_resumes_to_one_complete_store");
    for (i, step) in steps.into_iter().enumerate() {
        let legacy = l.legacy.join(i.to_string());
        let paths = DataPaths::at_root(l.paths.root().join(i.to_string()));
        let session =
            database_with_live_wal(&l.tmp.join(format!("scratch-{i}")), &legacy, "/legacy");
        let before = snapshot(&legacy);

        let error = faulted(&paths, &legacy, MigrationFault::CrashAfter(step));
        assert!(
            error.to_string().contains("injected fault"),
            "{step:?}: {error}"
        );
        let recorded = journal(&paths).unwrap_or_else(|| panic!("{step:?}: journal kept"));
        assert_eq!(recorded.schema, 1);
        assert_eq!(recorded.files.len(), 3);
        let expected_phase = match step {
            MigrationStep::JournalWritten | MigrationStep::Staged => MigrationPhase::Staging,
            MigrationStep::Verified => MigrationPhase::Verified,
            MigrationStep::FirstFilePublished => MigrationPhase::Publishing,
            MigrationStep::Published | MigrationStep::LegacyRetired => MigrationPhase::Published,
        };
        assert_eq!(recorded.phase, expected_phase, "{step:?}");
        if expected_phase != MigrationPhase::Published {
            // Originals are untouched until the publish is recorded.
            assert_eq!(snapshot(&legacy), before, "{step:?}");
        }

        let (store, outcome) = open(&paths, &legacy).unwrap();
        assert!(
            matches!(outcome, MigrationOutcome::RecoveredFromInterrupted { .. }),
            "{step:?}: {outcome:?}"
        );
        assert!(store.get_session(&session).unwrap().is_some(), "{step:?}");
        assert!(journal(&paths).is_none(), "{step:?}");
        assert!(staging_dirs(&paths).is_empty(), "{step:?}");
        assert_eq!(
            std::fs::read(legacy.join("awman.db.pre-migration")).unwrap(),
            before["awman.db"],
            "{step:?}: legacy backup kept"
        );
    }
}

#[test]
fn a_journal_does_not_explain_a_destination_it_did_not_publish() {
    let l = layout(
        "data::store_migration::a_journal_does_not_explain_a_destination_it_did_not_publish",
    );
    // Interrupted before publishing; then something else created a store.
    database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    faulted(
        &l.paths,
        &l.legacy,
        MigrationFault::CrashAfter(MigrationStep::Verified),
    );
    database_with_live_wal(&l.tmp.join("b"), l.paths.root(), "/independent");
    let legacy_before = snapshot(&l.legacy);
    let current_before = snapshot(l.paths.root());
    let error = open(&l.paths, &l.legacy).err().expect("conflict");
    assert!(
        matches!(error, DataError::StoreMigrationConflict { .. }),
        "{error}"
    );
    assert_eq!(snapshot(&l.legacy), legacy_before);
    assert_eq!(snapshot(l.paths.root()), current_before);

    // A partially published destination whose sidecar changed is no
    // longer the migration's either.
    let legacy = l.tmp.join("second/api");
    let paths = DataPaths::at_root(l.tmp.join("second/data"));
    database_with_live_wal(&l.tmp.join("c"), &legacy, "/legacy");
    faulted(
        &paths,
        &legacy,
        MigrationFault::CrashAfter(MigrationStep::FirstFilePublished),
    );
    let wal = paths.root().join("awman.db-wal");
    assert!(wal.is_file());
    std::fs::write(&wal, b"written by someone else").unwrap();
    let error = open(&paths, &legacy).err().expect("conflict");
    assert!(
        matches!(error, DataError::StoreMigrationConflict { .. }),
        "{error}"
    );
    assert_eq!(std::fs::read(&wal).unwrap(), b"written by someone else");
    assert!(legacy.join(DB_FILENAME).is_file());
}

/// A `published` journal proves nothing about what is at the destination
/// now. A store that replaced the published one after the crash is a
/// conflict: it is not opened, and the legacy database is neither removed
/// nor renamed, even when an older backup occupies its backup name.
#[test]
fn a_published_journal_does_not_bless_a_replaced_destination() {
    let l =
        layout("data::store_migration::a_published_journal_does_not_bless_a_replaced_destination");
    database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    // An unrelated older backup holds the name retirement would use.
    std::fs::write(l.legacy.join("awman.db.pre-migration"), b"older backup").unwrap();
    faulted(
        &l.paths,
        &l.legacy,
        MigrationFault::CrashAfter(MigrationStep::Published),
    );
    assert_eq!(journal(&l.paths).unwrap().phase, MigrationPhase::Published);
    // The published files are replaced by an independently populated store.
    for name in FILES {
        let _ = std::fs::remove_file(l.paths.root().join(name));
    }
    database_with_live_wal(&l.tmp.join("b"), l.paths.root(), "/foreign");
    let legacy_before = snapshot(&l.legacy);
    let current_before = snapshot(l.paths.root());

    let error = open(&l.paths, &l.legacy).map(|_| ()).expect_err("conflict");
    assert!(
        matches!(error, DataError::StoreMigrationConflict { .. }),
        "{error}"
    );
    assert_eq!(snapshot(&l.legacy), legacy_before, "legacy kept in place");
    assert_eq!(snapshot(l.paths.root()), current_before);
    assert_eq!(
        std::fs::read(l.legacy.join("awman.db.pre-migration")).unwrap(),
        b"older backup"
    );
    assert!(
        journal(&l.paths).is_some(),
        "the journal is kept for the user"
    );

    // A store written after publication (a sidecar the migration never
    // published) is not the migration's either.
    let legacy = l.tmp.join("second/api");
    let paths = DataPaths::at_root(l.tmp.join("second/data"));
    database_with_live_wal(&l.tmp.join("c"), &legacy, "/legacy");
    std::fs::remove_file(legacy.join("awman.db-shm")).unwrap();
    faulted(
        &paths,
        &legacy,
        MigrationFault::CrashAfter(MigrationStep::Published),
    );
    std::fs::write(paths.root().join("awman.db-shm"), b"opened since").unwrap();
    let error = open(&paths, &legacy).map(|_| ()).expect_err("conflict");
    assert!(
        matches!(error, DataError::StoreMigrationConflict { .. }),
        "{error}"
    );
    assert!(legacy.join(DB_FILENAME).is_file());
}

#[test]
fn stale_journals_without_source_or_destination_are_removed() {
    let l =
        layout("data::store_migration::stale_journals_without_source_or_destination_are_removed");
    for source in [
        std::path::absolute(l.legacy.join(DB_FILENAME)).unwrap(),
        l.tmp.join("elsewhere").join(DB_FILENAME),
    ] {
        l.paths.ensure_root().unwrap();
        let id = uuid::Uuid::new_v4();
        let staging = l
            .paths
            .root()
            .join(format!("{MIGRATION_STAGING_PREFIX}{}", id.hyphenated()));
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join(DB_FILENAME), b"half a copy").unwrap();
        let journal = MigrationJournal {
            schema: 1,
            migration_id: id.hyphenated().to_string(),
            source,
            files: vec![awman::data::fs::data_paths::MigrationFileRecord {
                name: DB_FILENAME.into(),
                len: 4096,
                sha256: "00".repeat(32),
            }],
            phase: MigrationPhase::Staging,
            started_at: "2026-11-20T00:00:00Z".into(),
        };
        awman::data::fs::private_files::write_private_json_atomic(
            &l.paths.migration_journal_path(),
            &journal,
            || Ok(()),
        )
        .unwrap();

        let (store, outcome) = open(&l.paths, &l.legacy).unwrap();
        assert_eq!(outcome, MigrationOutcome::FreshInstall);
        assert!(!l.paths.migration_journal_path().exists());
        assert!(!staging.exists());
        drop(store);
        for name in FILES {
            let _ = std::fs::remove_file(l.paths.root().join(name));
        }
    }
}

#[test]
fn storage_full_while_staging_leaves_the_legacy_store_untouched() {
    let l = layout(
        "data::store_migration::storage_full_while_staging_leaves_the_legacy_store_untouched",
    );
    let session = database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    let before = snapshot(&l.legacy);
    let error = faulted(&l.paths, &l.legacy, MigrationFault::StagingWriteFails);
    let DataError::Io { source, .. } = &error else {
        panic!("expected an I/O error, got {error}");
    };
    assert_eq!(source.kind(), std::io::ErrorKind::StorageFull);
    assert_eq!(snapshot(&l.legacy), before);
    assert!(snapshot(l.paths.root()).is_empty());
    assert!(journal(&l.paths).is_none());
    assert!(staging_dirs(&l.paths).is_empty());

    let (store, outcome) = open(&l.paths, &l.legacy).unwrap();
    assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
    assert!(store.get_session(&session).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn a_read_only_data_root_fails_before_touching_the_legacy_store() {
    use std::os::unix::fs::PermissionsExt;
    let l = layout(
        "data::store_migration::a_read_only_data_root_fails_before_touching_the_legacy_store",
    );
    database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    let before = snapshot(&l.legacy);
    l.paths.ensure_root().unwrap();
    std::fs::set_permissions(l.paths.root(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = open(&l.paths, &l.legacy).map(|_| ());
    std::fs::set_permissions(l.paths.root(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.expect_err("a read-only data root cannot be migrated into");
    // The lock cannot even be created: refused before any decision, as
    // the I/O error it is (not as a symlink the user should remove).
    assert!(matches!(error, DataError::Io { .. }), "{error}");
    assert!(!error.to_string().contains("symlink"), "{error}");
    assert!(error.to_string().contains("Permission denied"), "{error}");
    assert_eq!(snapshot(&l.legacy), before);
    assert!(snapshot(l.paths.root()).is_empty());
}

#[test]
fn lock_contention_is_bounded_and_migrates_nothing() {
    let l = layout("data::store_migration::lock_contention_is_bounded_and_migrates_nothing");
    database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    let before = snapshot(&l.legacy);
    l.paths.ensure_root().unwrap();
    let held = awman::data::fs::private_files::lock_private_exclusive(
        &l.paths.migration_lock_path(),
        || Ok(()),
    )
    .unwrap();
    let error = l
        .paths
        .open_session_store_with(
            &l.legacy,
            &StoreOpenOptions {
                lock_wait: Duration::from_millis(100),
                fault: None,
            },
        )
        .err()
        .expect("the lock is held");
    assert!(
        error.to_string().contains("another awman process"),
        "{error}"
    );
    assert_eq!(snapshot(&l.legacy), before);
    assert!(snapshot(l.paths.root()).is_empty());
    drop(held);
    assert!(matches!(
        open(&l.paths, &l.legacy).unwrap().1,
        MigrationOutcome::Migrated { .. }
    ));
}

/// A direct open of the shared database (the opener lease, image and
/// work-server bookkeeping use) waits for a migration in progress instead of
/// creating the destination beside the legacy store. Refused while the
/// migration holds the lock; afterwards it opens the migrated store, and the
/// migration-aware opener still finds one consistent database.
#[test]
fn a_direct_open_never_creates_the_destination_during_a_migration() {
    let l = layout(
        "data::store_migration::a_direct_open_never_creates_the_destination_during_a_migration",
    );
    let id = database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    l.paths.ensure_root().unwrap();
    let held = awman::data::fs::private_files::lock_private_exclusive(
        &l.paths.migration_lock_path(),
        || Ok(()),
    )
    .unwrap();
    let error =
        SqliteSessionStore::open_at_with_lock_wait(&l.paths.db_path(), Duration::from_millis(100))
            .map(|_| ())
            .expect_err("the migration holds the lock");
    assert!(error.to_string().contains("being migrated"), "{error}");
    assert!(snapshot(l.paths.root()).is_empty(), "nothing created");
    drop(held);

    let (_, outcome) = open(&l.paths, &l.legacy).unwrap();
    assert!(
        matches!(outcome, MigrationOutcome::Migrated { .. }),
        "{outcome:?}"
    );
    let direct = SqliteSessionStore::open_at(&l.paths.db_path()).unwrap();
    assert!(
        direct.get_session(&id).unwrap().is_some(),
        "the migrated row"
    );
    drop(direct);
    assert!(matches!(
        open(&l.paths, &l.legacy).unwrap().1,
        MigrationOutcome::AlreadyMigrated
    ));
}

#[test]
fn concurrent_opens_migrate_once_and_share_one_database() {
    let l = layout("data::store_migration::concurrent_opens_migrate_once_and_share_one_database");
    let session = database_with_live_wal(&l.tmp.join("a"), &l.legacy, "/legacy");
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (paths, legacy, session) = (&l.paths, &l.legacy, &session);
                scope.spawn(move || {
                    let store = paths.open_session_store(legacy).unwrap();
                    let saw_legacy = store.get_session(session).unwrap().is_some();
                    let own = WorkerSessionId::new().as_str().to_owned();
                    store
                        .insert_session(&own, &format!("/writer/{i}"), "2026-11-20T00:00:00Z")
                        .unwrap();
                    (saw_legacy, own)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(outcomes.iter().all(|(saw_legacy, _)| *saw_legacy));
    let (store, outcome) = open(&l.paths, &l.legacy).unwrap();
    assert_eq!(outcome, MigrationOutcome::AlreadyMigrated);
    for (_, own) in &outcomes {
        assert!(store.get_session(own).unwrap().is_some());
    }
    // Exactly one migration ran: one backup, no second copy.
    assert!(l.legacy.join("awman.db.pre-migration").is_file());
    assert!(!l.legacy.join(DB_FILENAME).exists());
}

#[test]
fn an_explicit_root_layout_is_never_migrated_onto_itself() {
    let l = layout("data::store_migration::an_explicit_root_layout_is_never_migrated_onto_itself");
    let session = database_with_live_wal(&l.tmp.join("a"), l.paths.root(), "/same");
    let before = snapshot(l.paths.root());
    let (store, outcome) = open(&l.paths, l.paths.root()).unwrap();
    assert_eq!(outcome, MigrationOutcome::AlreadyMigrated);
    assert!(store.get_session(&session).unwrap().is_some());
    drop(store);
    assert!(!l.paths.root().join("awman.db.pre-migration").exists());
    assert!(!before.is_empty());
}
