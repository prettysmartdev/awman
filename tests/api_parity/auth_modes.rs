//! Network-listener authentication: the verdict a request's `Authorization`
//! header gets from the daemon's named key store (WI 0140).
//!
//! The router maps `Accepted` to "continue" and both `Rejected` and `NoKeys`
//! to 401; these tests pin the store side of that contract.

use awman::data::fs::daemon_paths::DaemonPaths;
use awman::engine::auth::{ApiKeyStore, KeyVerdict};

fn store(dir: &std::path::Path) -> std::sync::Arc<ApiKeyStore> {
    ApiKeyStore::open(DaemonPaths::new(dir.join("daemon")).api_keys_file()).unwrap()
}

#[test]
fn header_shapes_that_carry_a_valid_key_are_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = store(tmp.path());
    let (record, key) = keys.create("client").unwrap();
    let accepted = KeyVerdict::Accepted { id: record.id };
    for header in [
        format!("Bearer {}", key.as_str()),
        format!("bearer {}", key.as_str()),
        format!("BEARER {}", key.as_str()),
        key.as_str().to_string(),
    ] {
        assert_eq!(keys.verify(Some(&header)), accepted, "{header:?}");
    }
}

#[test]
fn missing_malformed_and_wrong_headers_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = store(tmp.path());
    let (_, key) = keys.create("client").unwrap();
    let truncated = format!("Bearer {}", &key.as_str()[..63]);
    for header in [
        None,
        Some(""),
        Some("Bearer"),
        Some("Basic Zm9vOmJhcg=="),
        Some(truncated.as_str()),
    ] {
        assert_eq!(keys.verify(header), KeyVerdict::Rejected, "{header:?}");
    }
}

#[test]
fn an_empty_store_is_distinguishable_but_still_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = store(tmp.path());
    assert_eq!(keys.verify(None), KeyVerdict::NoKeys);
    assert_eq!(keys.verify(Some("Bearer x")), KeyVerdict::NoKeys);
    assert_eq!(keys.count(), 0);
}

#[test]
fn a_deleted_key_is_refused_on_the_next_request() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = store(tmp.path());
    let (record, key) = keys.create("client").unwrap();
    let header = format!("Bearer {}", key.as_str());
    assert!(matches!(
        keys.verify(Some(&header)),
        KeyVerdict::Accepted { .. }
    ));
    keys.delete(&record.id).unwrap();
    assert_eq!(keys.verify(Some(&header)), KeyVerdict::NoKeys);
}
