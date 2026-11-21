#![cfg(unix)]
use super::*;
use awman_test_support::fixtures::RepoBuilder;
use std::os::unix::fs::PermissionsExt;

fn source_patch(replacement: &str) -> String {
    format!("# defect: Replace a checked production boundary\n# expect-red: unit-xtask\ndiff --git a/src/probe.rs b/src/probe.rs\n--- a/src/probe.rs\n+++ b/src/probe.rs\n@@ -1 +1 @@\n-// baseline\n+// {replacement}\n")
}

#[test]
fn source_runner_classifies_detection_survival_compiler_blocked_and_stale_without_touching_checkout(
) {
    let mut s = TestSandbox::for_test(TestCase::component(
        "pipeline::source_defects::classification",
    ));
    s.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    let fake = s.install_fake(
        Path::new(awman_test_support::cargo_bin!("xtask-fake-cli")),
        "rustc",
    );
    fake.on(&["-vV"]).stdout("host: x86_64-unknown-linux-gnu\n");
    let manifest = std::fs::read_to_string(repo_root().join("ci/gates.toml")).unwrap();
    let m: toml::Value = toml::from_str(&manifest).unwrap();
    let version = m["tools"]["nextest"]["version"].as_str().unwrap();
    let script = format!(
        r#"#!/bin/sh
set -eu
case "$*" in
  'nextest --version') echo 'cargo-nextest {version}'; exit 0 ;;
  'nextest list'*)
    if grep -q COMPILE src/probe.rs; then echo 'compiler failed' >&2; exit 101; fi
    echo '{{}}'; exit 0 ;;
  'nextest run'*)
    mkdir -p target/nextest/default
    body=''
    status=0
    if grep -q DETECTED src/probe.rs; then body='<failure message="assertion failed"/>'; status=100; fi
    if grep -q BLOCKED src/probe.rs; then body='<failure message="BLOCKED[fixture]"/>'; status=100; fi
    printf '<testsuite><testcase classname="xtask::bin/xtask" name="probe">%s</testcase></testsuite>' "$body" > target/nextest/default/junit.xml
    exit "$status" ;;
esac
echo 'unexpected cargo invocation' >&2
exit 2
"#
    );
    let cargo = s.work().join("cargo-fixture");
    std::fs::write(&cargo, script).unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.install_exe(&cargo, "cargo");
    let repo = RepoBuilder::new(&s, "seeded")
        .file("Cargo.toml", "[workspace]\nmembers=[]\n")
        .file("Cargo.lock", "version = 4\n")
        .file("rust-toolchain.toml", "[toolchain]\nchannel='1.94.0'\n")
        .file("ci/gates.toml", manifest)
        .file(
            ".config/nextest.toml",
            "[profile.default.junit]\npath='junit.xml'\n",
        )
        .file("src/probe.rs", "// baseline\n")
        .init();
    // Only HEAD enters the baseline. Uncommitted operator edits stay untouched.
    repo.write("src/probe.rs", "// operator's uncommitted edit\n");
    for (name, patch, expected, code) in [
        ("detected", source_patch("DETECTED"), "red, as required", 0),
        ("survived", source_patch("SURVIVED"), "SURVIVED", 1),
        ("compile", source_patch("COMPILE"), "INCONCLUSIVE", 1),
        ("blocked", source_patch("BLOCKED"), "INCONCLUSIVE", 1),
        (
            "stale",
            source_patch("DETECTED").replace("-// baseline", "-// obsolete"),
            "STALE",
            1,
        ),
    ] {
        let patches = s.work().join(format!("patches-{name}/layer1"));
        std::fs::create_dir_all(&patches).unwrap();
        std::fs::write(patches.join("probe.patch"), patch).unwrap();
        let report = s.work().join(format!("{name}.md"));
        let out = s
            .command(awman_test_support::cargo_bin!("xtask"))
            .current_dir(repo.path())
            .env("CARGO_MANIFEST_DIR", repo.path().join("xtask"))
            .args(["ci", "seeded-defects", "source", "--dir"])
            .arg(patches.parent().unwrap())
            .arg("--report")
            .arg(&report)
            .run();
        assert_eq!(out.exit, Exit::Code(code), "{}", out.stderr_text());
        assert!(std::fs::read_to_string(&report).unwrap().contains(expected));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("src/probe.rs")).unwrap(),
            "// operator's uncommitted edit\n"
        );
        assert_eq!(
            repo.git(&["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
    }
}

#[test]
fn empty_header_only_and_unregistered_source_defects_are_nonpassing() {
    let mut s = TestSandbox::for_test(TestCase::component(
        "pipeline::source_defects::invalid_inputs",
    ));
    s.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    let repo = RepoBuilder::new(&s, "inputs")
        .file("Cargo.toml", "[workspace]\nmembers=[]\n")
        .file(
            "ci/gates.toml",
            std::fs::read(repo_root().join("ci/gates.toml")).unwrap(),
        )
        .file("src/probe.rs", "// baseline\n")
        .init();
    let dir = s.work().join("patches/layer1");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, patch, reason) in [
        ("empty", None, "no seeded-defect patches"),
        (
            "header",
            Some("# defect: header only\n# expect-red: unit-xtask\n".into()),
            "empty source defect patch",
        ),
        (
            "unregistered",
            Some(source_patch("DETECTED").replace("unit-xtask", "does-not-exist")),
            "unregistered expect-red suite",
        ),
    ] {
        if let Some(text) = patch {
            std::fs::write(dir.join("probe.patch"), text).unwrap();
        }
        let out = s
            .command(awman_test_support::cargo_bin!("xtask"))
            .current_dir(repo.path())
            .env("CARGO_MANIFEST_DIR", repo.path().join("xtask"))
            .args(["ci", "seeded-defects", "source", "--dir"])
            .arg(dir.parent().unwrap())
            .arg("--report")
            .arg(s.work().join(format!("{name}.md")))
            .run();
        assert!(!out.exit.success());
        assert!(out.stderr_text().contains(reason), "{}", out.stderr_text());
    }
}
