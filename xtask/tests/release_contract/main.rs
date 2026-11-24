//! Release entry point contracts with private state and fake GitHub transports.
#![cfg(unix)]
mod runner;
mod workflows;
use awman_test_support::{fakes::FakeCli, TestCase, TestSandbox};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}
fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn release_waits_for_the_exact_squash_commit_then_uses_local_promotion() {
    let make = fs::read_to_string(root().join("Makefile")).unwrap();
    assert!(make.contains("release:\n\t@bash tools/prerelease/release.sh"));
    let release = fs::read_to_string(root().join("tools/prerelease/release.sh")).unwrap();
    assert!(release.contains("Waiting for the release PR to be squash merged"));
    assert!(release.contains("merge_commit=$(gh pr view"));
    assert!(release.contains("git rev-list --parents -n 1 \"$merge_commit\""));
    assert!(release.contains("origin/main^{tree}"));
    assert!(release.contains("tools/prerelease/publish.sh"));
    assert!(!release.contains("/reviews?"));
    assert!(!release.contains("group-id.sh"));
    for prerelease in ["apple.sh", "linux.sh"] {
        let text = fs::read_to_string(root().join("tools/prerelease").join(prerelease)).unwrap();
        assert!(!text.contains("check-policy.sh"));
        assert!(!text.contains("AWMAN_RELEASE_APP"));
        assert!(!text.contains("group-id.sh"));
    }
    let publish = fs::read_to_string(root().join("tools/prerelease/publish.sh")).unwrap();
    assert!(publish.contains("AWMAN_RELEASE_RUNNER_GROUP_ID=$(bash"));
    let candidate = fs::read_to_string(root().join(".github/workflows/candidate.yml")).unwrap();
    assert!(candidate.contains("default: false"));
    assert!(candidate.contains("inputs.dry_run && inputs.promote"));
}

#[test]
fn remote_linux_resolves_cargo_without_a_login_shell() {
    use awman_test_support::Exit;
    use std::os::unix::fs::PermissionsExt;

    let mut sandbox = TestSandbox::for_test(TestCase::component("remote-linux-cargo-path"));
    let fake = Path::new(awman_test_support::cargo_bin!("xtask-fake-cli"));
    let uname = sandbox.install_fake(fake, "uname");
    uname.on(&["-s"]).stdout("Linux\n");
    uname.on(&["-m"]).stdout("x86_64\n");
    let cargo_home = sandbox.root().join("rustup");
    sandbox.env_mut().set("CARGO_HOME", &cargo_home);
    let script = root().join("tools/prerelease/remote-linux.sh");
    let run = || {
        sandbox
            .command("bash")
            .arg(&script)
            .args([
                "release/v0.14.0",
                "0123456789abcdef0123456789abcdef01234567",
            ])
            .run()
    };
    let missing = run();
    assert_eq!(missing.exit, Exit::Code(1));
    assert!(String::from_utf8_lossy(&missing.stderr)
        .contains("Cargo is unavailable to noninteractive SSH"));

    let cargo = cargo_home.join("bin/cargo");
    write(&cargo, "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let found = run();
    // The copied script would live under /tmp on the Linux host. Reaching the
    // work-directory check proves Cargo was resolved before any token is read.
    assert_eq!(found.exit, Exit::Code(2));
    assert!(!String::from_utf8_lossy(&found.stderr).contains("Cargo is unavailable"));
}

struct Publisher {
    sandbox: TestSandbox,
    state: PathBuf,
}
impl Publisher {
    fn new(id: &'static str) -> Self {
        let mut sandbox = TestSandbox::for_test(TestCase::component(id));
        let state = sandbox.root().join("publisher");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("state"), "none").unwrap();
        sandbox.env_mut().set("FAKE_STATE", &state);
        sandbox
            .env_mut()
            .set("FAKE_COMMIT", "0123456789abcdef0123456789abcdef01234567");
        sandbox.env_mut().set("RELEASE_RUNNER", "none");
        for name in ["git", "gh", "cargo"] {
            sandbox.install_exe(
                &root().join("xtask/tests/release_contract/publisher-fakes.sh"),
                name,
            );
        }
        Self { sandbox, state }
    }
    fn run(&self) -> awman_test_support::Output {
        self.sandbox
            .command("bash")
            .arg(root().join("tools/prerelease/publish.sh"))
            .args([
                "v1.2.3",
                "0123456789abcdef0123456789abcdef01234567",
                "example/awman",
            ])
            .run()
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.state.join("calls")).unwrap_or_default()
    }
}

#[test]
fn validated_candidate_is_tagged_and_published_by_make_release_path() {
    let p = Publisher::new("release-publish");
    p.run().assert_success();
    let calls = p.calls();
    let validation = calls
        .find("cargo xtask release promote verify-inputs")
        .unwrap();
    let tag = calls.find("cargo xtask release promote tag").unwrap();
    let publish = calls.find("cargo xtask release promote publish").unwrap();
    let verify = calls.find("gh workflow run promote.yml").unwrap();
    assert!(validation < tag && tag < publish && publish < verify);
    assert_eq!(
        fs::read_to_string(p.state.join("state")).unwrap().trim(),
        "verified"
    );
}

#[test]
fn failed_candidate_cannot_be_tagged() {
    let mut p = Publisher::new("release-candidate-fail");
    p.sandbox.env_mut().set("FAIL_CANDIDATE", "1");
    assert!(!p.run().exit.success());
    let calls = p.calls();
    assert!(calls.contains("gh run watch 44"));
    assert!(!calls.contains("release promote tag"));
    assert!(!calls.contains("release promote publish"));
}

#[test]
fn candidate_for_another_commit_is_refused_before_tagging() {
    let p = Publisher::new("release-wrong-commit");
    fs::write(p.state.join("state"), "validated").unwrap();
    fs::write(
        p.state.join("commit"),
        "ffffffffffffffffffffffffffffffffffffffff",
    )
    .unwrap();
    assert!(!p.run().exit.success());
    assert!(!p.calls().contains("release promote tag"));
}

#[test]
fn published_release_retries_verification_without_rebuilding() {
    let p = Publisher::new("release-verify-retry");
    fs::write(p.state.join("state"), "promoted").unwrap();
    fs::write(p.state.join("tag"), "").unwrap();
    p.run().assert_success();
    let calls = p.calls();
    assert!(!calls.contains("gh workflow run candidate.yml"));
    assert!(calls.contains("gh workflow run promote.yml"));
    assert_eq!(
        fs::read_to_string(p.state.join("state")).unwrap().trim(),
        "verified"
    );
}
