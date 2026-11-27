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
    assert!(text.contains("release run"));
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

#[test]
fn release_plan_is_json_and_invalid_evidence_never_authenticates_itself() {
    let s = sandbox("release_plan_is_json_and_invalid_evidence_never_authenticates_itself");
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let out = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["gates", "release-plan", "--root"])
        .arg(&root)
        .run();
    out.assert_success();
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["gate"], "release");
    assert_eq!(plan["assets"].as_array().unwrap().len(), 4);
    let fake_plan = s.work().join("untrusted.json");
    std::fs::write(
        &fake_plan,
        br#"{"schema":1,"authenticated":true,"outcome":"passed"}"#,
    )
    .unwrap();
    let record = s.work().join("verified.json");
    let out = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["gates", "verify-release-evidence", "--root"])
        .arg(&root)
        .arg("--plan")
        .arg(&fake_plan)
        .args(["--repo", "owner/repo", "--run-id", "42", "--out"])
        .arg(&record)
        .run();
    assert!(!out.exit.success());
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(record["schema"], 1);
    assert_eq!(record["outcome"], "failed");
    assert!(record["cells"].as_array().unwrap().is_empty());
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[test]
fn sbx_validation_retains_a_blocked_proof_on_an_unsupported_host() {
    let sandbox = sandbox("sbx_validation_retains_a_blocked_proof_on_an_unsupported_host");
    let out = sandbox.work().join("proofs");
    let result = sandbox
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["sbx-validation", "run", "--out"])
        .arg(&out)
        .run();
    assert_eq!(result.exit, awman_test_support::Exit::Code(2));
    let attempts: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(attempts.len(), 1);
    let proof: serde_json::Value =
        serde_json::from_slice(&std::fs::read(attempts[0].join("proof.json")).unwrap()).unwrap();
    assert_eq!(proof["outcome"], "blocked");
    assert_eq!(proof["steps"], serde_json::json!({}));
    assert!(proof["error"].as_str().unwrap().contains("Apple silicon"));
    let verify = sandbox
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["sbx-validation", "verify"])
        .arg(&attempts[0])
        .run();
    assert!(!verify.exit.success());
    // Fresh attempts cannot accidentally reuse a preceding proof.
    sandbox
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["sbx-validation", "run", "--out"])
        .arg(&out)
        .run();
    assert_eq!(std::fs::read_dir(out).unwrap().count(), 2);
    sandbox.finish();
}
