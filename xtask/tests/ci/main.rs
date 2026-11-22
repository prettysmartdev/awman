//! xtask `ci` integration tests (tier component).
//!
//! Owner: workflow step `ci-workflows`. Pre-created empty by the foundation step so
//! the target is registered; an empty target satisfies no suite.

use awman_test_support::{TestCase, TestSandbox};
use std::path::Path;

fn sandbox(id: &'static str) -> TestSandbox {
    let mut sandbox = TestSandbox::for_test(TestCase::component(id));
    sandbox.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    sandbox
}

/// Run `xtask ci` in the sandbox.
///
/// The sandbox clears the child environment and runs it from a temporary
/// directory outside the checkout, so xtask would otherwise fall back to its
/// compile-time manifest directory, which is stale when one `target/` is
/// shared by checkouts at different paths (a container and the host
/// worktree). Forward the `CARGO_MANIFEST_DIR` cargo sets for this test run so
/// the child resolves the checkout under test.
fn xtask(sandbox: &TestSandbox, args: &[&str]) -> awman_test_support::process::Output {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("cargo sets CARGO_MANIFEST_DIR for test processes");
    sandbox
        .command(awman_test_support::cargo_bin!("xtask"))
        .env("CARGO_MANIFEST_DIR", manifest_dir)
        .arg("ci")
        .args(args)
        .run()
}

#[test]
fn unknown_helper_is_refused_with_usage() {
    let s = sandbox("unknown_helper_is_refused_with_usage");
    let out = xtask(&s, &["no-such-helper"]);
    assert!(!out.exit.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown helper"));
}

#[test]
fn a_tampered_archive_fails_digest_verification() {
    let s = sandbox("a_tampered_archive_fails_digest_verification");
    let dir = s.work();
    let archive = dir.join("archive.tar.zst");
    std::fs::write(&archive, b"archive bytes").unwrap();
    let file = dir.join("digests.json");
    let named = format!("archive={}", archive.display());
    let rec = xtask(
        &s,
        &["digest", "record", "--out", &file.to_string_lossy(), &named],
    );
    assert!(
        rec.exit.success(),
        "{}",
        String::from_utf8_lossy(&rec.stderr)
    );
    let line = String::from_utf8(rec.stdout.clone()).unwrap();
    let sha = line
        .trim()
        .strip_prefix("archive_sha256=")
        .unwrap()
        .to_owned();
    let expect = format!("archive={sha}");
    let f = file.to_string_lossy();
    let args = ["digest", "verify", "--file", &f, "--expect", &expect];
    let alone = xtask(&s, &["digest", "verify", "--file", &f]);
    assert!(!alone.exit.success(), "a record cannot vouch for itself");
    let ok = xtask(&s, &args);
    assert!(ok.exit.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    std::fs::write(&archive, b"archive bytez").unwrap();
    let bad = xtask(&s, &args);
    assert!(!bad.exit.success());
}

#[test]
fn a_wrong_independent_digest_fails_even_when_the_record_matches() {
    let s = sandbox("a_wrong_independent_digest_fails_even_when_the_record_matches");
    let dir = s.work();
    let archive = dir.join("archive.tar.zst");
    std::fs::write(&archive, b"archive bytes").unwrap();
    let file = dir.join("digests.json");
    let named = format!("archive={}", archive.display());
    let rec = xtask(
        &s,
        &["digest", "record", "--out", &file.to_string_lossy(), &named],
    );
    assert!(rec.exit.success());
    let bad = xtask(
        &s,
        &[
            "digest",
            "verify",
            "--file",
            &file.to_string_lossy(),
            "--expect",
            &format!("archive={}", "0".repeat(64)),
        ],
    );
    assert!(!bad.exit.success());
}

#[test]
fn cache_keys_are_stable_and_differ_by_build() {
    let s = sandbox("cache_keys_are_stable_and_differ_by_build");
    let a = xtask(&s, &["cache-key", "--build", "linux-x64-default"]);
    let b = xtask(&s, &["cache-key", "--build", "linux-x64-default"]);
    let c = xtask(&s, &["cache-key", "--build", "linux-arm64-builtin"]);
    assert!(a.exit.success() && b.exit.success() && c.exit.success());
    assert_eq!(a.stdout, b.stdout);
    assert_ne!(a.stdout, c.stdout);
    assert!(!String::from_utf8_lossy(&a.stdout).trim().is_empty());
}

#[test]
fn actionlint_config_declares_every_self_hosted_label_once() {
    let s = sandbox("actionlint_config_declares_every_self_hosted_label_once");
    let out_file = s.work().join("actionlint.yaml");
    let out = xtask(
        &s,
        &["actionlint-config", "--out", &out_file.to_string_lossy()],
    );
    assert!(out.exit.success());
    let text = std::fs::read_to_string(out_file).unwrap();
    assert!(text.starts_with("self-hosted-runner:\n  labels:\n"));
    let labels: Vec<&str> = text.lines().skip(2).collect();
    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(labels.len(), unique.len());
    assert!(!labels.is_empty());
}

#[test]
fn called_full_gate_and_candidate_main_have_disjoint_artifact_names() {
    use std::collections::BTreeSet;
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    let parsed: serde_yaml::Value = serde_yaml::from_str(workflow).unwrap();
    let names = |gate: &str| -> BTreeSet<String> {
        parsed["jobs"]
            .as_mapping()
            .unwrap()
            .values()
            .flat_map(|job| job["steps"].as_sequence().into_iter().flatten())
            .filter(|step| {
                step["uses"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("actions/upload-artifact@"))
            })
            .map(|step| {
                let mut name = step["with"]["name"].as_str().unwrap().to_owned();
                for expression in [
                    "${{ steps.gate.outputs.gate }}",
                    "${{ needs.plan.outputs.gate }}",
                    "${{ env.GATE }}",
                    "${{ env.GATE_NAME }}",
                ] {
                    name = name.replace(expression, gate);
                }
                name.replace("${{ inputs.mode }}", "gate")
            })
            .collect()
    };
    let full = names("nightly");
    let candidate = names("main");
    assert!(!full.is_empty());
    assert!(
        full.is_disjoint(&candidate),
        "artifact collision: {:?}",
        full.intersection(&candidate).collect::<Vec<_>>()
    );
}

#[test]
fn lint_job_executes_each_lint_once_while_legacy_shim_waits_for_protection_cutover() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../../.github/workflows/ci.yml")).unwrap();
    let scripts: Vec<_> = workflow["jobs"]["lint"]["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|step| step["run"].as_str())
        .collect();
    // Manifest suites own these commands. Standalone copies would execute twice.
    assert!(scripts
        .iter()
        .all(|run| !run.contains("cargo xtask ci lint-shell")
            && !run.contains("cargo xtask ci lint-notice")
            && !run.contains("cargo xtask lint workflows")));
    assert_eq!(
        scripts
            .iter()
            .filter(|run| run.contains("cargo xtask test run --suite"))
            .count(),
        1
    );
    let manifest = include_str!("../../../ci/gates.toml");
    assert!(manifest.contains("command = [\"make\", \"architecture-rules\"]"));
}
