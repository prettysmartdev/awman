//! Owner-only stores through the production `private_files` API: creation,
//! refusal of links, foreign owners and broad access before anything is
//! written, and atomic replacement that never writes through an existing
//! entry. Unix cases check modes and owners; Windows cases check owner SIDs,
//! DACLs, junctions and hard links on a native host.
use awman::data::error::DataError;
use awman::data::fs::private_files::*;
use serde_json::{json, Value};
use std::path::Path;

fn refusal<T: std::fmt::Debug>(result: Result<T, DataError>) -> String {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.to_string(),
    }
}

fn read(path: &Path) -> Option<Value> {
    read_private_json::<Value>(path).unwrap()
}

/// Shared by both platforms: a record replaced in place keeps the previous
/// value until the rename, a failed `check` leaves no sibling behind, and an
/// absent record (or absent directory) reads as `None`.
#[test]
fn replacement_is_atomic_and_absent_records_read_as_none() {
    let s = crate::support::sandbox(
        "data::private_files::replacement_is_atomic_and_absent_records_read_as_none",
    );
    let store = s.root().join("store");
    create_private_dir(&store).unwrap();
    let record = store.join("record.json");
    assert_eq!(read(&record), None);
    assert_eq!(read(&store.join("missing/record.json")), None);

    write_private_json_atomic(&record, &json!({"v": 1}), || Ok(())).unwrap();
    assert_eq!(read(&record), Some(json!({"v": 1})));
    write_private_json_atomic(&record, &json!({"v": 2}), || Ok(())).unwrap();
    assert_eq!(read(&record), Some(json!({"v": 2})));

    let failed = write_private_json_atomic(&record, &json!({"v": 3}), || {
        Err(DataError::Other("abandoned".into()))
    });
    assert!(failed.is_err());
    assert_eq!(read(&record), Some(json!({"v": 2})));
    let leftovers: Vec<_> = std::fs::read_dir(&store)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| is_interrupted_private_write(p))
        .collect();
    assert!(leftovers.is_empty(), "staging siblings left: {leftovers:?}");

    // A lock file is created private and can be locked again after release.
    let lock = store.join("store.lock");
    drop(lock_private_exclusive(&lock, || Ok(())).unwrap());
    drop(lock_private_exclusive(&lock, || Ok(())).unwrap());
}

/// Replacing a record that has a second hard link swaps the directory entry:
/// the other name keeps the old bytes and the new record has one link.
#[test]
fn replacement_never_writes_through_a_hard_link() {
    let s = crate::support::sandbox(
        "data::private_files::replacement_never_writes_through_a_hard_link",
    );
    let store = s.root().join("store");
    create_private_dir(&store).unwrap();
    let record = store.join("record.json");
    write_private_json_atomic(&record, &json!({"v": 1}), || Ok(())).unwrap();
    let other = store.join("other.json");
    std::fs::hard_link(&record, &other).unwrap();

    let why = refusal(open_private_file(&record));
    assert!(why.contains("hard links"), "{why}");
    let why = refusal(read_private_json::<Value>(&other));
    assert!(why.contains("hard links"), "{why}");

    write_private_json_atomic(&record, &json!({"v": 2}), || Ok(())).unwrap();
    assert_eq!(read(&record), Some(json!({"v": 2})));
    let old: Value = serde_json::from_slice(&std::fs::read(&other).unwrap()).unwrap();
    assert_eq!(old, json!({"v": 1}));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn created_objects_are_owner_only_and_broad_or_linked_ones_are_refused_unchanged() {
        let s = crate::support::sandbox(
            "data::private_files::unix::created_objects_are_owner_only_and_broad_or_linked_ones_are_refused_unchanged",
        );
        let root = s.root().join("data");
        let store = root.join("a/b");
        create_private_dir(&store).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&store), 0o700);
        drop(create_private_file_new(&store.join("key")).unwrap());
        assert_eq!(mode(&store.join("key")), 0o600);
        let record = store.join("record.json");
        write_private_json_atomic(&record, &json!({"v": 1}), || Ok(())).unwrap();
        assert_eq!(mode(&record), 0o600);

        // A group-readable directory is refused and left as it was.
        let broad = s.root().join("broad");
        std::fs::create_dir(&broad).unwrap();
        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o750)).unwrap();
        let why = refusal(create_private_dir(&broad));
        assert!(why.contains("accessible to other users"), "{why}");
        assert_eq!(mode(&broad), 0o750);

        // A world-readable record is refused, never read or adopted.
        let open = store.join("open.json");
        std::fs::write(&open, b"{}").unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
        let why = refusal(read_private_json::<Value>(&open));
        assert!(why.contains("accessible to other users"), "{why}");
        assert_eq!(mode(&open), 0o644);

        // Symlinks are refused as directories, records and locks.
        let target = s.root().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir_link = s.root().join("dir-link");
        std::os::unix::fs::symlink(&target, &dir_link).unwrap();
        let why = refusal(create_private_dir(&dir_link));
        assert!(why.contains("symlinks are refused"), "{why}");
        let file_link = store.join("link.json");
        std::os::unix::fs::symlink(&record, &file_link).unwrap();
        let why = refusal(read_private_json::<Value>(&file_link));
        assert!(why.contains("symlinks are refused"), "{why}");
        let why = refusal(open_private_lock(&file_link, true));
        assert!(why.contains("symlinks are refused"), "{why}");
        // Cache-style reads treat a link as a miss.
        assert_eq!(
            read_private_json_or_miss::<Value>(&file_link, 1024).unwrap(),
            None
        );
        assert_eq!(
            std::fs::symlink_metadata(&record).unwrap().nlink(),
            1,
            "the link target is untouched"
        );
    }
}

/// Native Windows cases. They need a Windows host; tools come from System32
/// (`icacls`, `cmd`) and change ACLs only on directories this test created.
#[cfg(windows)]
mod windows {
    use super::*;
    use awman_test_support::TestSandbox;

    const EVERYONE: &str = "*S-1-1-0";

    fn icacls(s: &TestSandbox, path: &Path, args: &[&str]) -> String {
        let out = s
            .command("icacls")
            .arg(path)
            .args(args)
            .label("icacls")
            .run();
        out.assert_success();
        out.stdout_text()
    }

    fn acl(s: &TestSandbox, path: &Path) -> String {
        icacls(s, path, &[])
    }

    fn junction(s: &TestSandbox, link: &Path, target: &Path) {
        s.command("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .label("mklink")
            .run()
            .assert_success();
    }

    fn is_empty(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().next().is_none()
    }

    /// A data root under a directory whose inheritable ACL grants Everyone:
    /// the existing broad root is refused with its ACL unchanged, a record
    /// inheriting it is refused, while a store created below gets a
    /// protected owner-only DACL that does not inherit the broad entry.
    #[test]
    fn inherited_broad_acl_is_refused_and_new_stores_do_not_inherit_it() {
        let s = crate::support::sandbox(
            "data::private_files::windows::inherited_broad_acl_is_refused_and_new_stores_do_not_inherit_it",
        );
        let shared = s.root().join("shared");
        std::fs::create_dir(&shared).unwrap();
        icacls(&s, &shared, &["/grant", &format!("{EVERYONE}:(OI)(CI)F")]);
        let custom_root = shared.join("awman-data");
        std::fs::create_dir(&custom_root).unwrap();
        let before = acl(&s, &custom_root);

        let why = refusal(create_private_dir(&custom_root));
        assert!(
            why.contains("grants access to another account (S-1-1-0)"),
            "{why}"
        );
        assert_eq!(
            acl(&s, &custom_root),
            before,
            "an existing ACL is never changed"
        );
        let why = refusal(create_private_dir(&shared));
        assert!(why.contains("S-1-1-0"), "{why}");

        let inherited = custom_root.join("record.json");
        std::fs::write(&inherited, b"{}").unwrap();
        let why = refusal(read_private_json::<Value>(&inherited));
        assert!(why.contains("S-1-1-0"), "{why}");
        let why = refusal(open_private_lock(&inherited, false));
        assert!(why.contains("S-1-1-0"), "{why}");

        let store = shared.join("private/store");
        create_private_dir(&store).unwrap();
        for dir in [shared.join("private"), store.clone()] {
            let listing = acl(&s, &dir);
            assert!(!listing.contains("Everyone"), "{listing}");
            assert!(!listing.contains("(I)"), "nothing is inherited: {listing}");
        }
        let record = store.join("secret.json");
        write_private_json_atomic(&record, &json!({"key": "v"}), || Ok(())).unwrap();
        let listing = acl(&s, &record);
        assert!(!listing.contains("Everyone"), "{listing}");
        assert_eq!(read(&record), Some(json!({"key": "v"})));
        drop(create_private_file_new(&store.join("key.pem")).unwrap());
        assert!(!acl(&s, &store.join("key.pem")).contains("Everyone"));
    }

    /// A store or record owned by another account is refused. Setting the
    /// owner to LocalSystem needs an elevated runner (as hosted CI is).
    #[test]
    fn wrong_owner_is_refused() {
        let s = crate::support::sandbox("data::private_files::windows::wrong_owner_is_refused");
        let store = s.root().join("store");
        create_private_dir(&store).unwrap();
        let record = store.join("record.json");
        write_private_json_atomic(&record, &json!({"v": 1}), || Ok(())).unwrap();
        let foreign = s.root().join("foreign");
        create_private_dir(&foreign).unwrap();

        icacls(&s, &foreign, &["/setowner", "*S-1-5-18"]);
        icacls(&s, &record, &["/setowner", "*S-1-5-18"]);
        let why = refusal(create_private_dir(&foreign));
        assert!(why.contains("owned by another account (S-1-5-18)"), "{why}");
        let why = refusal(read_private_json::<Value>(&record));
        assert!(why.contains("owned by another account"), "{why}");
    }

    /// Junctions are refused as the store, as an ancestor of a store, record
    /// or lock, and nothing is created through them.
    #[test]
    fn junction_ancestors_and_leaves_are_refused_without_writing_through() {
        let s = crate::support::sandbox(
            "data::private_files::windows::junction_ancestors_and_leaves_are_refused_without_writing_through",
        );
        let target = s.root().join("target");
        create_private_dir(&target).unwrap();
        let link = s.root().join("link");
        junction(&s, &link, &target);

        let why = refusal(create_private_dir(&link));
        assert!(why.contains("reparse point"), "{why}");
        let why = refusal(create_private_dir(&link.join("store")));
        assert!(why.contains("reparse point"), "{why}");
        let why = refusal(create_private_dir_new(&link.join("fresh")));
        assert!(why.contains("reparse point"), "{why}");
        let why = refusal(create_private_file_new(&link.join("key.pem")));
        assert!(why.contains("reparse point"), "{why}");
        let why = refusal(open_private_lock(&link.join("store.lock"), true));
        assert!(why.contains("reparse point"), "{why}");
        let why = refusal(write_private_json_atomic(
            &link.join("record.json"),
            &json!({"secret": "x"}),
            || Ok(()),
        ));
        assert!(why.contains("reparse point"), "{why}");
        let why = refusal(read_private_json::<Value>(&link.join("record.json")));
        assert!(why.contains("reparse point"), "{why}");
        assert!(
            is_empty(&target),
            "nothing was written through the junction"
        );

        // A junction planted in place of a record is not followed by readers
        // (private or cache-style) and nothing is read through it.
        let store = s.root().join("store");
        create_private_dir(&store).unwrap();
        let planted = store.join("record.json");
        junction(&s, &planted, &target);
        let why = refusal(read_private_json::<Value>(&planted));
        assert!(
            why.contains("links are refused") || why.contains("regular file"),
            "{why}"
        );
        assert_eq!(
            read_private_json_or_miss::<Value>(&planted, 1024).unwrap(),
            None
        );
    }

    /// Hard links and replacement on NTFS: an extra link is refused, a
    /// replacement swaps the entry and leaves the other link's bytes alone,
    /// and a concurrent reader always sees a complete record.
    #[test]
    fn replacement_under_concurrent_readers_is_complete() {
        let s = crate::support::sandbox(
            "data::private_files::windows::replacement_under_concurrent_readers_is_complete",
        );
        let store = s.root().join("store");
        create_private_dir(&store).unwrap();
        let record = store.join("record.json");
        write_private_json_atomic(&record, &json!({"v": 0}), || Ok(())).unwrap();
        let reader = {
            let record = record.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    let value = read(&record).expect("record never disappears");
                    assert!(value["v"].is_u64(), "complete record: {value}");
                }
            })
        };
        for v in 1..=50u64 {
            write_private_json_atomic(&record, &json!({"v": v}), || Ok(())).unwrap();
        }
        reader.join().unwrap();
        assert_eq!(read(&record), Some(json!({"v": 50})));
    }
}
