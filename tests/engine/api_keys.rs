//! Named API key store for the daemon's network listener (WI 0140).
//!
//! Create returns the plaintext once; only hashes reach disk (mode 0600);
//! list never carries key material; delete revokes at once; ids are `key_`;
//! zero keys rejects every request.

use std::path::{Path, PathBuf};

use awman::data::fs::api_key_store::ApiKeyFile;
use awman::data::fs::daemon_paths::DaemonPaths;
use awman::engine::auth::api_keys::{ApiKeyStore, KeyVerdict};

fn keys_path(dir: &Path) -> PathBuf {
    DaemonPaths::new(dir.join("daemon")).api_keys_file()
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

#[test]
fn keys_file_lives_at_daemon_api_keys_json() {
    let path = DaemonPaths::new("/data/daemon").api_keys_file();
    assert_eq!(path, PathBuf::from("/data/daemon/api_keys.json"));
}

#[test]
fn create_returns_the_key_once_and_stores_only_its_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let path = keys_path(tmp.path());
    let store = ApiKeyStore::open(path.clone()).unwrap();

    let (record, key) = store.create("ci").unwrap();
    assert!(record.id.as_str().starts_with("key_"), "{}", record.id);
    assert_eq!(record.name, "ci");
    assert!(record.last_used_at.is_none());
    assert_eq!(key.as_str().len(), 64);
    assert!(key
        .as_str()
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_ne!(record.hash, key.as_str());
    assert!(
        !format!("{key:?}").contains(key.as_str()),
        "Debug leaks the key"
    );

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains(key.as_str()),
        "plaintext key reached disk"
    );
    assert!(on_disk.contains(&record.hash));
    assert!(on_disk.contains(record.id.as_str()));

    // Nothing in the store can give the plaintext back.
    for listed in store.list() {
        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains(key.as_str()));
    }
}

#[cfg(unix)]
#[test]
fn keys_file_is_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = keys_path(tmp.path());
    let store = ApiKeyStore::open(path.clone()).unwrap();
    store.create("ci").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "api_keys.json mode {mode:o}");
}

#[cfg(unix)]
#[test]
fn an_unsafe_keys_file_refuses_to_open() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = keys_path(tmp.path());
    ApiKeyStore::open(path.clone())
        .unwrap()
        .create("ci")
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ApiKeyStore::open(path).is_err());
}

#[test]
fn list_is_oldest_first_and_carries_no_key_material() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ApiKeyStore::open(keys_path(tmp.path())).unwrap();
    let (a, key_a) = store.create("first").unwrap();
    let (b, key_b) = store.create("second").unwrap();
    let listed = store.list();
    assert_eq!(
        listed.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        vec![a.id.clone(), b.id.clone()]
    );
    let all = serde_json::to_string(&listed).unwrap();
    assert!(!all.contains(key_a.as_str()) && !all.contains(key_b.as_str()));
    assert_eq!(store.get(&a.id).unwrap().name, "first");
    assert_eq!(store.count(), 2);
}

#[test]
fn verify_accepts_current_keys_and_rejects_everything_else() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ApiKeyStore::open(keys_path(tmp.path())).unwrap();
    let (record, key) = store.create("ci").unwrap();

    let accepted = KeyVerdict::Accepted {
        id: record.id.clone(),
    };
    assert_eq!(store.verify(Some(&bearer(key.as_str()))), accepted);
    assert_eq!(
        store.verify(Some(&format!("bearer {}", key.as_str()))),
        accepted
    );
    assert_eq!(store.verify(Some(key.as_str())), accepted);

    assert_eq!(store.verify(None), KeyVerdict::Rejected);
    assert_eq!(store.verify(Some("")), KeyVerdict::Rejected);
    assert_eq!(store.verify(Some("Bearer ")), KeyVerdict::Rejected);
    assert_eq!(
        store.verify(Some(&bearer(&"0".repeat(64)))),
        KeyVerdict::Rejected
    );
    // The stored hash is not itself a credential.
    assert_eq!(
        store.verify(Some(&bearer(&record.hash))),
        KeyVerdict::Rejected
    );
}

#[test]
fn delete_revokes_at_once_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let path = keys_path(tmp.path());
    let store = ApiKeyStore::open(path.clone()).unwrap();
    let (old, old_key) = store.create("old").unwrap();
    let (new, new_key) = store.create("new").unwrap();

    assert!(store.delete(&old.id).unwrap());
    assert_eq!(
        store.verify(Some(&bearer(old_key.as_str()))),
        KeyVerdict::Rejected
    );
    assert_eq!(
        store.verify(Some(&bearer(new_key.as_str()))),
        KeyVerdict::Accepted { id: new.id.clone() }
    );
    assert!(store.get(&old.id).is_none());
    assert!(
        !store.delete(&old.id).unwrap(),
        "second delete finds nothing"
    );

    // A restart sees the revocation.
    let reopened = ApiKeyStore::open(path).unwrap();
    assert_eq!(
        reopened.verify(Some(&bearer(old_key.as_str()))),
        KeyVerdict::Rejected
    );
    assert_eq!(reopened.count(), 1);
}

#[test]
fn zero_keys_rejects_every_request() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ApiKeyStore::open(keys_path(tmp.path())).unwrap();
    assert_eq!(store.verify(None), KeyVerdict::NoKeys);
    assert_eq!(store.verify(Some("Bearer whatever")), KeyVerdict::NoKeys);

    let (record, key) = store.create("only").unwrap();
    assert!(store.delete(&record.id).unwrap());
    assert_eq!(store.count(), 0);
    assert_eq!(
        store.verify(Some(&bearer(key.as_str()))),
        KeyVerdict::NoKeys
    );
}

#[test]
fn rotation_without_downtime() {
    // Add a second key, move the client, delete the first: at every step
    // some key is accepted.
    let tmp = tempfile::tempdir().unwrap();
    let store = ApiKeyStore::open(keys_path(tmp.path())).unwrap();
    let (first, first_key) = store.create("client-2026-10").unwrap();
    let (second, second_key) = store.create("client-2026-11").unwrap();
    assert!(matches!(
        store.verify(Some(&bearer(first_key.as_str()))),
        KeyVerdict::Accepted { .. }
    ));
    assert!(matches!(
        store.verify(Some(&bearer(second_key.as_str()))),
        KeyVerdict::Accepted { .. }
    ));
    store.delete(&first.id).unwrap();
    assert_eq!(
        store.verify(Some(&bearer(second_key.as_str()))),
        KeyVerdict::Accepted { id: second.id }
    );
}

#[test]
fn names_must_be_valid_and_unique() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ApiKeyStore::open(keys_path(tmp.path())).unwrap();
    assert!(store.create("Bad Name").is_err());
    assert!(store.create("").is_err());
    store.create("ci").unwrap();
    assert!(store.create("ci").is_err());
    assert_eq!(store.count(), 1);
}

#[test]
fn last_used_at_updates_on_use_and_reaches_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let path = keys_path(tmp.path());
    let store = ApiKeyStore::open(path.clone()).unwrap();
    let (record, key) = store.create("ci").unwrap();
    let (unused, _) = store.create("unused").unwrap();

    store.verify(Some(&bearer(key.as_str())));
    let used = store.get(&record.id).unwrap().last_used_at;
    assert!(used.is_some());
    assert!(store.get(&unused.id).unwrap().last_used_at.is_none());

    // The first use of a key is flushed immediately.
    let file = ApiKeyFile::load(&path).unwrap();
    let stored = file.keys.iter().find(|k| k.id == record.id).unwrap();
    assert_eq!(stored.last_used_at, used);
}

#[test]
fn keys_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = keys_path(tmp.path());
    let (record, key) = ApiKeyStore::open(path.clone())
        .unwrap()
        .create("ci")
        .unwrap();
    let reopened = ApiKeyStore::open(path).unwrap();
    assert_eq!(reopened.get(&record.id).unwrap().hash, record.hash);
    assert_eq!(
        reopened.verify(Some(&bearer(key.as_str()))),
        KeyVerdict::Accepted { id: record.id }
    );
}
