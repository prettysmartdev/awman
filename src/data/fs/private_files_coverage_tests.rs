//! Permission-mode and locking coverage for the private-file helpers.

use super::*;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o777
}

fn try_lock(path: &Path, mode: LockMode) -> Option<File> {
    lock_private_timeout(path, mode, Duration::ZERO, || Ok(())).unwrap()
}

#[test]
fn create_private_dir_creates_nested_owner_only_dirs_and_is_idempotent() {
    let root = private_root();
    let nested = root.path().join("a").join("b");
    create_private_dir(&nested).unwrap();
    assert_eq!(mode(&nested), 0o700);
    assert_eq!(mode(&root.path().join("a")), 0o700);
    create_private_dir(&nested).unwrap();
}

#[test]
fn create_private_dir_refuses_an_existing_group_or_world_accessible_dir() {
    let root = private_root();
    let dir = root.path().join("open");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    let err = create_private_dir(&dir).unwrap_err();
    assert!(matches!(err, DataError::InvalidPath { .. }), "{err:?}");
    assert!(
        err.to_string().contains("accessible to other users"),
        "{err}"
    );
    // Refused, never silently tightened.
    assert_eq!(mode(&dir), 0o750);
}

#[test]
fn create_private_dir_refuses_a_regular_file() {
    let root = private_root();
    let file = root.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(create_private_dir(&file).is_err());
}

#[test]
fn create_private_dir_new_requires_an_absent_dir_and_an_existing_parent() {
    let root = private_root();
    let dir = root.path().join("fresh");
    create_private_dir_new(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert!(matches!(
        create_private_dir_new(&dir),
        Err(DataError::Io { .. })
    ));
    assert!(create_private_dir_new(&root.path().join("missing").join("child")).is_err());
}

#[test]
fn open_private_lock_absent_without_create_is_none_and_create_is_owner_only() {
    let root = private_root();
    let path = root.path().join("lock");
    assert!(open_private_lock(&path, false).unwrap().is_none());
    assert!(!path.exists());
    assert!(open_private_lock(&path, true).unwrap().is_some());
    assert_eq!(mode(&path), 0o600);
    assert!(open_private_lock(&path, false).unwrap().is_some());
}

#[test]
fn open_private_lock_refuses_hard_links_public_modes_and_non_files() {
    let root = private_root();
    let path = root.path().join("lock");
    drop(open_private_lock(&path, true).unwrap());

    let alias = root.path().join("alias");
    std::fs::hard_link(&path, &alias).unwrap();
    let err = open_private_lock(&path, false).unwrap_err();
    assert!(err.to_string().contains("hard links"), "{err}");
    std::fs::remove_file(&alias).unwrap();
    assert!(open_private_lock(&path, false).unwrap().is_some());

    for bad in [0o640, 0o604, 0o660, 0o602] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bad)).unwrap();
        let err = open_private_lock(&path, false).unwrap_err();
        assert!(err.to_string().contains("accessible"), "{bad:o}: {err}");
    }
    // Owner-only but stricter than 0600 is still private for reading.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(open_private(&path, false, false).unwrap().is_some());

    let dir = root.path().join("dir");
    std::fs::create_dir(&dir).unwrap();
    assert!(open_private(&dir, false, false).is_err());
}

#[test]
fn creating_through_a_dangling_symlink_never_creates_the_target() {
    let root = private_root();
    let target = root.path().join("target");
    let link = root.path().join("lock");
    symlink(&target, &link).unwrap();
    assert!(open_private_lock(&link, true).is_err());
    assert!(lock_private_exclusive(&link, || Ok(())).is_err());
    assert!(lock_private_timeout(&link, LockMode::Shared, Duration::ZERO, || Ok(())).is_err());
    assert!(!target.exists());
}

#[test]
fn exclusive_lock_excludes_every_other_holder_until_dropped() {
    let root = private_root();
    let path = root.path().join("lock");
    let held = lock_private_exclusive(&path, || Ok(())).unwrap();
    assert_eq!(mode(&path), 0o600);
    assert!(try_lock(&path, LockMode::Exclusive).is_none());
    assert!(try_lock(&path, LockMode::Shared).is_none());
    drop(held);
    assert!(try_lock(&path, LockMode::Exclusive).is_some());
}

#[test]
fn shared_locks_coexist_but_exclude_an_exclusive_one() {
    let root = private_root();
    let path = root.path().join("lock");
    let first = lock_private(&path, LockMode::Shared, || Ok(())).unwrap();
    let second = lock_private(&path, LockMode::Shared, || Ok(())).unwrap();
    assert!(try_lock(&path, LockMode::Exclusive).is_none());
    drop(first);
    assert!(try_lock(&path, LockMode::Exclusive).is_none());
    drop(second);
    assert!(try_lock(&path, LockMode::Exclusive).is_some());
}

#[test]
fn lock_check_runs_before_and_after_acquisition_and_can_abort() {
    let root = private_root();
    let path = root.path().join("lock");

    let calls = std::cell::Cell::new(0);
    let file = lock_private_exclusive(&path, || {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .unwrap();
    // Once before the successful attempt, once after it.
    assert_eq!(calls.get(), 2);
    drop(file);

    // A check that refuses after acquisition releases the lock again.
    let calls = std::cell::Cell::new(0);
    let err = lock_private_exclusive(&path, || {
        calls.set(calls.get() + 1);
        if calls.get() == 2 {
            Err(DataError::Other("cancelled".into()))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(matches!(err, DataError::Other(ref m) if m == "cancelled"));
    assert!(
        try_lock(&path, LockMode::Exclusive).is_some(),
        "an abandoned lock must not stay held"
    );
}

#[test]
fn a_waiter_on_a_busy_lock_is_abandoned_by_its_check() {
    let root = private_root();
    let path = root.path().join("lock");
    let _held = lock_private_exclusive(&path, || Ok(())).unwrap();
    let err = lock_private(&path, LockMode::Exclusive, || {
        Err(DataError::Other("deadline".into()))
    })
    .unwrap_err();
    assert!(matches!(err, DataError::Other(ref m) if m == "deadline"));
    let err = lock_private_timeout(&path, LockMode::Shared, Duration::from_secs(3600), || {
        Err(DataError::Other("deadline".into()))
    })
    .unwrap_err();
    assert!(matches!(err, DataError::Other(ref m) if m == "deadline"));
}

#[test]
fn atomic_write_is_owner_only_replaces_and_leaves_no_staging_file() {
    let root = private_root();
    let record = root.path().join("record.json");
    write_private_json_atomic(&record, &serde_json::json!({"v": 1}), || Ok(())).unwrap();
    assert_eq!(mode(&record), 0o600);
    write_private_json_atomic(&record, &serde_json::json!({"v": 2}), || Ok(())).unwrap();
    assert_eq!(
        read_private_json::<serde_json::Value>(&record).unwrap(),
        Some(serde_json::json!({"v": 2}))
    );
    let names: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("record.json")]);
}

#[test]
fn atomic_write_aborted_by_check_keeps_the_old_record_and_cleans_up() {
    let root = private_root();
    let record = root.path().join("record.json");
    write_private_json_atomic(&record, &serde_json::json!({"v": 1}), || Ok(())).unwrap();
    let err = write_private_json_atomic(&record, &serde_json::json!({"v": 2}), || {
        Err(DataError::Other("lost lease".into()))
    })
    .unwrap_err();
    assert!(matches!(err, DataError::Other(_)));
    assert_eq!(
        read_private_json::<serde_json::Value>(&record).unwrap(),
        Some(serde_json::json!({"v": 1}))
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn read_private_json_absent_is_none_and_garbage_is_an_error() {
    let root = private_root();
    let record = root.path().join("record.json");
    assert!(read_private_json::<serde_json::Value>(&record)
        .unwrap()
        .is_none());
    let mut file = create_private_file_new(&record).unwrap();
    file.write_all(b"{not json").unwrap();
    drop(file);
    let err = read_private_json::<serde_json::Value>(&record).unwrap_err();
    assert!(err.to_string().contains("not a valid record"), "{err}");
}

#[test]
fn json_or_miss_treats_everything_unusable_as_a_miss() {
    let root = private_root();
    let record = root.path().join("r.json");
    assert!(read_private_json_or_miss::<serde_json::Value>(&record, 64)
        .unwrap()
        .is_none());

    std::fs::write(&record, b"{\"a\":1}").unwrap();
    assert_eq!(
        read_private_json_or_miss::<serde_json::Value>(&record, 64).unwrap(),
        Some(serde_json::json!({"a": 1}))
    );
    // Exactly at the limit is accepted; one byte under it is a miss.
    assert!(read_private_json_or_miss::<serde_json::Value>(&record, 7)
        .unwrap()
        .is_some());
    assert!(read_private_json_or_miss::<serde_json::Value>(&record, 6)
        .unwrap()
        .is_none());

    let link = root.path().join("link.json");
    symlink(&record, &link).unwrap();
    assert!(read_private_json_or_miss::<serde_json::Value>(&link, 64)
        .unwrap()
        .is_none());

    let dir = root.path().join("dir.json");
    std::fs::create_dir(&dir).unwrap();
    assert!(read_private_json_or_miss::<serde_json::Value>(&dir, 64)
        .unwrap()
        .is_none());

    std::fs::write(&record, b"garbage").unwrap();
    assert!(read_private_json_or_miss::<serde_json::Value>(&record, 64)
        .unwrap()
        .is_none());
}

#[test]
fn approval_read_requires_exactly_0600_and_a_bounded_size() {
    let root = private_root();
    let record = root.path().join("approval.json");
    assert!(read_private_approval_json::<serde_json::Value>(&record, 64)
        .unwrap()
        .is_none());
    write_private_json_atomic(&record, &serde_json::json!({"ok": true}), || Ok(())).unwrap();
    assert_eq!(
        read_private_approval_json::<serde_json::Value>(&record, 64).unwrap(),
        Some(serde_json::json!({"ok": true}))
    );
    assert!(read_private_approval_json::<serde_json::Value>(&record, 4)
        .unwrap()
        .is_none());
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(read_private_approval_json::<serde_json::Value>(&record, 64)
        .unwrap()
        .is_none());
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_private_approval_json::<serde_json::Value>(&record, 64).is_err());
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.path().join("link.json");
    symlink(&record, &link).unwrap();
    assert!(read_private_approval_json::<serde_json::Value>(&link, 64).is_err());
}

#[test]
fn interrupted_write_names_and_remove_semantics() {
    assert!(is_interrupted_private_write(Path::new(
        "/x/.record.json.0123abcd.tmp"
    )));
    assert!(!is_interrupted_private_write(Path::new("/x/record.json")));
    assert!(!is_interrupted_private_write(Path::new("/x/record.tmp")));
    assert!(!is_interrupted_private_write(Path::new("/x/.hidden")));

    let root = private_root();
    let path = root.path().join("gone");
    remove_private_file(&path).unwrap();
    std::fs::write(&path, b"").unwrap();
    remove_private_file(&path).unwrap();
    assert!(!path.exists());
    sync_private_dir(root.path());
}
