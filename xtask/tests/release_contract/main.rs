//! Release entry-point contracts (design 6.1 layer 3): the Makefile targets
//! are one-command wrappers, deleted release paths stay deleted, and the
//! workflows accept exactly what the command dispatches.
#![cfg(unix)]
mod workflows;

use awman_test_support::{TestCase, TestSandbox};
use std::path::{Path, PathBuf};

fn sandbox(id: &'static str) -> TestSandbox {
    let mut s = TestSandbox::for_test(TestCase::component(id));
    s.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    s
}

/// The checkout under test, resolved at run time: a compile-time
/// `CARGO_MANIFEST_DIR` is stale when a `target/` is shared across checkouts.
fn root() -> PathBuf {
    awman_test_support::workspace_root()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The recipe lines of a Makefile target.
fn recipe(make: &str, target: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in make.lines() {
        if line.starts_with(&format!("{target}:")) {
            inside = true;
            continue;
        }
        if inside {
            if line.starts_with('\t') {
                out.push_str(line);
                out.push('\n');
            } else if !line.trim().is_empty() {
                break;
            }
        }
    }
    out
}

#[test]
fn every_release_target_is_a_thin_call_into_cargo_xtask_release() {
    let make = read("Makefile");
    for (target, sub) in [
        ("release", "run"),
        ("release-status", "status"),
        ("release-doctor", "doctor"),
        ("release-abandon", "abandon"),
        ("release-rehearse", "rehearse"),
    ] {
        let body = recipe(&make, target);
        assert_eq!(
            body.lines().count(),
            1,
            "{target} must be one command:\n{body}"
        );
        assert!(
            body.contains(&format!("cargo xtask release {sub}")),
            "{target}: {body}"
        );
    }
    // Every operator variable maps to a flag of the same command.
    for (var, flag) in [
        ("VERSION", "--version"),
        ("LINUX_HOST", "--linux-host"),
        ("YES", "--yes"),
        ("SUPERSEDE", "--supersede"),
        ("TAKEOVER", "--takeover"),
        ("REHEARSE", "--rehearse"),
    ] {
        assert!(
            make.contains(&format!("$({var})")) && make.contains(flag),
            "{var} → {flag}"
        );
    }
    let deleted = [
        ["apple-", "prerelease:"].concat(),
        ["apple-silicon-", "prerelease:"].concat(),
        ["linux-", "prerelease:"].concat(),
        ["RELEASE_", "RUNNER"].concat(),
    ];
    for gone in &deleted {
        assert!(!make.contains(gone), "Makefile still has {gone}");
    }
    assert!(recipe(&make, "validate-native").contains("native-validation run"));
    let sbx = recipe(&make, "validate-sbx");
    assert!(sbx.contains("cargo xtask sbx-validation run"));
    assert!(
        make.contains("export LINUX_HOST"),
        "Make command-line Linux destinations must reach xtask unchanged"
    );
    assert!(!sbx.contains("release run") && !sbx.contains("--tier boundary"));
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                files_under(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}

#[test]
fn deleted_release_scripts_workflows_and_targets_stay_deleted_and_unreferenced() {
    for gone in [
        ["tools", "/prerelease"].concat().as_str(),
        &[".github/workflows/candidate", ".yml"].concat(),
        &[".github/workflows/promote", ".yml"].concat(),
        "xtask/src/release/candidate.rs",
        "xtask/src/release/promote.rs",
        "xtask/src/release/runner.rs",
        "xtask/src/release/state.rs",
        "xtask/src/release/operator.rs",
        "xtask/src/release/validate.rs",
    ] {
        assert!(!root().join(gone).exists(), "{gone} must not exist");
    }
    let mut files = vec![root().join("Makefile")];
    for dir in [
        ".github/workflows",
        "tools",
        "xtask/src/release",
        "xtask/src/fixtures",
        "xtask/tests/release_contract",
        "xtask/tests/pipeline",
    ] {
        files_under(&root().join(dir), &mut files);
    }
    files.extend(
        [
            "xtask/src/fixtures.rs",
            "xtask/src/native_validation.rs",
            "xtask/src/main.rs",
        ]
        .map(|f| root().join(f)),
    );
    let needles = [
        ["tools", "/prerelease"].concat(),
        ["candidate", ".yml"].concat(),
        ["promote", ".yml"].concat(),
        ["apple-", "prerelease"].concat(),
        ["linux-", "prerelease"].concat(),
        ["RELEASE_", "RUNNER"].concat(),
        ["runner ", "group"].concat(),
    ];
    // Prose (`*.md`) belongs to the documentation step; code and workflows here.
    for f in files
        .into_iter()
        .filter(|f| f.extension().is_none_or(|x| x != "md"))
    {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for n in &needles {
            assert!(
                !text.contains(n.as_str()),
                "{} references deleted `{n}`",
                f.display()
            );
        }
    }
}

#[test]
fn the_release_command_help_lists_the_one_command_and_its_recovery_tools() {
    let s = sandbox("release_contract::help");
    let out = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["release", "--help"])
        .run();
    out.assert_success();
    let text = String::from_utf8_lossy(&out.stdout);
    for sub in [
        "run",
        "doctor",
        "status",
        "abandon",
        "rehearse",
        "check-workflows",
    ] {
        assert!(
            text.lines().any(|l| l.trim_start().starts_with(sub)),
            "{sub}"
        );
    }
    assert!(!text.contains("candidate") && !text.contains("promote"));
}

#[test]
fn the_checked_in_plan_fixture_is_a_complete_consumer_contract() {
    let plan: serde_json::Value = serde_json::from_str(&read(
        "xtask/tests/release_contract/fixtures/release-plan.json",
    ))
    .unwrap();
    assert_eq!(plan["schema"], 1);
    assert_eq!(plan["assets"].as_array().unwrap().len(), 4);
    let natives = plan["native"].as_array().unwrap();
    assert_eq!(natives.len(), 4);
    assert!(natives
        .iter()
        .all(|n| n["required"] == true && n.get("waivable").is_none()));
    let arch =
        |id: &str| natives.iter().find(|n| n["id"] == id).unwrap()["fixtures"][0]["arch"].clone();
    assert_eq!(arch("native.linux-x64"), "amd64");
    assert_eq!(arch("native.macos-arm64"), "arm64");
    assert!(plan["assets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["builder"] == "hosted"));
}
