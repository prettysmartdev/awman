//! CLI contracts. Foundation registers this target; core decision tests live
//! beside their modules so they run before the integration target barrier.
use awman_test_support::{TestCase, TestSandbox};
use std::path::Path;
fn sandbox(id: &'static str) -> TestSandbox {
    let mut sandbox = TestSandbox::for_test(TestCase::component(id));
    sandbox.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    sandbox
}
#[test]
fn frozen_dispatch_exposes_gate_help() {
    let sandbox = sandbox("frozen_dispatch_exposes_gate_help");
    let out = sandbox
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["--help"])
        .run();
    assert!(out.exit.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("gates check|render|plan"));
    assert!(text.contains("release <subcommand>"));
}
#[test]
fn missing_required_evidence_is_red() {
    let sandbox = sandbox("missing_required_evidence_is_red");
    let out = sandbox
        .command(awman_test_support::cargo_bin!("xtask"))
        .args([
            "evidence",
            "emit",
            "--cell",
            "missing@build",
            "--gate",
            "pr",
            "--junit",
            "absent.xml",
            "--expected",
            "absent.json",
            "--out",
            "absent-evidence",
        ])
        .run();
    assert!(!out.exit.success());
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("run metadata"));
}
