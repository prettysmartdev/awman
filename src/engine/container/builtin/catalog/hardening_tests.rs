//! Version-marker hardening for the shared catalog.
use super::*;
use std::os::unix::fs::symlink;

fn paths() -> (tempfile::TempDir, BuiltinPaths) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let state = temp.path().canonicalize().unwrap().join("state");
    let paths = BuiltinPaths::resolve(&state).unwrap();
    (temp, paths)
}

#[test]
fn a_symlinked_version_marker_is_never_followed() {
    let (temp, paths) = paths();
    let outside = temp.path().join("outside");
    std::fs::write(&outside, PROTOCOL).unwrap();
    symlink(&outside, paths.home.join("awman-runtime-version")).unwrap();
    assert!(check(&paths).is_err());
}

#[test]
fn concurrent_first_starts_agree_on_one_marker() {
    let (_temp, paths) = paths();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6).map(|_| scope.spawn(|| check(&paths))).collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });
    assert_eq!(
        std::fs::read_to_string(paths.home.join("awman-runtime-version")).unwrap(),
        PROTOCOL
    );
}

#[test]
fn an_old_protocol_marker_names_both_versions_and_changes_nothing() {
    let (_temp, paths) = paths();
    let marker = paths.home.join("awman-runtime-version");
    std::fs::write(&marker, "0.6.9/17/awman-0").unwrap();
    match check(&paths) {
        Err(EngineError::WorkerProtocolMismatch { expected, found }) => {
            assert_eq!(expected, PROTOCOL);
            assert_eq!(found, "0.6.9/17/awman-0");
        }
        other => panic!(
            "expected WorkerProtocolMismatch, got {:?}",
            other.map(|_| ())
        ),
    }
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "0.6.9/17/awman-0"
    );
}
