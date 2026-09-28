//! The real binary's handling of the builtin state directory: the socket path
//! budget (macOS 104-byte `sockaddr_un` bound, measured in bytes), symlinks,
//! and refusal side effects. Needs the feature binary; SKIP otherwise.

use std::path::Path;

use crate::binary::{awman, has_builtin_runtime, Scratch};

fn clean_with_state(scratch: &Scratch, state: &Path) -> std::process::Output {
    let repo = scratch.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    scratch
        .command(&awman(), Some("/usr/bin:/bin"))
        .env("AWMAN_TEST_BUILTIN", "1")
        .env("AWMAN_BUILTIN_STATE_DIR", state)
        .arg("clean")
        .current_dir(&repo)
        .output()
        .unwrap()
}

fn skip() -> bool {
    if has_builtin_runtime() {
        return false;
    }
    eprintln!("SKIP: builtin state-dir test: awman was built without the builtin runtime");
    true
}

#[test]
fn builtin_state_dir_too_long_for_sockets_is_refused_with_the_limit_and_creates_nothing() {
    if skip() {
        return;
    }
    for component in ["a".repeat(90), "é".repeat(45), "🦀".repeat(24)] {
        let scratch = Scratch::new();
        crate::sqlite::config_home(&scratch);
        let base = tempfile::Builder::new()
            .prefix("awb.")
            .tempdir_in("/tmp")
            .unwrap();
        let state = base.path().join(&component).join("s");
        let output = clean_with_state(&scratch, &state);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(
            text.contains("103-byte limit"),
            "the limit must be named: {text}"
        );
        assert!(text.contains("failed to detect agent runtime"), "{text}");
        assert!(
            !base.path().join(&component).exists(),
            "no directory may be created for a refused root"
        );
    }
}

#[test]
fn builtin_state_dir_that_is_a_symlink_or_public_is_refused() {
    if skip() {
        return;
    }
    use std::os::unix::fs::{symlink, PermissionsExt};
    let scratch = Scratch::new();
    crate::sqlite::config_home(&scratch);
    let base = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    let real = base.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = base.path().join("link");
    symlink(&real, &link).unwrap();
    let output = clean_with_state(&scratch, &link);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("symlink"),
        "{:?}",
        output
    );

    let open = base.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = clean_with_state(&scratch, &open);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("private ownership"),
        "{:?}",
        output
    );
    assert_eq!(
        std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
        0o755,
        "an existing root is checked, never chmodded or adopted"
    );
}
