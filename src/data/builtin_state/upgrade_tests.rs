//! Filesystem and protocol boundaries of in-place catalog upgrades.
use super::*;
use crate::data::builtin_runtime::{UPGRADABLE_FROM, WORKER_PROTOCOL};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

fn state() -> (tempfile::TempDir, BuiltinState) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let state = BuiltinState::resolve(&temp.path().join("s"), |_| Ok(())).unwrap();
    (temp, state)
}

fn assert_no_backup(state: &BuiltinState) {
    for entry in std::fs::read_dir(&state.home).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(!name.to_string_lossy().contains("db.pre-"), "{name:?}");
    }
}

#[test]
fn only_exact_allowlisted_markers_are_upgradable() {
    let (_temp, state) = state();
    let marker = state.home.join(RUNTIME_VERSION_FILE);
    assert!(!UPGRADABLE_FROM.is_empty());
    for &old in UPGRADABLE_FROM {
        std::fs::write(&marker, old).unwrap();
        assert_eq!(
            state.check_runtime_marker(WORKER_PROTOCOL).unwrap(),
            RuntimeMarker::Upgradable { found: old.into() }
        );
        for near_match in [
            old[..old.len() - 1].to_owned(),
            format!("{old}/extra"),
            format!("{old}\n"),
            format!(" {old}"),
            format!("{old}\0"),
        ] {
            std::fs::write(&marker, &near_match).unwrap();
            assert_eq!(
                state.check_runtime_marker(WORKER_PROTOCOL).unwrap(),
                RuntimeMarker::Mismatch {
                    found: near_match.clone()
                }
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), near_match);
        }
    }
    for unknown in [
        "0.7.2",
        "0.7.2/18",
        "0.7.9/18/awman-1",
        "99.0.0/99/awman-99",
    ] {
        std::fs::write(&marker, unknown).unwrap();
        assert_eq!(
            state.check_runtime_marker(WORKER_PROTOCOL).unwrap(),
            RuntimeMarker::Mismatch {
                found: unknown.into()
            }
        );
    }
    std::fs::write(&marker, WORKER_PROTOCOL).unwrap();
    assert_eq!(
        state.check_runtime_marker(WORKER_PROTOCOL).unwrap(),
        RuntimeMarker::Matches
    );
    std::fs::remove_file(&marker).unwrap();
    std::fs::write(state.home.join("db/old.sqlite"), b"existing catalog").unwrap();
    assert_eq!(
        state.check_runtime_marker(WORKER_PROTOCOL).unwrap(),
        RuntimeMarker::Mismatch {
            found: "unversioned existing catalog".into()
        }
    );
    assert!(!marker.exists());
    assert_no_backup(&state);
}

#[test]
fn backup_catalog_copies_nested_content_with_private_permissions() {
    let (_temp, state) = state();
    let _lock = state.lock().unwrap();
    let db = state.home.join("db");
    std::fs::create_dir(db.join("nested")).unwrap();
    for path in [db.join("catalog"), db.join("nested/wal")] {
        std::fs::write(&path, b"original bytes").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).unwrap();
    }
    std::fs::set_permissions(db.join("nested"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let backup = state
        .backup_catalog("0.7.2/18/awman-1", "msb.db", "seaql_migrations")
        .unwrap();
    assert_eq!(backup, state.home.join("db.pre-0.7.2_18_awman-1"));
    for (relative, mode) in [
        ("", 0o700),
        ("nested", 0o700),
        ("catalog", 0o600),
        ("nested/wal", 0o600),
    ] {
        assert_eq!(
            std::fs::metadata(backup.join(relative)).unwrap().mode() & 0o777,
            mode
        );
    }
    for relative in ["catalog", "nested/wal"] {
        assert_eq!(
            std::fs::read(backup.join(relative)).unwrap(),
            b"original bytes"
        );
        std::fs::write(db.join(relative), b"changed").unwrap();
        assert_eq!(
            std::fs::read(backup.join(relative)).unwrap(),
            b"original bytes"
        );
    }
    assert_eq!(
        state.catalog_backup_path("../a/🦀"),
        state.home.join("db.pre-.._a__")
    );
    assert_eq!(state.catalog_backup_path(""), state.home.join("db.pre-_"));
}

#[test]
fn backup_catalog_refuses_existing_targets_and_names_them() {
    for kind in ["directory", "file", "dangling symlink"] {
        let (_temp, state) = state();
        let _lock = state.lock().unwrap();
        let target = state.catalog_backup_path("old/protocol");
        match kind {
            "directory" => std::fs::create_dir(&target).unwrap(),
            "file" => std::fs::write(&target, b"keep me").unwrap(),
            _ => symlink(state.home.join("absent"), &target).unwrap(),
        }
        let error = state
            .backup_catalog("old/protocol", "msb.db", "seaql_migrations")
            .unwrap_err();
        assert!(
            matches!(&error, DataError::BuiltinState(message) if message.contains("already exists") && message.contains(&target.display().to_string())),
            "{error}"
        );
        assert!(target.symlink_metadata().is_ok());
        if kind == "file" {
            assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        }
        assert!(!std::fs::read_dir(&state.home).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".partial-")));
    }
}

#[test]
fn backup_catalog_refuses_symlinked_db_without_touching_target() {
    let (temp, state) = state();
    let _lock = state.lock().unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("catalog"), b"outside").unwrap();
    std::fs::remove_dir(state.home.join("db")).unwrap();
    symlink(&outside, state.home.join("db")).unwrap();
    let error = state
        .backup_catalog("old", "msb.db", "seaql_migrations")
        .unwrap_err();
    assert!(
        matches!(error, DataError::BuiltinState(message) if message.contains("symlinks") && message.contains(&state.home.join("db").display().to_string()))
    );
    assert_eq!(std::fs::read(outside.join("catalog")).unwrap(), b"outside");
    assert_no_backup(&state);
}

#[test]
fn backup_catalog_removes_partial_copy_when_nested_entry_is_refused() {
    for kind in ["file symlink", "directory symlink", "socket"] {
        let (temp, state) = state();
        let _lock = state.lock().unwrap();
        let nested = state.home.join("db/nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("good"), b"keep original").unwrap();
        let outside = temp.path().join("outside");
        let bad = nested.join("bad");
        let _socket = match kind {
            "file symlink" => {
                std::fs::write(&outside, b"outside").unwrap();
                symlink(&outside, &bad).unwrap();
                None
            }
            "directory symlink" => {
                std::fs::create_dir(&outside).unwrap();
                symlink(&outside, &bad).unwrap();
                None
            }
            _ => Some(std::os::unix::net::UnixListener::bind(&bad).unwrap()),
        };
        // The nested staging directory must have been created before the bad
        // entry is reached, regardless of filesystem enumeration order.
        let error = state
            .backup_catalog("old", "msb.db", "seaql_migrations")
            .unwrap_err();
        assert!(
            matches!(error, DataError::BuiltinState(message) if message.contains(&bad.display().to_string()))
        );
        assert_eq!(
            std::fs::read(nested.join("good")).unwrap(),
            b"keep original"
        );
        assert!(bad.symlink_metadata().is_ok());
        assert_no_backup(&state);
    }
}

#[test]
fn replace_runtime_marker_write_failure_preserves_old_marker() {
    const CHILD: &str = "AWMAN_TEST_MARKER_WRITE_FAILURE";
    if let Some(home) = std::env::var_os(CHILD) {
        let state = BuiltinState::at(home);
        assert!(matches!(
            state.replace_runtime_marker(WORKER_PROTOCOL),
            Err(DataError::Io { .. })
        ));
        return;
    }
    let (_temp, state) = state();
    let marker = state.home.join(RUNTIME_VERSION_FILE);
    std::fs::write(&marker, b"old protocol").unwrap();
    let inode = std::fs::metadata(&marker).unwrap().ino();
    // Apply the fault only in the child. A zero file-size limit forces a
    // staged write failure even as root; ignoring SIGXFSZ exposes the error.
    let output = std::process::Command::new("bash")
        .args(["-c", "trap '' XFSZ; ulimit -f 0; exec \"$@\"", "marker-write-failure"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "data::builtin_state::upgrade_tests::replace_runtime_marker_write_failure_preserves_old_marker", "--nocapture"])
        .env(CHILD, &state.home)
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the child must run exactly this test: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"old protocol");
    assert_eq!(std::fs::metadata(&marker).unwrap().ino(), inode);
    assert_eq!(
        std::fs::read_dir(&state.home).unwrap().count(),
        STATE_SUBDIRS.len() + 1,
        "failed staging file was removed"
    );
}

#[test]
fn a_crash_before_the_marker_rename_leaves_the_old_marker() {
    let (_temp, state) = state();
    let marker = state.home.join(RUNTIME_VERSION_FILE);
    std::fs::write(&marker, UPGRADABLE_FROM[0]).unwrap();
    let inode = std::fs::metadata(&marker).unwrap().ino();
    inject_fault(
        &format!("publish:{RUNTIME_VERSION_FILE}:rename"),
        Fault::Crash,
    );
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.replace_runtime_marker(WORKER_PROTOCOL)
    }));
    assert!(crashed.is_err(), "the fault point was not reached");
    // The new protocol was fully staged, but the crash came before the rename.
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        UPGRADABLE_FROM[0]
    );
    assert_eq!(std::fs::metadata(&marker).unwrap().ino(), inode);
    assert_eq!(
        state.check_runtime_marker(WORKER_PROTOCOL).unwrap(),
        RuntimeMarker::Upgradable {
            found: UPGRADABLE_FROM[0].into()
        },
        "the next run retries the upgrade"
    );
    state.replace_runtime_marker(WORKER_PROTOCOL).unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), WORKER_PROTOCOL);
}

#[test]
fn replace_runtime_marker_publishes_a_new_private_inode() {
    let (_temp, state) = state();
    let marker = state.home.join(RUNTIME_VERSION_FILE);
    std::fs::write(&marker, b"old protocol").unwrap();
    let mut old = File::open(&marker).unwrap();
    state.replace_runtime_marker(WORKER_PROTOCOL).unwrap();
    let mut old_bytes = String::new();
    old.read_to_string(&mut old_bytes).unwrap();
    assert_eq!(
        old_bytes, "old protocol",
        "replacement must not truncate the old inode"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), WORKER_PROTOCOL);
    assert_ne!(
        old.metadata().unwrap().ino(),
        std::fs::metadata(&marker).unwrap().ino()
    );
    assert_eq!(std::fs::metadata(&marker).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn restore_catalog_checkpoints_saved_wal_and_removes_staging_directories() {
    let (_temp, state) = state();
    let _lock = state.lock().unwrap();
    let db = state.home.join("db/msb.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
         PRAGMA wal_autocheckpoint=0;
         CREATE TABLE payload (value TEXT);
         INSERT INTO payload VALUES ('saved in WAL');",
        )
        .unwrap();
    assert!(state.home.join("db/msb.db-wal").exists());
    let backup = state
        .backup_catalog("old", "msb.db", "seaql_migrations")
        .unwrap();
    // The backup is self-contained: its WAL was folded into its database.
    assert!(std::fs::metadata(backup.join("msb.db-wal")).map_or(true, |m| m.len() == 0));
    let saved = std::fs::read(backup.join("msb.db")).unwrap();
    connection
        .execute("UPDATE payload SET value = 'new data'", [])
        .unwrap();
    drop(connection);

    state.restore_catalog(&backup, "msb.db").unwrap();
    assert!(
        !state.home.join("db/msb.db-wal").exists(),
        "restore must fold the saved WAL into the main file"
    );
    let restored = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT value FROM payload", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "saved in WAL"
    );
    drop(restored);
    assert_eq!(std::fs::read(backup.join("msb.db")).unwrap(), saved);
    assert!(!state.home.join(CATALOG_RESTORE_JOURNAL).exists());
    assert!(!std::fs::read_dir(&state.home).unwrap().any(|e| {
        let name = e.unwrap().file_name();
        let name = name.to_string_lossy();
        name.starts_with(".db.restore-") || name.starts_with(".db.discard-")
    }));
}

#[test]
fn restore_and_discard_refuse_symlinked_or_unrelated_backup_paths() {
    let (temp, state) = state();
    let _lock = state.lock().unwrap();
    std::fs::write(state.home.join("db/catalog"), b"original").unwrap();
    let backup = state
        .backup_catalog("old", "msb.db", "seaql_migrations")
        .unwrap();
    let link = state.home.join("db.pre-symlink");
    symlink(&backup, &link).unwrap();
    assert!(state.restore_catalog(&link, "msb.db").is_err());
    assert!(state.discard_catalog_backup(&link).is_err());
    let unrelated = temp.path().join("db.pre-outside");
    std::fs::create_dir(&unrelated).unwrap();
    for path in [&state.home.join("db"), &unrelated] {
        assert!(state.discard_catalog_backup(path).is_err());
        assert!(path.is_dir());
    }
    assert_eq!(
        std::fs::read(state.home.join("db/catalog")).unwrap(),
        b"original"
    );
    state.discard_catalog_backup(&backup).unwrap();
    assert!(!backup.exists());
}

#[test]
fn applied_migrations_reads_ledger_without_changing_catalog() {
    let (_temp, state) = state();
    let db = state.home.join("db/msb.db");
    assert_eq!(applied_migrations(&db, "ledger").unwrap(), None);
    assert!(!db.exists());
    let connection = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(applied_migrations(&db, "ledger").unwrap(), None);
    connection
        .execute_batch(
            "CREATE TABLE ledger (version TEXT); INSERT INTO ledger VALUES ('b'), ('a');",
        )
        .unwrap();
    drop(connection);
    let before = std::fs::read(&db).unwrap();
    assert_eq!(
        applied_migrations(&db, "ledger").unwrap(),
        Some(["a".into(), "b".into()].into())
    );
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert!(applied_migrations(&db, "ledger; DROP TABLE ledger").is_err());
    let link = state.home.join("db/linked.db");
    symlink(&db, &link).unwrap();
    assert!(applied_migrations(&link, "ledger").is_err());
}

#[test]
fn held_locks_ignores_stale_files_symlinks_and_directories() {
    let (_temp, state) = state();
    assert!(state.held_locks("run/locks").unwrap().is_empty());
    let locks = state.home.join("run/locks");
    std::fs::create_dir(&locks).unwrap();
    let held_path = locks.join("held.lock");
    let held = File::create(&held_path).unwrap();
    held.lock().unwrap();
    File::create(locks.join("abandoned.lock")).unwrap();
    std::fs::create_dir(locks.join("directory")).unwrap();
    symlink(&held_path, locks.join("symlink.lock")).unwrap();
    assert_eq!(state.held_locks("run/locks").unwrap(), vec![held_path]);
    drop(held);
    assert!(state.held_locks("run/locks").unwrap().is_empty());
}

fn ledger_catalog(state: &BuiltinState) {
    let connection = rusqlite::Connection::open(state.home.join("db/msb.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE seaql_migrations (version TEXT PRIMARY KEY);
             INSERT INTO seaql_migrations VALUES ('a'), ('b');
             CREATE TABLE payload (value TEXT);
             INSERT INTO payload VALUES ('original');",
        )
        .unwrap();
    std::fs::create_dir(state.home.join("db/nested")).unwrap();
    std::fs::write(state.home.join("db/nested/metadata"), b"metadata").unwrap();
}

fn payload(state: &BuiltinState) -> String {
    rusqlite::Connection::open(state.home.join("db/msb.db"))
        .unwrap()
        .query_row("SELECT value FROM payload", [], |r| r.get(0))
        .unwrap()
}

fn set_payload(state: &BuiltinState, value: &str) {
    rusqlite::Connection::open(state.home.join("db/msb.db"))
        .unwrap()
        .execute("UPDATE payload SET value = ?1", [value])
        .unwrap();
}

fn backup(state: &BuiltinState) -> PathBuf {
    state
        .backup_catalog("old", "msb.db", "seaql_migrations")
        .unwrap()
}

fn hidden_leftovers(state: &BuiltinState) -> Vec<String> {
    std::fs::read_dir(&state.home)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".partial-") || name.starts_with(".db.restore-"))
        .collect()
}

#[test]
fn backup_manifest_records_migrations_and_damaged_backups_are_never_restored() {
    let (_temp, state) = state();
    let _lock = state.lock().unwrap();
    ledger_catalog(&state);
    let saved = backup(&state);
    let manifest = state.verified_catalog_backup(&saved).unwrap();
    assert_eq!(manifest.from, "old");
    assert_eq!(manifest.migrations, Some(["a".into(), "b".into()].into()));
    set_payload(&state, "changed");
    state.restore_catalog(&saved, "msb.db").unwrap();
    assert_eq!(payload(&state), "original");
    assert!(
        !state.home.join("db").join(CATALOG_BACKUP_MANIFEST).exists(),
        "the manifest is not part of the catalog"
    );

    for damage in [
        "no manifest",
        "grown file",
        "extra file",
        "missing file",
        "empty directory",
    ] {
        std::fs::remove_dir_all(&saved).unwrap();
        if damage == "empty directory" {
            std::fs::create_dir(&saved).unwrap();
        } else {
            backup(&state);
        }
        match damage {
            "no manifest" => std::fs::remove_file(saved.join(CATALOG_BACKUP_MANIFEST)).unwrap(),
            "grown file" => {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(saved.join("nested/metadata"))
                    .unwrap();
                file.write_all(b"!").unwrap();
            }
            "extra file" => std::fs::write(saved.join("stray"), b"").unwrap(),
            "missing file" => std::fs::remove_file(saved.join("nested/metadata")).unwrap(),
            _ => {}
        }
        set_payload(&state, "live");
        assert!(state.verified_catalog_backup(&saved).is_err(), "{damage}");
        assert!(state.restore_catalog(&saved, "msb.db").is_err(), "{damage}");
        assert_eq!(payload(&state), "live", "{damage}");
        assert!(!state.home.join(CATALOG_RESTORE_JOURNAL).exists());
    }
}

#[test]
fn a_crash_between_the_restore_renames_is_journalled_and_repeated() {
    let (_temp, state) = state();
    {
        let _lock = state.lock().unwrap();
        ledger_catalog(&state);
        let saved = backup(&state);
        set_payload(&state, "migrated");
        inject_fault("restore:swap", Fault::Crash);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.restore_catalog(&saved, "msb.db")
        }));
        assert!(crashed.is_err());
        assert!(!state.home.join("db").exists(), "db/ was displaced");
        assert_eq!(state.pending_catalog_restore().unwrap(), Some(saved));
    }
    // The next run recreates an empty db/; the journal still names the backup.
    let state = BuiltinState::resolve(&state.home, |_| Ok(())).unwrap();
    let _lock = state.lock().unwrap();
    let saved = state.pending_catalog_restore().unwrap().unwrap();
    state.restore_catalog(&saved, "msb.db").unwrap();
    assert_eq!(payload(&state), "original");
    assert_eq!(state.pending_catalog_restore().unwrap(), None);
    assert!(state.verified_catalog_backup(&saved).is_ok());
}

#[test]
fn a_failed_restore_swap_puts_the_displaced_catalog_back() {
    let (_temp, state) = state();
    let _lock = state.lock().unwrap();
    ledger_catalog(&state);
    let saved = backup(&state);
    set_payload(&state, "migrated");
    inject_fault("restore:swap", Fault::Fail);
    assert!(state.restore_catalog(&saved, "msb.db").is_err());
    assert_eq!(payload(&state), "migrated");
    assert_eq!(
        state.pending_catalog_restore().unwrap(),
        Some(saved.clone())
    );
    assert!(hidden_leftovers(&state).is_empty());
    state.restore_catalog(&saved, "msb.db").unwrap();
    assert_eq!(payload(&state), "original");
}

#[test]
fn directory_sync_failures_are_reported_and_keep_the_last_good_copy() {
    let (_temp, state) = state();
    let _lock = state.lock().unwrap();
    ledger_catalog(&state);
    inject_fault("backup:sync", Fault::Fail);
    assert!(matches!(
        state.backup_catalog("old", "msb.db", "seaql_migrations"),
        Err(DataError::Io { .. })
    ));
    assert_no_backup(&state);
    assert!(hidden_leftovers(&state).is_empty());
    assert_eq!(payload(&state), "original");

    let saved = backup(&state);
    set_payload(&state, "migrated");
    inject_fault("restore:sync", Fault::Fail);
    assert!(state.restore_catalog(&saved, "msb.db").is_err());
    assert_eq!(
        state.pending_catalog_restore().unwrap(),
        Some(saved.clone()),
        "an unsynced restore stays journalled"
    );
    assert!(state.verified_catalog_backup(&saved).is_ok());

    std::fs::write(state.home.join(RUNTIME_VERSION_FILE), "old").unwrap();
    inject_fault(&format!("publish:{RUNTIME_VERSION_FILE}:sync"), Fault::Fail);
    assert!(state.replace_runtime_marker(WORKER_PROTOCOL).is_err());
    assert_eq!(
        std::fs::read_to_string(state.home.join(RUNTIME_VERSION_FILE)).unwrap(),
        WORKER_PROTOCOL,
        "the error came after the rename"
    );
}

#[test]
fn paths_substituted_during_a_backup_are_refused_not_followed() {
    for race in ["directory", "file", "claimed name"] {
        let (temp, state) = state();
        let _lock = state.lock().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"outside").unwrap();
        let db = state.home.join("db");
        std::fs::create_dir(db.join("nested")).unwrap();
        std::fs::write(db.join("nested/file"), b"inside").unwrap();
        let target = state.catalog_backup_path("old");
        let (point, swap): (&str, Box<dyn FnMut()>) = match race {
            "directory" => ("copy:open-dir", {
                let (db, outside) = (db.clone(), outside.clone());
                Box::new(move || {
                    std::fs::rename(db.join("nested"), db.join("moved")).unwrap();
                    symlink(&outside, db.join("nested")).unwrap();
                })
            }),
            "file" => ("copy:open-file", {
                let (db, outside) = (db.clone(), outside.clone());
                Box::new(move || {
                    std::fs::remove_file(db.join("nested/file")).unwrap();
                    symlink(outside.join("secret"), db.join("nested/file")).unwrap();
                })
            }),
            _ => ("backup:claim", {
                let target = target.clone();
                Box::new(move || std::fs::create_dir(&target).unwrap())
            }),
        };
        inject_fault(point, Fault::Run(swap));
        let error = state
            .backup_catalog("old", "msb.db", "seaql_migrations")
            .unwrap_err();
        let expected = if race == "claimed name" {
            "already exists"
        } else {
            "symlink"
        };
        assert!(error.to_string().contains(expected), "{race}: {error}");
        assert!(hidden_leftovers(&state).is_empty(), "{race}");
        if race == "claimed name" {
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        } else {
            assert_no_backup(&state);
        }
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"outside");
    }
}

#[test]
fn backup_on_a_full_disk_removes_the_partial_copy() {
    const CHILD: &str = "AWMAN_TEST_BACKUP_DISK_FULL";
    if let Some(home) = std::env::var_os(CHILD) {
        let state = BuiltinState::at(home);
        assert!(matches!(
            state.backup_catalog("old", "msb.db", "seaql_migrations"),
            Err(DataError::Io { .. })
        ));
        return;
    }
    let (_temp, state) = state();
    let big = vec![7u8; 256 * 1024];
    std::fs::write(state.home.join("db/catalog"), &big).unwrap();
    // A file-size limit in the child makes the copy fail with EFBIG, as a
    // full disk would, without filling the real disk.
    let output = std::process::Command::new("bash")
        .args([
            "-c",
            "trap '' XFSZ; ulimit -f 64; exec \"$@\"",
            "backup-disk-full",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data::builtin_state::upgrade_tests::backup_on_a_full_disk_removes_the_partial_copy",
            "--nocapture",
        ])
        .env(CHILD, &state.home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert_no_backup(&state);
    assert!(hidden_leftovers(&state).is_empty());
    assert_eq!(std::fs::read(state.home.join("db/catalog")).unwrap(), big);
}

#[test]
fn retired_sandbox_files_move_aside_and_back() {
    let (_temp, state) = state();
    let sandboxes = state.home.join("sandboxes");
    for name in ["a", "b"] {
        std::fs::create_dir(sandboxes.join(name)).unwrap();
        std::fs::write(sandboxes.join(name).join("disk"), name).unwrap();
    }
    state.retire_sandbox_files("a").unwrap();
    state.retire_sandbox_files("b").unwrap();
    state.retire_sandbox_files("never-existed").unwrap();
    let retired = state.home.join(RETIRED_SANDBOXES_DIR);
    assert!(!sandboxes.join("a").exists());
    assert_eq!(std::fs::read(retired.join("b/disk")).unwrap(), b"b");
    state.restore_retired_sandbox_files().unwrap();
    for name in ["a", "b"] {
        assert_eq!(
            std::fs::read(sandboxes.join(name).join("disk")).unwrap(),
            name.as_bytes()
        );
    }
    assert!(!retired.exists());
    state.restore_retired_sandbox_files().unwrap();
    state.retire_sandbox_files("a").unwrap();
    state.discard_retired_sandbox_files().unwrap();
    assert!(!retired.exists() && !sandboxes.join("a").exists());
    assert!(sandboxes.join("b").exists());
    for invalid in ["", ".", "..", "a/b"] {
        assert!(state.retire_sandbox_files(invalid).is_err(), "{invalid:?}");
    }
}

#[test]
fn existing_image_leases_are_taken_exclusively_or_reported_busy() {
    let (_temp, state) = state();
    assert!(state.lock_image_leases().unwrap().is_empty());
    let launch = state.image_lease("image:tag", false).unwrap();
    assert!(matches!(
        state.lock_image_leases(),
        Err(DataError::BuiltinStateBusy { .. })
    ));
    drop(launch);
    let held = after_forked_copies_close(|| state.lock_image_leases()).unwrap();
    assert_eq!(held.len(), 1);
    assert!(matches!(
        state.image_lease("image:tag", false),
        Err(DataError::BuiltinStateBusy { .. })
    ));
    drop(held);
    assert!(after_forked_copies_close(|| state.image_lease("image:tag", false)).is_ok());
}
