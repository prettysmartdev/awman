//! Socket budgeting, private roots and stale/hostile lock files.
use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let root = temp.path().canonicalize().unwrap();
    (temp, root)
}

fn is_too_long(result: Result<(), EngineError>) -> bool {
    matches!(
        result,
        Err(EngineError::SocketPathTooLong { limit: 103, .. })
    )
}

#[test]
fn the_socket_limit_is_the_macos_bound_minus_the_terminator() {
    assert_eq!(SOCKET_PATH_LIMIT, 104 - 1);
}

#[test]
fn the_budget_is_measured_in_bytes_at_the_exact_boundary() {
    // "/run/agent/<32 hex>.control.sock" is 56 bytes, leaving 47 for the root.
    assert!(check_socket_budget(Path::new(&format!("/{}", "a".repeat(46)))).is_ok());
    assert!(is_too_long(check_socket_budget(Path::new(&format!(
        "/{}",
        "a".repeat(47)
    )))));
    // 2-byte characters: 23 fit (47 bytes), 24 do not, although 24 < 47 characters.
    assert!(check_socket_budget(Path::new(&format!("/{}", "é".repeat(23)))).is_ok());
    assert!(is_too_long(check_socket_budget(Path::new(&format!(
        "/{}",
        "é".repeat(24)
    )))));
    // 4-byte characters and combining marks count by encoded bytes too.
    assert!(is_too_long(check_socket_budget(Path::new(&format!(
        "/{}",
        "🦀".repeat(12)
    )))));
    assert!(is_too_long(check_socket_budget(Path::new(&format!(
        "/{}",
        "e\u{301}".repeat(16)
    )))));
}

#[test]
fn an_overlong_root_fails_before_anything_is_created() {
    let (_temp, base) = scratch();
    for name in ["a".repeat(80), "é".repeat(40), "🦀".repeat(20)] {
        let root = base.join(name).join("state");
        assert!(is_too_long(BuiltinPaths::resolve(&root).map(|_| ())));
        assert!(
            !root.parent().unwrap().exists(),
            "a refused root must leave nothing behind"
        );
    }
}

#[test]
fn a_short_state_root_used_by_the_test_harness_fits() {
    // tools/isolated-test.sh points AWMAN_BUILTIN_STATE_DIR at /tmp/awman-b.XXXXXX.
    assert!(check_socket_budget(Path::new("/tmp/awman-b.AbC123")).is_ok());
    // The throwaway HOME it would otherwise derive does not.
    assert!(is_too_long(check_socket_budget(Path::new(
        "/var/tmp/test-fixtures/test-run.AbC123/home/.awman/builtin"
    ))));
}

#[test]
fn relative_and_parent_relative_roots_are_refused() {
    for root in ["state", "./state", "/tmp/../tmp/awman-x/state"] {
        let result = BuiltinPaths::resolve(Path::new(root));
        assert!(
            matches!(result, Err(EngineError::Config(_))),
            "{root}: {:?}",
            result.map(|_| ())
        );
    }
}

#[test]
fn a_stale_lock_file_from_a_crashed_process_is_reusable() {
    let (_temp, root) = scratch();
    let paths = BuiltinPaths::resolve(&root.join("state")).unwrap();
    // A dead owner leaves the file (contents and all) but the OS drops its flock.
    let stale = paths.home.join("awman.lock");
    std::fs::write(&stale, b"pid 999999 crashed").unwrap();
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o600)).unwrap();
    let guard = paths.lock().expect("a lock nobody holds is acquirable");
    drop(guard);
    assert!(paths.lock().is_ok());
}

#[test]
fn hostile_lock_files_are_refused() {
    let (_temp, root) = scratch();
    let paths = BuiltinPaths::resolve(&root.join("state")).unwrap();
    let lock = paths.home.join("awman.lock");

    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        matches!(paths.lock(), Err(EngineError::Config(m)) if m.contains("private regular file")),
        "group/world-readable lock"
    );

    std::fs::remove_file(&lock).unwrap();
    let target = root.join("elsewhere");
    std::fs::write(&target, b"").unwrap();
    symlink(&target, &lock).unwrap();
    assert!(paths.lock().is_err(), "a symlinked lock is never followed");

    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&lock, paths.home.join("second-name")).unwrap();
    assert!(
        matches!(paths.lock(), Err(EngineError::Config(_))),
        "a multiply-linked lock file is refused"
    );
}

#[test]
fn concurrent_lockers_serialise_and_the_loser_reports_busy() {
    let (_temp, root) = scratch();
    let paths = BuiltinPaths::resolve(&root.join("state")).unwrap();
    let held = paths.lock().unwrap();
    let contender = paths.clone();
    let outcome = std::thread::spawn(move || {
        contender
            .lock_named("awman.lock", Duration::from_millis(80))
            .map(|_| ())
    })
    .join()
    .unwrap();
    assert!(matches!(
        outcome,
        Err(EngineError::BuiltinRuntimeUnavailable { reason }) if reason.contains("busy")
    ));
    drop(held);
    assert!(paths.lock().is_ok());
}

#[test]
fn every_state_subdirectory_is_private() {
    let (_temp, root) = scratch();
    let paths = BuiltinPaths::resolve(&root.join("state")).unwrap();
    for sub in [
        "",
        "run",
        "cache",
        "logs",
        "sandboxes",
        "volumes",
        "snapshots",
        "secrets",
        "attach",
        "db",
    ] {
        let mode = std::fs::metadata(paths.home.join(sub))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{sub:?} must not be group/world accessible"
        );
    }
}

#[test]
fn hostile_ids_cannot_escape_the_attach_directory() {
    let paths = BuiltinPaths {
        home: "/private".into(),
    };
    for id in ["../../etc/passwd", "a/b", "\0", "é🦀", &"x".repeat(4096)] {
        let path = paths.attach(id, "owner/../../x");
        assert_eq!(
            path.parent().unwrap(),
            Path::new("/private/attach"),
            "{id:?}"
        );
        assert!(path.as_os_str().len() < SOCKET_PATH_LIMIT + "/private".len());
    }
}
