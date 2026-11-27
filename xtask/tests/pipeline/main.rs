//! xtask `pipeline` integration tests (tier component).
//!
//! The release state machine runs through its production driver against
//! the directory-backed fault world (`cargo xtask release run --world DIR`,
//! design 6.1 layer 1). Each `release run` below is a separate process, so
//! resume after a killed driver is real cross-process recovery. Nothing
//! contacts a service, a host or a network.

use awman_test_support::{Exit, TestCase, TestSandbox};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

mod readiness;
mod source_defects;

const VERSION: &str = "v0.15.0";
const LINUX_HOST: &str = "build@kvm1";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha_file(p: &Path) -> String {
    sha(&std::fs::read(p).unwrap())
}

/// The checkout under test, resolved at run time: a compile-time
/// `CARGO_MANIFEST_DIR` is stale when a `target/` is shared across checkouts.
fn repo_root() -> PathBuf {
    awman_test_support::workspace_root()
}

fn r() -> String {
    "2".repeat(40)
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn passed(&self) -> &Self {
        assert_eq!(
            self.code, 0,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }
    /// Exactly one primary error and one recovery line.
    fn stopped(&self, code: i32, label: &str, stop: &str) -> &Self {
        assert_eq!(
            self.code, code,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        let lines: Vec<&str> = self
            .stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(
            lines.len(),
            2,
            "one error and one recovery expected:\n{}",
            self.stderr
        );
        assert!(
            lines[0].starts_with(&format!("release: {label} [{stop}] ")),
            "{}",
            self.stderr
        );
        assert!(lines[1].starts_with("  recovery: "), "{}", self.stderr);
        self
    }
}

struct W {
    sandbox: TestSandbox,
    world: PathBuf,
}

impl W {
    fn new(id: &'static str) -> W {
        let mut sandbox = TestSandbox::for_test(TestCase::component(id));
        sandbox.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
        let world = sandbox.work().join("world");
        let w = W { sandbox, world };
        w.xtask(&[
            "release",
            "world-seed",
            "--world",
            w.world.to_str().unwrap(),
        ])
        .passed();
        w
    }

    fn xtask(&self, args: &[&str]) -> Run {
        let out = self
            .sandbox
            .command(awman_test_support::cargo_bin!("xtask"))
            .args(args)
            .run();
        Run {
            code: match out.exit {
                Exit::Code(c) => c,
                _ => -1,
            },
            stdout: String::from_utf8_lossy(&out.stdout).into(),
            stderr: String::from_utf8_lossy(&out.stderr).into(),
        }
    }

    fn release(&self, extra: &[&str]) -> Run {
        let mut args = vec![
            "release",
            "run",
            "--world",
            self.world.to_str().unwrap(),
            "--linux-host",
            LINUX_HOST,
        ];
        args.extend_from_slice(extra);
        self.xtask(&args)
    }

    fn store(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.world.join("world.json")).unwrap()).unwrap()
    }

    fn edit(&self, f: impl FnOnce(&mut Value)) {
        let mut v = self.store();
        f(&mut v);
        std::fs::write(
            self.world.join("world.json"),
            serde_json::to_vec(&v).unwrap(),
        )
        .unwrap();
    }

    fn calls(&self, prefix: &str) -> usize {
        self.store()["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c.as_str().unwrap().starts_with(prefix))
            .count()
    }

    fn journal(&self) -> Value {
        serde_json::from_slice(
            &std::fs::read(self.world.join(format!("state/{VERSION}/state.json"))).unwrap(),
        )
        .unwrap()
    }

    fn published(&self) -> Option<Value> {
        self.store()["releases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tag"] == VERSION && r["draft"] == false)
            .cloned()
    }
}

#[test]
fn one_command_publishes_and_verifies_exactly_the_validated_bytes() {
    let w = W::new("pipeline::release::clean");
    let run = w.release(&[]);
    run.passed();
    assert!(run.stdout.contains("published and verified"));
    assert!(run.stdout.contains("collect started"), "{}", run.stdout);
    assert!(run.stdout.contains("collect: ci: run"), "{}", run.stdout);
    assert!(run.stdout.contains("actions/runs/"), "{}", run.stdout);
    assert!(run.stdout.contains("evidence verified"), "{}", run.stdout);
    assert!(
        run.stdout.contains("reconciling") && run.stdout.contains("bytes"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("PUBLICATION CONFIRMATION"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("logs/collect.log") && run.stdout.contains("timings.jsonl"),
        "{}",
        run.stdout
    );
    let s = w.store();
    let tag = &s["tags"][VERSION];
    assert_eq!(tag["commit"], r());
    assert_eq!(tag["annotated"], true);
    let j = w.journal();
    let seal = j["release"]["seal_sha256"].as_str().unwrap();
    assert!(tag["message"]
        .as_str()
        .unwrap()
        .contains(&format!("awman-release-seal: sha256:{seal}")));
    assert_eq!(w.calls("dispatch "), 5, "one CI gate and four asset builds");
    assert_eq!(w.calls("confirm"), 1);
    let release = w.published().expect("published release");
    let assets = release["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 6);
    for (name, b) in j["builds"].as_object().unwrap() {
        let published = assets.iter().find(|a| a["name"] == name.as_str()).unwrap();
        assert_eq!(published["digest"], b["sha256"], "{name}");
    }
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(
            w.world
                .join(format!("state/{VERSION}/evidence/receipt.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["outcome"], "verified");
    assert_eq!(receipt["seal_sha256"], seal);
    for phase in [
        "preflight",
        "pin",
        "push",
        "dispatch",
        "collect",
        "native",
        "seal",
        "confirm",
        "land",
        "tag",
        "publish",
        "verify",
        "cleanup",
    ] {
        assert_eq!(j["phases"][phase]["status"], "done", "{phase}");
    }
    // A second invocation re-probes and changes nothing.
    w.release(&[]).passed();
    assert_eq!(w.calls("dispatch "), 5);
    assert_eq!(w.calls("tag "), 1);
    assert_eq!(w.calls("publish "), 1);
}

#[test]
fn a_killed_driver_resumes_in_a_new_process_without_dispatching_again() {
    let w = W::new("pipeline::release::killed");
    w.edit(|s| s["faults"] = json!([{"op": "run", "nth": 4, "effect": "crash"}]));
    w.release(&[]).stopped(137, "KILLED", "killed");
    assert_eq!(w.calls("dispatch "), 5);
    let resumed = w.release(&[]);
    resumed.passed();
    assert!(resumed.stdout.contains("resumed"), "{}", resumed.stdout);
    assert_eq!(w.calls("dispatch "), 5, "resume attached to the same runs");
    assert!(w.published().is_some());
}

#[test]
fn upload_progress_survives_a_lost_response_and_resume() {
    let w = W::new("pipeline::release::upload_progress_resume");
    w.edit(|s| {
        s["faults"] = json!([{"op": "upload_asset", "nth": 2, "effect": "error-after-commit"}])
    });
    let stopped = w.release(&[]);
    stopped.stopped(2, "BLOCKED", "network");
    assert!(
        stopped.stdout.contains("seal blocked"),
        "{}",
        stopped.stdout
    );
    assert!(
        stopped.stdout.contains("reconciling") && stopped.stdout.contains("bytes"),
        "{}",
        stopped.stdout
    );
    let resumed = w.release(&[]);
    resumed.passed();
    assert!(
        resumed.stdout.contains("seal resumed"),
        "{}",
        resumed.stdout
    );
    assert!(
        resumed.stdout.contains("upload reconciled"),
        "{}",
        resumed.stdout
    );
    assert_eq!(w.calls("dispatch "), 5);
}

#[test]
fn an_unreachable_host_blocks_in_preflight_before_anything_is_pushed_or_dispatched() {
    let w = W::new("pipeline::release::host_unreachable");
    w.edit(|s| s["script"]["unreachable"] = json!([LINUX_HOST]));
    w.release(&[])
        .stopped(2, "BLOCKED", "linux-host-unreachable");
    assert_eq!(w.calls("push "), 0);
    assert_eq!(w.calls("dispatch "), 0);
    assert_eq!(w.calls("claim "), 0);
    // Restoring the host and running the same command continues.
    w.edit(|s| s["script"]["unreachable"] = json!([]));
    w.release(&[]).passed();
}

#[test]
fn no_linux_host_is_a_blocked_prerequisite_never_a_waiver() {
    let w = W::new("pipeline::release::no_linux_host");
    w.xtask(&["release", "run", "--world", w.world.to_str().unwrap()])
        .stopped(2, "BLOCKED", "linux-host-missing");
    assert_eq!(w.calls("dispatch "), 0);
}

#[test]
fn a_failed_assertion_is_never_retried_into_green() {
    let w = W::new("pipeline::release::product_failure");
    w.edit(|s| s["script"]["ci"]["fail"] = json!({"1": "product", "2": "product"}));
    w.release(&[]).stopped(1, "FAILED", "ci-failed");
    w.release(&[]).stopped(1, "FAILED", "ci-failed");
    assert_eq!(w.calls("rerun "), 0);
    assert_eq!(w.calls("dispatch "), 5);
    assert!(w.store()["tags"].as_object().unwrap().is_empty());
}

#[test]
fn status_is_read_only_and_names_the_first_error_and_its_recovery() {
    let w = W::new("pipeline::release::status");
    w.edit(|s| s["script"]["confirm"] = json!(false));
    w.release(&[]).stopped(2, "BLOCKED", "not-confirmed");
    let records = |s: Value| {
        json!([
            s["calls"],
            s["releases"],
            s["tags"],
            s["claims"],
            s["refs"],
            s["runs"].as_array().map(|r| r.len())
        ])
    };
    let before = records(w.store());
    let journal_before =
        std::fs::read(w.world.join(format!("state/{VERSION}/state.json"))).unwrap();
    let status = w.xtask(&[
        "release",
        "status",
        "--version",
        VERSION,
        "--world",
        w.world.to_str().unwrap(),
    ]);
    status.passed();
    assert!(
        status
            .stdout
            .contains("first error in confirm: [not-confirmed]"),
        "{}",
        status.stdout
    );
    assert!(status.stdout.contains("recovery: "));
    assert!(status.stdout.contains("seal       done"));
    assert_eq!(
        records(w.store()),
        before,
        "status changed the service state"
    );
    assert_eq!(
        std::fs::read(w.world.join(format!("state/{VERSION}/state.json"))).unwrap(),
        journal_before
    );
}

#[test]
fn abandon_removes_only_its_own_draft_claim_and_state_and_is_refused_once_tagged() {
    let w = W::new("pipeline::release::abandon");
    w.edit(|s| {
        s["script"]["confirm"] = json!(false);
        s["releases"] = json!([{"id": 7, "tag": "v0.14.0", "name": "v0.14.0", "draft": true, "target": "x", "body": "someone else", "assets": []}]);
    });
    w.release(&[]).stopped(2, "BLOCKED", "not-confirmed");
    w.xtask(&[
        "release",
        "abandon",
        "--version",
        VERSION,
        "--world",
        w.world.to_str().unwrap(),
    ])
    .passed();
    let s = w.store();
    let releases = s["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 1, "only the foreign draft remains");
    assert_eq!(releases[0]["id"], 7);
    assert!(s["claims"].as_object().unwrap().is_empty());
    assert!(!w.world.join(format!("state/{VERSION}")).exists());

    let t = W::new("pipeline::release::abandon_tagged");
    t.release(&[]).passed();
    t.xtask(&[
        "release",
        "abandon",
        "--version",
        VERSION,
        "--world",
        t.world.to_str().unwrap(),
    ])
    .stopped(2, "BLOCKED", "supersede-refused");
    assert!(t.published().is_some());
}

#[test]
fn a_slow_release_is_marked_as_missing_the_hour_and_keeps_its_timing_ledger() {
    let w = W::new("pipeline::release::hour_target_missed");
    w.edit(|s| s["script"]["confirm"] = json!(false));
    // Preparation started two hours before the (virtual) command start.
    let decided = "2026-11-25T10:00:00Z";
    w.release(&["--decided-at", decided])
        .stopped(2, "BLOCKED", "not-confirmed");
    w.edit(|s| s["script"]["confirm"] = json!(true));
    let done = w.release(&[]);
    done.passed();
    assert!(
        done.stdout.contains("performance target MISSED"),
        "{}",
        done.stdout
    );
    let j = w.journal();
    assert_eq!(
        j["decided_at"], decided,
        "the decision time survives resume"
    );
    let inv = j["invocations"].as_array().unwrap();
    assert_eq!(inv.len(), 2, "both invocations stay in the ledger: {inv:?}");
    assert_eq!(inv[0]["outcome"], "not-confirmed");
    assert_eq!(inv[1]["outcome"], "done");
    let timings =
        std::fs::read_to_string(w.world.join(format!("state/{VERSION}/timings.jsonl"))).unwrap();
    assert!(timings.contains("\"event\":\"missed\""), "{timings}");
    assert!(
        timings.contains("\"event\":\"service\""),
        "service stages are not in the ledger"
    );
}

#[test]
fn a_disposable_repository_rehearsal_publishes_only_the_rehearsal_namespace() {
    // Not authorized: the repository is not in the clone's allowlist.
    let w = W::new("pipeline::release::disposable_refused");
    w.release(&["--rehearsal-repo", "example/awman"])
        .stopped(2, "BLOCKED", "rehearsal-refused");
    assert_eq!(w.calls("claim "), 0, "a refused rehearsal wrote state");
    // The rehearsal repository must be the clone's origin.
    let w = W::new("pipeline::release::disposable_wrong_repo");
    w.edit(|s| {
        s["source"]["config"]["awman.release.rehearsalRepos"] = json!("example/other");
    });
    w.release(&["--rehearsal-repo", "example/other"])
        .stopped(2, "BLOCKED", "wrong-repo");
    // Authorized: the complete production path runs, tag/release names are
    // `rehearsal-*`, and no product version tag exists.
    let w = W::new("pipeline::release::disposable");
    w.edit(|s| {
        s["source"]["config"]["awman.release.rehearsalRepos"] = json!("example/awman");
    });
    let out = w.release(&["--rehearsal-repo", "example/awman"]);
    out.passed();
    let s = w.store();
    let tags: Vec<&String> = s["tags"].as_object().unwrap().keys().collect();
    assert_eq!(tags.len(), 1, "{tags:?}");
    assert!(
        tags[0].starts_with(&format!("rehearsal-{VERSION}-")),
        "{tags:?}"
    );
    let rel = &s["releases"].as_array().unwrap()[0];
    assert_eq!(&rel["tag"], tags[0]);
    assert_eq!(rel["draft"], false);
    assert!(w
        .world
        .join(format!("state/disposable-{VERSION}/evidence/receipt.json"))
        .exists());
}

#[test]
fn a_rehearsal_never_lands_tags_or_publishes_and_removes_its_draft() {
    let w = W::new("pipeline::release::rehearsal");
    let main_before = w.store()["refs"]["heads/main"].clone();
    w.release(&["--rehearse"]).passed();
    let s = w.store();
    assert!(s["tags"].as_object().unwrap().is_empty());
    assert!(
        s["releases"].as_array().unwrap().is_empty(),
        "rehearsal draft deleted"
    );
    assert!(s["claims"].as_object().unwrap().is_empty());
    assert_eq!(s["refs"]["heads/main"], main_before);
    assert!(!s["refs"]
        .as_object()
        .unwrap()
        .keys()
        .any(|k| k.starts_with("heads/rehearsal/")));
    assert_eq!(w.calls("dispatch "), 5);
    assert_eq!(w.calls("confirm"), 0);
}

#[test]
fn a_second_clone_is_locked_out_until_an_explicit_takeover() {
    let w = W::new("pipeline::release::takeover");
    w.edit(|s| s["script"]["confirm"] = json!(false));
    w.release(&[]).stopped(2, "BLOCKED", "not-confirmed");
    let other = w.sandbox.work().join("other-clone-state");
    let second = w.release(&["--state", other.to_str().unwrap()]);
    second.stopped(2, "BLOCKED", "attempt-locked");
    let owner = std::fs::read_to_string(w.world.join("state/owner-id")).unwrap();
    assert!(second
        .stderr
        .contains(&format!("TAKEOVER={}", owner.trim())));
    w.edit(|s| s["script"]["confirm"] = json!(true));
    w.release(&[
        "--state",
        other.to_str().unwrap(),
        "--takeover",
        owner.trim(),
    ])
    .passed();
    assert_eq!(w.calls("dispatch "), 5, "the new owner reused every run");
    assert_eq!(w.calls("native-run"), 2, "and every selected native result");
    assert!(w.published().is_some());
}

#[test]
fn supersede_starts_a_new_unsealed_generation_and_is_refused_after_the_seal() {
    let w = W::new("pipeline::release::supersede");
    w.edit(|s| s["script"]["builds"]["awman-linux-arm64"]["fail"] = json!({"1": "product"}));
    w.release(&[]).stopped(1, "FAILED", "build-failed");
    w.edit(|s| s["script"]["builds"]["awman-linux-arm64"]["fail"] = json!({}));
    w.release(&["--supersede"]).passed();
    assert_eq!(w.journal()["generation"], 2);
    assert_eq!(
        w.calls("dispatch "),
        10,
        "a new generation has new operation keys"
    );

    let sealed = W::new("pipeline::release::supersede_sealed");
    sealed.edit(|s| s["script"]["confirm"] = json!(false));
    sealed.release(&[]).stopped(2, "BLOCKED", "not-confirmed");
    sealed
        .release(&["--supersede"])
        .stopped(2, "BLOCKED", "supersede-refused");
}

#[test]
fn tampered_public_bytes_fail_verification_without_a_receipt() {
    let w = W::new("pipeline::release::verify_mismatch");
    w.edit(|s| s["faults"] = json!([{"op": "download_public", "nth": 4, "effect": "mismatch"}]));
    w.release(&[]).stopped(1, "FAILED", "verify-mismatch");
    assert!(!w
        .world
        .join(format!("state/{VERSION}/evidence/receipt.json"))
        .exists());
    let status = w.xtask(&[
        "release",
        "status",
        "--version",
        VERSION,
        "--world",
        w.world.to_str().unwrap(),
    ]);
    assert!(
        status.stdout.contains("published but NOT yet verified"),
        "{}",
        status.stdout
    );
}

#[test]
fn the_seal_binds_the_exact_assets_and_the_checksums() {
    let w = W::new("pipeline::release::seal");
    w.release(&[]).passed();
    let dir = w.world.join(format!("state/{VERSION}/evidence"));
    let seal = std::fs::read(dir.join("seal.json")).unwrap();
    assert_eq!(sha(&seal), w.journal()["release"]["seal_sha256"]);
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    let seal: Value = serde_json::from_slice(&seal).unwrap();
    for a in seal["assets"].as_array().unwrap() {
        let name = a["name"].as_str().unwrap();
        if name.starts_with("awman-") {
            assert!(
                sums.contains(&format!("{}  {name}\n", a["sha256"].as_str().unwrap())),
                "{name}"
            );
        }
    }
    assert_eq!(
        sha_file(&dir.join("SHA256SUMS")),
        seal["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "SHA256SUMS")
            .unwrap()["sha256"]
    );
}

// ── every layer-1 rehearsal scenario, run through `release rehearse` ──────────

const SCENARIOS: &[&str] = &[
    "experimental-sbx-does-not-gate-release",
    "sbx-qualification-changed-on-resume",
    "sbx-qualification-changed-before-seal",
    "sbx-retained-proof-tampered-on-requalification",
    "sbx-pass",
    "sbx-prerequisite-blocked",
    "sbx-product-failed",
    "sbx-kit-blocked",
    "sbx-interrupted",
    "sbx-evidence-tampered",
    "clean",
    "clean-reuse",
    "transient-ci",
    "product-ci",
    "build-startup-failure",
    "host-unreachable-preflight",
    "host-lost-midway",
    "native-interrupted",
    "killed-before-claim",
    "interrupted-collect",
    "dispatch-response-lost",
    "journal-lost",
    "upload-partial",
    "tag-response-lost",
    "publish-retry",
    "verify-retry",
    "verify-mismatch",
    "main-moved",
    "main-moved-policy",
    "main-conflict",
    "controller-checkout-changed",
    "tag-elsewhere",
    "missing-build-record",
    "missing-native-test",
    "native-failed",
    "second-owner",
    "transport-step-retry",
    "hosted-evidence-lost",
    "hosted-evidence-unrecoverable",
    "expired-journal-lost",
    "expired-after-tag",
    "receipt-lost",
    "public-assets-lost",
    "tag-annotation-lost",
    "merge-absorbs-policy",
];

fn sandbox(id: &'static str) -> TestSandbox {
    let mut s = TestSandbox::for_test(TestCase::component(id));
    s.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    s
}

fn rehearse(id: &'static str, scenario: &str) {
    let s = sandbox(id);
    let out_dir = s.work().join("rehearsal");
    let out = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args([
            "release",
            "rehearse",
            "--repeat",
            "1",
            "--scenario",
            scenario,
            "--out",
        ])
        .arg(&out_dir)
        .run();
    let report: Value =
        serde_json::from_slice(&std::fs::read(out_dir.join("report.json")).unwrap()).unwrap();
    let r = &report["scenarios"][0];
    assert_eq!(r["scenario"], scenario);
    assert_eq!(
        r["passed"],
        true,
        "{}\n{}",
        r["failures"],
        String::from_utf8_lossy(&out.stderr)
    );
    let expected_recovery = match scenario {
        "transient-ci"
        | "host-unreachable-preflight"
        | "host-lost-midway"
        | "native-interrupted"
        | "publish-retry" => 1,
        "clean" => 0,
        _ => return assert!(out.exit.success()),
    };
    let recovery = r["recovery"].as_array().unwrap();
    assert_eq!(
        recovery.len(),
        expected_recovery,
        "{scenario}: {recovery:?}"
    );
    for item in recovery {
        assert!(!item["primary_error"]
            .as_str()
            .unwrap_or_default()
            .is_empty());
        assert!(!item["action"].as_str().unwrap_or_default().is_empty());
    }
    if !matches!(scenario, "clean" | "transient-ci") {
        let stops = r["stops"].as_array().unwrap();
        assert_eq!(stops.len(), 1, "{scenario}: expected one primary stop");
        assert_eq!(
            stops[0]["formatted_lines"], 2,
            "{scenario}: one error and one recovery line"
        );
    }
    assert!(out.exit.success());
    assert_eq!(report["kind"], "mixed");
}

macro_rules! scenario_tests {
    ($($name:ident => $scenario:literal,)*) => {
        $(
            #[test]
            fn $name() {
                rehearse(concat!("pipeline::rehearse::", $scenario), $scenario);
            }
        )*
    };
}

scenario_tests! {
    rehearse_experimental_sbx_does_not_gate_release => "experimental-sbx-does-not-gate-release",
    rehearse_sbx_qualification_changed_on_resume => "sbx-qualification-changed-on-resume",
    rehearse_sbx_qualification_changed_before_seal => "sbx-qualification-changed-before-seal",
    rehearse_sbx_retained_proof_tampered_on_requalification => "sbx-retained-proof-tampered-on-requalification",
    rehearse_sbx_pass => "sbx-pass",
    rehearse_sbx_prerequisite_blocked => "sbx-prerequisite-blocked",
    rehearse_sbx_product_failed => "sbx-product-failed",
    rehearse_sbx_kit_blocked => "sbx-kit-blocked",
    rehearse_sbx_interrupted => "sbx-interrupted",
    rehearse_sbx_evidence_tampered => "sbx-evidence-tampered",

    rehearse_clean => "clean",
    rehearse_clean_reuse => "clean-reuse",
    rehearse_transient_ci => "transient-ci",
    rehearse_product_ci => "product-ci",
    rehearse_build_startup_failure => "build-startup-failure",
    rehearse_host_unreachable_preflight => "host-unreachable-preflight",
    rehearse_host_lost_midway => "host-lost-midway",
    rehearse_native_interrupted => "native-interrupted",
    rehearse_killed_before_claim => "killed-before-claim",
    rehearse_interrupted_collect => "interrupted-collect",
    rehearse_dispatch_response_lost => "dispatch-response-lost",
    rehearse_journal_lost => "journal-lost",
    rehearse_upload_partial => "upload-partial",
    rehearse_tag_response_lost => "tag-response-lost",
    rehearse_publish_retry => "publish-retry",
    rehearse_verify_retry => "verify-retry",
    rehearse_verify_mismatch => "verify-mismatch",
    rehearse_main_moved => "main-moved",
    rehearse_main_moved_policy => "main-moved-policy",
    rehearse_main_conflict => "main-conflict",
    rehearse_controller_checkout_changed => "controller-checkout-changed",
    rehearse_tag_elsewhere => "tag-elsewhere",
    rehearse_missing_build_record => "missing-build-record",
    rehearse_missing_native_test => "missing-native-test",
    rehearse_native_failed => "native-failed",
    rehearse_second_owner => "second-owner",
    rehearse_transport_step_retry => "transport-step-retry",
    rehearse_hosted_evidence_lost => "hosted-evidence-lost",
    rehearse_hosted_evidence_unrecoverable => "hosted-evidence-unrecoverable",
    rehearse_expired_journal_lost => "expired-journal-lost",
    rehearse_expired_after_tag => "expired-after-tag",
    rehearse_receipt_lost => "receipt-lost",
    rehearse_public_assets_lost => "public-assets-lost",
    rehearse_tag_annotation_lost => "tag-annotation-lost",
    rehearse_merge_absorbs_policy => "merge-absorbs-policy",
}

#[test]
fn every_rehearsal_scenario_has_a_test() {
    let s = sandbox("pipeline::rehearse::list");
    let out = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["release", "rehearse", "--list"])
        .run();
    out.assert_success();
    let listed: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(listed, SCENARIOS);
}

#[test]
fn incomplete_or_failed_evidence_never_validates_a_release() {
    // Each world lacks one required piece; none may reach the seal.
    type Case = (
        &'static str,
        &'static str,
        fn(&mut Value),
        i32,
        &'static str,
    );
    let cases: [Case; 4] = [
        (
            "pipeline::release::no_build_record",
            "build.json",
            |s| s["script"]["builds"]["awman-linux-amd64"]["missing"] = json!("build.json"),
            1,
            "build-evidence-invalid",
        ),
        (
            "pipeline::release::short_smoke",
            "smoke inventory",
            |s| s["script"]["builds"]["awman-windows-amd64.exe"]["missing"] = json!("smoke-test"),
            1,
            "build-evidence-invalid",
        ),
        (
            "pipeline::release::short_native",
            "native inventory",
            |s| s["script"]["native"]["native.linux-x64"]["omit_test"] = json!(true),
            1,
            "native-result-invalid",
        ),
        (
            "pipeline::release::blocked_native",
            "blocked native check",
            |s| {
                s["script"]["native"]["native.macos-arm64"]["outcome"] = json!("blocked");
                s["script"]["native"]["native.macos-arm64"]["reason_code"] = json!("no-hypervisor");
            },
            2,
            "native-blocked",
        ),
    ];
    for (id, what, edit, code, stop) in cases {
        let w = W::new(id);
        w.edit(edit);
        let label = if code == 1 { "FAILED" } else { "BLOCKED" };
        w.release(&[]).stopped(code, label, stop);
        let s = w.store();
        assert!(
            s["releases"].as_array().unwrap().is_empty(),
            "{what}: a draft was created"
        );
        assert!(s["tags"].as_object().unwrap().is_empty(), "{what}: tagged");
        assert_eq!(w.calls("rerun "), 0, "{what}: retried");
    }
}
