//! Failure evidence (WI 3.8): a failing test leaves a record with the panic,
//! processes and their output, notes and a reproduce command.

use crate::{fake_cli_exe, panic_text};
use awman_test_support::{evidence, TestCase, TestSandbox};
use std::path::PathBuf;

#[test]
fn a_panicking_test_writes_a_failure_record_and_keeps_the_sandbox() {
    let parent = tempfile::tempdir().unwrap();
    let mut root = PathBuf::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut sandbox = TestSandbox::for_test_in(
            TestCase::component("evidence::simulated_failure"),
            Some(parent.path()),
        );
        sandbox.register_exe("fake-cli", fake_cli_exe());
        root = sandbox.root().to_path_buf();
        let fake = sandbox.install_fake(fake_cli_exe(), "tool");
        fake.on(&[]).stdout("tool output\n").exit(4);
        sandbox.redact("hunter2-secret");
        evidence::note("seed", "0xfeed");
        sandbox.note("vm", "owner-123");
        let out = sandbox.command("tool").label("the tool").run();
        assert!(
            out.exit.success(),
            "tool failed with password hunter2-secret"
        );
    }));
    let err = result.unwrap_err();
    assert!(panic_text(&err).contains("tool failed"));
    assert!(root.is_dir(), "a failed sandbox is kept");
    let records = root.join("evidence/records");
    let record_path = std::fs::read_dir(&records)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .expect("a record");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["schema"], 1);
    assert_eq!(record["tier"], "component");
    assert_eq!(record["seed"], "0xfeed");
    assert_eq!(record["vms"][0], "owner-123");
    assert!(record["test"]
        .as_str()
        .unwrap()
        .ends_with(" evidence::simulated_failure"));
    let panic = record["panic"].as_str().unwrap();
    assert!(panic.contains("tool failed"), "{panic}");
    assert!(
        !panic.contains("hunter2-secret"),
        "secrets are redacted: {panic}"
    );
    assert!(record["reproduce"]
        .as_str()
        .unwrap()
        .contains("test(=evidence::simulated_failure)"));
    let tool = record["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["label"] == "the tool")
        .expect("the tool process")
        .clone();
    assert_eq!(tool["exit"], "exit code 4");
    assert!(!record["toolchain"].as_str().unwrap().is_empty());
    let stdout = record_path
        .parent()
        .unwrap()
        .join(tool["stdout"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(stdout).unwrap(), "tool output\n");
}

#[test]
fn a_passing_test_leaves_nothing_behind() {
    let parent = tempfile::tempdir().unwrap();
    let mut sandbox =
        TestSandbox::for_test_in(TestCase::component("evidence::pass"), Some(parent.path()));
    sandbox.register_exe("fake-cli", fake_cli_exe());
    let fake = sandbox.install_fake(fake_cli_exe(), "tool");
    fake.on(&[]).exit(0);
    sandbox.command("tool").run().assert_success();
    let root = sandbox.root().to_path_buf();
    sandbox.finish();
    assert!(!root.exists());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn on_finish_probes_report_leaks() {
    let parent = tempfile::tempdir().unwrap();
    let sandbox =
        TestSandbox::for_test_in(TestCase::component("evidence::probe"), Some(parent.path()));
    sandbox.on_finish(
        "vm list",
        Box::new(|| vec![awman_test_support::Leak::new("vm", "owner-1 still running")]),
    );
    let text = crate::expect_panic(|| sandbox.finish());
    assert!(
        text.contains("vm: owner-1 still running (probe vm list)"),
        "{text}"
    );
}
