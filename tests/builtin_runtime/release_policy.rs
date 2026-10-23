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

/// Both hardware entry points build the previous awman themselves, so the
/// old-to-new upgrade test never needs one built by hand.
#[test]
fn hardware_entry_points_build_the_previous_builtin_awman() {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let makefile = std::fs::read_to_string(root.join("Makefile")).unwrap();
    let recipe: String = makefile
        .split("\ntest-builtin:\n")
        .nth(1)
        .expect("Makefile has a test-builtin target")
        .lines()
        .take_while(|line| line.starts_with('\t'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        recipe.contains("tools/build-previous-builtin-awman.sh --if-hypervisor")
            && recipe.contains("export AWMAN_TEST_BUILTIN_PREVIOUS="),
        "make test-builtin must build the previous awman: {recipe}"
    );
    let native = std::fs::read_to_string(root.join("tools/native-builtin-ci.sh")).unwrap();
    assert!(
        native.contains("$(bash tools/build-previous-builtin-awman.sh)")
            && native.contains("export AWMAN_TEST_BUILTIN_PREVIOUS="),
        "tools/native-builtin-ci.sh must build the previous awman"
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
