//! Upgrade the catalog through the lifecycle rig without starting a guest.
use super::*;
use crate::data::builtin_state::{
    applied_migrations, inject_fault, Fault, CATALOG_BACKUP_MANIFEST, CATALOG_RESTORE_JOURNAL,
    RETIRED_SANDBOXES_DIR, RUNTIME_VERSION_FILE,
};
use crate::engine::container::builtin::catalog::{
    CatalogState, CatalogUpgrade, LabelledSandbox, CATALOG_DATABASE, LIFECYCLE_LOCK_DIR,
    MIGRATION_LEDGER,
};
use std::path::{Path, PathBuf};
use std::sync::Barrier;

const OLD: &str = "0.7.2/18/awman-1";

fn database(home: &Path) -> PathBuf {
    home.join("db").join(CATALOG_DATABASE)
}

fn old_catalog(rig: &Rig) {
    let home = &rig.backend.paths.home;
    std::fs::write(home.join(RUNTIME_VERSION_FILE), OLD).unwrap();
    let connection = rusqlite::Connection::open(database(home)).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE seaql_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
         INSERT INTO seaql_migrations VALUES ('old-migration', 1);
         CREATE TABLE payload (value TEXT NOT NULL);
         INSERT INTO payload VALUES ('original row');",
        )
        .unwrap();
    drop(connection);
    std::fs::create_dir(home.join("db/nested")).unwrap();
    std::fs::write(home.join("db/nested/metadata"), b"original metadata").unwrap();
    let db = database(home);
    rig.driver.state.lock().unwrap().upgrade.migrate = Some(Box::new(move || {
        let connection = rusqlite::Connection::open(&db).unwrap();
        // Also proves a retry restored the backup before migrating again.
        let value: String = connection
            .query_row("SELECT value FROM payload", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "original row");
        connection
            .execute_batch(
                "INSERT INTO seaql_migrations VALUES ('new-migration', 2);
             UPDATE payload SET value = 'migrated row';",
            )
            .unwrap();
        Ok(())
    }));
    assert_eq!(
        rig.backend.paths.prepare_catalog().unwrap(),
        CatalogState::NeedsUpgrade { found: OLD.into() }
    );
}

fn tree(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(path, path, &mut files);
    files
}

/// A backup's files without its manifest: what `db/` held when it was taken.
fn backup_tree(backup: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = tree(backup);
    assert!(files.remove(Path::new(CATALOG_BACKUP_MANIFEST)).is_some());
    files
}

fn assert_old_and_no_backup(rig: &Rig, original: &BTreeMap<PathBuf, Vec<u8>>) {
    let paths = &rig.backend.paths;
    assert_eq!(
        std::fs::read_to_string(paths.home.join(RUNTIME_VERSION_FILE)).unwrap(),
        OLD
    );
    assert_eq!(&tree(&paths.home.join("db")), original);
    assert!(!std::fs::read_dir(&paths.home).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("db.pre-")));
}

fn add_sandbox(rig: &Rig, name: &str, protocol: Option<&str>, awman: bool) -> String {
    let mut spec = new_spec(name, "previous-session");
    match protocol {
        Some(protocol) => {
            spec.labels
                .insert(naming::LABEL_PROTOCOL.into(), protocol.into());
        }
        None => {
            spec.labels.remove(naming::LABEL_PROTOCOL);
        }
    }
    if !awman {
        spec.labels.remove(naming::LABEL_AWMAN);
    }
    let id = spec.name.clone();
    // Seed the old labels directly: normal driver operations reject a
    // protocol mismatch, whereas the upgrade must retire these records.
    rig.driver
        .state
        .lock()
        .unwrap()
        .sandboxes
        .push(FakeSandbox {
            spec,
            created: chrono::Utc::now(),
            stopped: true,
            execs: Vec::new(),
            events: None,
            controls: Default::default(),
        });
    id
}

#[test]
fn upgrade_retires_old_sandboxes_and_keeps_current_images_and_oci_cache() {
    let rig = Rig::new("upgrade");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let original = tree(&paths.home.join("db"));
    let old = add_sandbox(&rig, "old", Some(OLD), true);
    let missing = add_sandbox(&rig, "missing-protocol", None, true);
    let current = add_sandbox(&rig, "current", Some(naming::PROTOCOL), true);
    let foreign = add_sandbox(&rig, "foreign", Some(OLD), false);
    let images = rig.driver.state.lock().unwrap().images.clone();
    let cache = paths.home.join("cache/oci/blobs/sha256");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("blob"), b"cached OCI content").unwrap();
    let cached = tree(&paths.home.join("cache"));
    let identities = paths.home.join("awman-images.json");
    std::fs::write(&identities, b"image identity sentinel").unwrap();

    let upgraded = paths.upgrade_catalog(&*rig.driver).unwrap().unwrap();
    assert_eq!(upgraded.found, OLD);
    assert_eq!(upgraded.expected, naming::PROTOCOL);
    assert_eq!(upgraded.state_dir, paths.home);
    assert_eq!(upgraded.retired, 2);
    assert_eq!(upgraded.backup, paths.state().catalog_backup_path(OLD));
    assert_eq!(backup_tree(&upgraded.backup), original);
    assert_eq!(
        upgraded.notice(),
        format!(
            "upgraded builtin state {} from {OLD} to {}; previous catalog saved in {}",
            paths.home.display(),
            naming::PROTOCOL,
            upgraded.backup.display()
        )
    );
    assert_eq!(paths.prepare_catalog().unwrap(), CatalogState::Ready);
    assert_eq!(
        std::fs::read_to_string(paths.home.join(RUNTIME_VERSION_FILE)).unwrap(),
        naming::PROTOCOL
    );
    assert_eq!(rig.driver.names(), vec![current, foreign]);
    let state = rig.driver.state.lock().unwrap();
    assert_eq!(state.upgrade.retired, vec![old, missing]);
    assert_eq!(state.upgrade.migrations, 1);
    assert_eq!(state.upgrade.closes, 1);
    assert_eq!(state.images.len(), images.len());
    for (tag, image) in images {
        let kept = &state.images[&tag];
        assert_eq!(
            (
                &kept.home,
                &kept.user,
                &kept.workdir,
                &kept.argv,
                &kept.accounts.passwd,
                &kept.accounts.group
            ),
            (
                &image.home,
                &image.user,
                &image.workdir,
                &image.argv,
                &image.accounts.passwd,
                &image.accounts.group
            )
        );
    }
    assert!(state.removed_images.is_empty());
    assert_eq!(state.creates, 0, "migration must not boot a guest");
    assert!(state.finished.is_empty());
    assert_eq!(tree(&paths.home.join("cache")), cached);
    assert_eq!(
        std::fs::read(identities).unwrap(),
        b"image identity sentinel"
    );
}

#[test]
fn held_lifecycle_or_import_lock_blocks_upgrade_without_changes_and_exits_one() {
    for lifecycle in [true, false] {
        let rig = Rig::new("blocked");
        old_catalog(&rig);
        add_sandbox(&rig, "old", Some(OLD), true);
        let paths = &rig.backend.paths;
        let original = tree(&paths.home.join("db"));
        let names = rig.driver.names();
        let held = if lifecycle {
            let directory = paths.home.join(LIFECYCLE_LOCK_DIR);
            std::fs::create_dir_all(&directory).unwrap();
            let file = std::fs::File::create(directory.join("live.lock")).unwrap();
            file.lock().unwrap();
            file
        } else {
            paths.import_lock().unwrap()
        };
        let error = paths.upgrade_catalog(&*rig.driver).unwrap_err();
        assert!(
            matches!(&error, EngineError::BuiltinCatalogUpgradeBlocked { state_dir, found } if state_dir == &paths.home && found == OLD),
            "{error:?}"
        );
        assert_eq!(error.exit_code(), 1);
        assert_old_and_no_backup(&rig, &original);
        assert_eq!(rig.driver.names(), names);
        {
            let state = rig.driver.state.lock().unwrap();
            assert_eq!(state.upgrade.migrations, 0);
            assert_eq!(state.upgrade.closes, 0);
            assert!(state.upgrade.retired.is_empty());
            assert!(state.removed_images.is_empty());
        }
        drop(held);
        // The lifecycle lock file remains; an abandoned file is not live.
        assert!(paths.upgrade_catalog(&*rig.driver).unwrap().is_some());
        assert_eq!(paths.prepare_catalog().unwrap(), CatalogState::Ready);
    }
}

#[test]
fn concurrent_upgraders_run_once_and_both_finish_ready() {
    let rig = Rig::new("concurrent");
    old_catalog(&rig);
    let original = tree(&rig.backend.paths.home.join("db"));
    let barrier = Barrier::new(2);
    let upgraded = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    // Independently open the same state directory, and force both
                    // callers to observe NeedsUpgrade before either can commit.
                    let paths = BuiltinPaths::resolve(&rig.backend.paths.home).unwrap();
                    assert_eq!(
                        paths.prepare_catalog().unwrap(),
                        CatalogState::NeedsUpgrade { found: OLD.into() }
                    );
                    barrier.wait();
                    let result = paths.upgrade_catalog(&*rig.driver).unwrap();
                    assert_eq!(paths.prepare_catalog().unwrap(), CatalogState::Ready);
                    result
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        upgraded.len(),
        1,
        "only the winning upgrader emits a notice"
    );
    assert_eq!(backup_tree(&upgraded[0].backup), original);
    let state = rig.driver.state.lock().unwrap();
    assert_eq!(state.upgrade.migrations, 1);
    assert_eq!(state.upgrade.closes, 1);
}

#[test]
fn crash_after_migration_recovers_original_backup_and_retries() {
    let rig = Rig::new("crash");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let original = tree(&paths.home.join("db"));
    rig.driver.state.lock().unwrap().upgrade.crash_before_commit = true;
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        paths.upgrade_catalog(&*rig.driver)
    }));
    assert!(crashed.is_err());
    assert_eq!(
        std::fs::read_to_string(paths.home.join(RUNTIME_VERSION_FILE)).unwrap(),
        OLD
    );
    let backup = paths.state().catalog_backup_path(OLD);
    assert_eq!(backup_tree(&backup), original);
    assert_ne!(tree(&paths.home.join("db")), original);
    let migrations = applied_migrations(&database(&paths.home), MIGRATION_LEDGER)
        .unwrap()
        .unwrap();
    assert_eq!(
        migrations,
        ["old-migration".into(), "new-migration".into()].into()
    );
    assert_eq!(rig.driver.state.lock().unwrap().upgrade.closes, 1);

    rig.driver.state.lock().unwrap().upgrade.crash_before_commit = false;
    let reopened = BuiltinPaths::resolve(&paths.home).unwrap();
    assert_eq!(
        reopened.prepare_catalog().unwrap(),
        CatalogState::NeedsUpgrade { found: OLD.into() }
    );
    assert!(reopened.upgrade_catalog(&*rig.driver).unwrap().is_some());
    assert_eq!(reopened.prepare_catalog().unwrap(), CatalogState::Ready);
    assert_eq!(
        backup_tree(&backup),
        original,
        "the original backup is retained"
    );
    assert_eq!(rig.driver.state.lock().unwrap().upgrade.migrations, 2);
    assert_eq!(rig.driver.state.lock().unwrap().upgrade.closes, 2);
}

struct FailBeforeCommit<'a>(&'a FakeDriver);
impl CatalogUpgrade for FailBeforeCommit<'_> {
    fn migrate(&self) -> Result<(), EngineError> {
        self.0.migrate()
    }
    fn sandbox_labels(&self) -> Result<Vec<LabelledSandbox>, EngineError> {
        self.0.sandbox_labels()
    }
    fn retire_stale_sandbox(&self, id: &str) -> Result<(), EngineError> {
        self.0.retire_stale_sandbox(id)
    }
    fn close(&self) -> Result<(), EngineError> {
        self.0.close()
    }
    fn before_commit(&self) -> Result<(), EngineError> {
        Err(EngineError::Other("commit failed".into()))
    }
}

#[test]
fn migration_retirement_and_commit_errors_restore_catalog_and_allow_retry() {
    for failure in ["migrate", "retire", "commit"] {
        let rig = Rig::new("rollback");
        old_catalog(&rig);
        let paths = &rig.backend.paths;
        let original = tree(&paths.home.join("db"));
        if failure == "migrate" {
            let mut state = rig.driver.state.lock().unwrap();
            let migrate = state.upgrade.migrate.take().unwrap();
            let failed = std::sync::atomic::AtomicBool::new(false);
            state.upgrade.migrate = Some(Box::new(move || {
                migrate()?;
                if !failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    Err(EngineError::Other("migration failed".into()))
                } else {
                    Ok(())
                }
            }));
        } else if failure == "retire" {
            let id = add_sandbox(&rig, "old", Some(OLD), true);
            rig.driver.state.lock().unwrap().upgrade.fail_retire = Some(id);
        }
        let result = if failure == "commit" {
            paths.upgrade_catalog(&FailBeforeCommit(&rig.driver))
        } else {
            paths.upgrade_catalog(&*rig.driver)
        };
        let error = result.unwrap_err();
        let message = match failure {
            "migrate" => "migration failed",
            "retire" => "stale sandbox removal failed",
            _ => "commit failed",
        };
        assert_eq!(error.to_string(), message);
        assert_old_and_no_backup(&rig, &original);
        assert_eq!(rig.driver.state.lock().unwrap().upgrade.closes, 1);
        rig.driver.state.lock().unwrap().upgrade.fail_retire = None;
        assert!(paths.upgrade_catalog(&*rig.driver).unwrap().is_some());
        assert_eq!(paths.prepare_catalog().unwrap(), CatalogState::Ready);
        assert_ne!(tree(&paths.home.join("db")), original);
        assert_eq!(rig.driver.state.lock().unwrap().upgrade.migrations, 2);
        assert_eq!(rig.driver.state.lock().unwrap().upgrade.closes, 2);
    }
}

#[test]
fn existing_backup_without_new_migrations_is_named_and_preserved() {
    for damaged in [
        "unchanged",
        "empty",
        "missing",
        "corrupt",
        "different ledger",
        "empty backup",
        "backup without ledger",
        "backup without manifest",
        "tampered backup",
    ] {
        let rig = Rig::new("existing-backup");
        old_catalog(&rig);
        let paths = &rig.backend.paths;
        let backup = {
            let _lock = paths.lock().unwrap();
            paths
                .state()
                .backup_catalog(OLD, CATALOG_DATABASE, MIGRATION_LEDGER)
                .unwrap()
        };
        let migrate_live = || {
            let connection = rusqlite::Connection::open(database(&paths.home)).unwrap();
            connection
                .execute_batch("INSERT INTO seaql_migrations VALUES ('new-migration', 2);")
                .unwrap();
        };
        match damaged {
            // The live catalog is valid and unmigrated (finding: an empty
            // backup used to count as "no migrations" and replace it).
            "empty backup" => {
                std::fs::remove_dir_all(&backup).unwrap();
                std::fs::create_dir(&backup).unwrap();
            }
            "backup without ledger" => {
                std::fs::remove_file(backup.join(CATALOG_BACKUP_MANIFEST)).unwrap();
                std::fs::remove_file(backup.join(CATALOG_DATABASE)).unwrap();
                rusqlite::Connection::open(backup.join(CATALOG_DATABASE))
                    .unwrap()
                    .execute_batch("CREATE TABLE unrelated (x);")
                    .unwrap();
                migrate_live();
            }
            "backup without manifest" => {
                std::fs::remove_file(backup.join(CATALOG_BACKUP_MANIFEST)).unwrap();
                migrate_live();
            }
            "tampered backup" => {
                std::fs::write(backup.join("nested/metadata"), b"short").unwrap();
                migrate_live();
            }
            _ => {}
        }
        let saved = tree(&backup);
        match damaged {
            "empty" | "missing" => {
                std::fs::remove_dir_all(paths.home.join("db")).unwrap();
                if damaged == "empty" {
                    std::fs::create_dir(paths.home.join("db")).unwrap();
                }
            }
            "corrupt" => std::fs::write(database(&paths.home), b"not SQLite").unwrap(),
            "different ledger" => {
                let connection = rusqlite::Connection::open(database(&paths.home)).unwrap();
                connection.execute_batch("DELETE FROM seaql_migrations; INSERT INTO seaql_migrations VALUES ('other', 2), ('unrelated', 3);").unwrap();
            }
            _ => {}
        }
        let before = paths
            .home
            .join("db")
            .exists()
            .then(|| tree(&paths.home.join("db")));
        let error = paths.upgrade_catalog(&*rig.driver).unwrap_err();
        assert!(
            matches!(&error, EngineError::BuiltinCatalogBackupExists { backup: found, .. } if found == &backup),
            "{error:?}"
        );
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert_eq!(error.exit_code(), 1);
        assert_eq!(tree(&backup), saved);
        assert_eq!(
            paths
                .home
                .join("db")
                .exists()
                .then(|| tree(&paths.home.join("db"))),
            before
        );
        assert_eq!(
            std::fs::read_to_string(paths.home.join(RUNTIME_VERSION_FILE)).unwrap(),
            OLD
        );
        assert_eq!(rig.driver.state.lock().unwrap().upgrade.migrations, 0);
    }
}

fn payload(home: &Path) -> String {
    rusqlite::Connection::open(database(home))
        .unwrap()
        .query_row("SELECT value FROM payload", [], |r| r.get(0))
        .unwrap()
}

/// Run `extra` after the rig's migration on every attempt.
fn after_migrate(rig: &Rig, extra: impl Fn() + Send + 'static) {
    let mut state = rig.driver.state.lock().unwrap();
    let migrate = state.upgrade.migrate.take().unwrap();
    state.upgrade.migrate = Some(Box::new(move || {
        migrate()?;
        extra();
        Ok(())
    }));
}

#[test]
fn a_crash_inside_the_recovery_restore_resumes_from_the_backup() {
    let rig = Rig::new("restore-crash");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let original = tree(&paths.home.join("db"));
    rig.driver.state.lock().unwrap().upgrade.crash_before_commit = true;
    let crash = |paths: &BuiltinPaths| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            paths.upgrade_catalog(&*rig.driver)
        }))
    };
    assert!(crash(paths).is_err());
    rig.driver.state.lock().unwrap().upgrade.crash_before_commit = false;
    // The next attempt restores the migrated catalog from the backup and
    // dies between the two renames: `db/` is gone.
    inject_fault("restore:swap", Fault::Crash);
    assert!(crash(paths).is_err());
    assert!(!paths.home.join("db").exists());
    assert!(paths.home.join(CATALOG_RESTORE_JOURNAL).exists());

    // Resolving recreates an empty `db/`; the journal still proves the
    // backup was being restored, so the upgrade resumes instead of refusing.
    let reopened = BuiltinPaths::resolve(&paths.home).unwrap();
    assert_eq!(
        std::fs::read_dir(reopened.home.join("db")).unwrap().count(),
        0
    );
    let upgraded = reopened.upgrade_catalog(&*rig.driver).unwrap().unwrap();
    assert_eq!(backup_tree(&upgraded.backup), original);
    assert_eq!(reopened.prepare_catalog().unwrap(), CatalogState::Ready);
    // The migration script asserts it saw the original row each time.
    assert_eq!(payload(&reopened.home), "migrated row");
    assert_eq!(rig.driver.state.lock().unwrap().upgrade.migrations, 2);
    assert!(!reopened.home.join(CATALOG_RESTORE_JOURNAL).exists());
}

#[test]
fn a_rollback_whose_restore_fails_keeps_the_backup_and_recovers_next_time() {
    let rig = Rig::new("rollback-restore-fails");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let id = add_sandbox(&rig, "old", Some(OLD), true);
    rig.driver.state.lock().unwrap().upgrade.fail_retire = Some(id);
    inject_fault("restore:swap", Fault::Fail);
    assert!(paths.upgrade_catalog(&*rig.driver).is_err());
    // The swap failed, so the migrated `db/` was put back, the backup kept
    // and the restore journalled.
    assert_eq!(payload(&paths.home), "migrated row");
    let backup = paths.state().catalog_backup_path(OLD);
    assert!(paths.state().verified_catalog_backup(&backup).is_ok());
    assert!(paths.home.join(CATALOG_RESTORE_JOURNAL).exists());
    assert_eq!(
        std::fs::read_to_string(paths.home.join(RUNTIME_VERSION_FILE)).unwrap(),
        OLD
    );
    rig.driver.state.lock().unwrap().upgrade.fail_retire = None;
    assert!(paths.upgrade_catalog(&*rig.driver).unwrap().is_some());
    assert_eq!(paths.prepare_catalog().unwrap(), CatalogState::Ready);
}

#[test]
fn a_rollback_after_a_partial_retirement_puts_the_sandbox_files_back() {
    let rig = Rig::new("partial-retire");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let original = tree(&paths.home.join("db"));
    let first = add_sandbox(&rig, "first", Some(OLD), true);
    let second = add_sandbox(&rig, "second", Some(OLD), true);
    for id in [&first, &second] {
        let directory = paths.home.join("sandboxes").join(id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("disk.img"), id.as_bytes()).unwrap();
    }
    let sandboxes = tree(&paths.home.join("sandboxes"));
    rig.driver.state.lock().unwrap().upgrade.sandboxes_dir = Some(paths.home.join("sandboxes"));
    rig.driver.state.lock().unwrap().upgrade.fail_retire = Some(second.clone());
    assert!(paths.upgrade_catalog(&*rig.driver).is_err());
    assert_eq!(
        rig.driver.state.lock().unwrap().upgrade.retired,
        vec![first.clone()],
        "the first sandbox was removed before the failure"
    );
    assert_old_and_no_backup(&rig, &original);
    assert_eq!(tree(&paths.home.join("sandboxes")), sandboxes);
    assert!(!paths.home.join(RETIRED_SANDBOXES_DIR).exists());

    rig.driver.state.lock().unwrap().upgrade.fail_retire = None;
    assert!(paths.upgrade_catalog(&*rig.driver).unwrap().is_some());
    assert!(!paths.home.join("sandboxes").join(&second).exists());
    assert!(!paths.home.join(RETIRED_SANDBOXES_DIR).exists());
}

#[test]
fn a_failure_after_the_new_marker_is_visible_is_not_rolled_back() {
    let rig = Rig::new("marker-sync");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    inject_fault(&format!("publish:{RUNTIME_VERSION_FILE}:sync"), Fault::Fail);
    assert!(paths.upgrade_catalog(&*rig.driver).is_err());
    assert_eq!(
        std::fs::read_to_string(paths.home.join(RUNTIME_VERSION_FILE)).unwrap(),
        naming::PROTOCOL
    );
    assert_eq!(
        payload(&paths.home),
        "migrated row",
        "a new marker must never sit next to the old catalog"
    );
    assert!(paths
        .state()
        .verified_catalog_backup(&paths.state().catalog_backup_path(OLD))
        .is_ok());
    assert_eq!(paths.prepare_catalog().unwrap(), CatalogState::Ready);
}

#[test]
fn a_backup_that_cannot_be_made_durable_is_removed_and_nothing_changes() {
    let rig = Rig::new("backup-sync");
    old_catalog(&rig);
    let original = tree(&rig.backend.paths.home.join("db"));
    inject_fault("backup:sync", Fault::Fail);
    assert!(rig.backend.paths.upgrade_catalog(&*rig.driver).is_err());
    assert_old_and_no_backup(&rig, &original);
    assert_eq!(rig.driver.state.lock().unwrap().upgrade.migrations, 0);
}

#[test]
fn an_older_awman_cannot_launch_or_import_while_the_upgrade_runs() {
    // A launch of the older awman that is already planned holds its image
    // lease (shared): the gate refuses.
    let rig = Rig::new("fence");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let original = tree(&paths.home.join("db"));
    let planned = paths.image_lease("img:old", false).unwrap();
    assert!(matches!(
        paths.upgrade_catalog(&*rig.driver),
        Err(EngineError::BuiltinCatalogUpgradeBlocked { .. })
    ));
    assert_old_and_no_backup(&rig, &original);
    drop(planned);

    // Once past the gate, a launch or import that resumes during the
    // migration fails at once or waits, instead of writing the catalog.
    let observed = Arc::new(Mutex::new(Vec::new()));
    let home = paths.home.clone();
    let seen = observed.clone();
    after_migrate(&rig, move || {
        let paths = BuiltinPaths::resolve(&home).unwrap();
        let state = paths.state();
        seen.lock().unwrap().push((
            paths.image_lease("img:old", false).is_err(),
            state
                .lock_controlled(
                    crate::data::builtin_state::IMPORT_LOCK,
                    Duration::ZERO,
                    false,
                    || Ok(()),
                )
                .is_err(),
        ));
    });
    assert!(paths.upgrade_catalog(&*rig.driver).unwrap().is_some());
    assert_eq!(*observed.lock().unwrap(), vec![(true, true)]);
    // The fence is gone after the upgrade.
    assert!(paths.image_lease("img:old", false).is_ok());
}

#[test]
fn a_vm_started_during_the_upgrade_blocks_the_commit_and_rolls_back() {
    let rig = Rig::new("late-vm");
    old_catalog(&rig);
    let paths = &rig.backend.paths;
    let original = tree(&paths.home.join("db"));
    let held = Arc::new(Mutex::new(None));
    let locks = paths.home.join(LIFECYCLE_LOCK_DIR);
    let vm = held.clone();
    after_migrate(&rig, move || {
        // A VM of the older awman for an image it had no lease file for.
        std::fs::create_dir_all(&locks).unwrap();
        let file = std::fs::File::create(locks.join("late.lock")).unwrap();
        file.lock().unwrap();
        *vm.lock().unwrap() = Some(file);
    });
    assert!(matches!(
        paths.upgrade_catalog(&*rig.driver),
        Err(EngineError::BuiltinCatalogUpgradeBlocked { .. })
    ));
    assert_old_and_no_backup(&rig, &original);
    held.lock().unwrap().take();
}
