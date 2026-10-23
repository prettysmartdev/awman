//! The release guard for builtin worker-protocol changes
//! (`tools/release-upgrade-policy.sh`) and the previous-awman revision picker
//! (`tools/build-previous-builtin-awman.sh`), run against scratch git
//! repositories.

use std::path::Path;
use std::process::Command;

#[test]
fn real_git_release_upgrade_policy_fails_closed_and_honours_its_exceptions() {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let output = Command::new("bash")
        .arg(root.join("tools/test-release-upgrade-policy.sh"))
        .output()
        .expect("run tools/test-release-upgrade-policy.sh");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `tools/build-previous-builtin-awman.sh` picks the older awman that the
/// old-to-new hardware test builds from git history.
#[test]
fn real_git_previous_builtin_awman_is_the_last_commit_on_the_previous_protocol() {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let output = Command::new("bash")
        .arg(root.join("tools/test-build-previous-builtin-awman.sh"))
        .output()
        .expect("run tools/test-build-previous-builtin-awman.sh");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
