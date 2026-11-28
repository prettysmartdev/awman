//! `make release-rehearse` (design 6.1 layer 1, 6.2, 7.13): every scenario
//! runs the production driver (`run::run`) against a fresh fault world, in
//! process, with a virtual clock. A "killed" fault stops the driver exactly
//! where SIGKILL would; the next invocation builds a new world handle, as a
//! new process would. Virtual release durations are **modelled** from scripted
//! latencies; monotonic wall time around the driver is measured separately and
//! neither is wall-clock publication evidence.

use super::error::Stop;
use super::forge_dir::{self, Dir, Fault, Store};
use super::journal::StateRoot;
use super::run::{self, Options};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

pub const VERSION: &str = "v0.15.0";
pub const LINUX_HOST: &str = "build@kvm1";
const PLAN: &str = include_str!("../../tests/release_contract/fixtures/release-plan.json");
// Explicit optional-SBX scenarios retain all qualification/cleanup refusals.
const PLAN_WITH_SBX: &str =
    include_str!("../../tests/release_contract/fixtures/release-plan-with-sbx.json");

/// One invocation of `make release` and what it must end with.
pub struct Call {
    pub before: fn(&Path),
    pub opts: fn(&mut Options),
    /// `None` = success; otherwise the expected stop code.
    pub expect: Option<&'static str>,
}

pub struct Scenario {
    pub name: &'static str,
    pub setup: fn(&mut Store),
    pub calls: Vec<Call>,
    pub check: fn(&Path, &Store) -> Vec<String>,
}

fn nothing(_: &Path) {}
fn plain(_: &mut Options) {}
fn call(expect: Option<&'static str>) -> Call {
    Call {
        before: nothing,
        opts: plain,
        expect,
    }
}
fn count(s: &Store, prefix: &str) -> usize {
    s.calls.iter().filter(|c| c.starts_with(prefix)).count()
}
fn fault(op: &str, nth: u32, effect: &str) -> Fault {
    Fault {
        op: op.into(),
        nth,
        effect: effect.into(),
    }
}
fn published(s: &Store) -> bool {
    s.releases.iter().any(|r| r.tag == VERSION && !r.draft)
}
fn journal(world: &Path) -> Value {
    std::fs::read(world.join(format!("state/{VERSION}/state.json")))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
fn expect(errors: &mut Vec<String>, ok: bool, what: &str) {
    if !ok {
        errors.push(what.to_string());
    }
}

/// Publication completed with exactly the sealed bytes at R.
fn complete(world: &Path, s: &Store, dispatches: usize) -> Vec<String> {
    let mut e = Vec::new();
    expect(&mut e, published(s), "release is not published");
    expect(
        &mut e,
        s.tags
            .get(VERSION)
            .is_some_and(|t| t.commit == "2".repeat(40) && t.annotated),
        "tag does not point at R",
    );
    expect(
        &mut e,
        count(s, "dispatch ") == dispatches,
        &format!(
            "expected {dispatches} dispatches, saw {}",
            count(s, "dispatch ")
        ),
    );
    expect(
        &mut e,
        world
            .join(format!("state/{VERSION}/evidence/receipt.json"))
            .exists(),
        "no verification receipt",
    );
    // The published evidence asset, read back from the release itself.
    let dir = Dir {
        path: world.to_path_buf(),
    };
    let ev: Value = s
        .releases
        .iter()
        .filter(|r| r.tag == VERSION)
        .flat_map(|r| r.assets.iter())
        .find(|a| a.name == "release-evidence.json")
        .and_then(|a| s.asset_blobs.get(&a.id))
        .and_then(|b| serde_json::from_slice(&dir.blob(b)).ok())
        .unwrap_or(Value::Null);
    expect(
        &mut e,
        ev["assets"].as_array().map(|a| a.len()) == Some(4),
        "evidence does not list 4 assets",
    );
    expect(
        &mut e,
        ev["native"].as_object().is_some_and(|n| {
            n.len()
                == forge_dir::plan_for(s, &"2".repeat(40))["native"]
                    .as_array()
                    .unwrap()
                    .len()
                && n.values().all(|v| v["outcome"] == "passed")
        }),
        "evidence does not list every selected native pass",
    );
    let selected = &ev["sbx_selection"]["hosts"];
    expect(
        &mut e,
        selected.as_array().is_some_and(|hosts| {
            !hosts.is_empty()
                && hosts
                    .iter()
                    .all(|h| ev["sbx_smoke"][h["id"].as_str().unwrap()]["outcome"] == "passed")
        }),
        "seal lost a selected SBX result",
    );
    expect(
        &mut e,
        s.releases
            .iter()
            .filter(|r| r.tag == VERSION)
            .all(|r| r.assets.len() == 6),
        "release does not hold exactly 6 assets",
    );
    e
}

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        Scenario {
            name: "experimental-sbx-does-not-gate-release",
            setup: |s| s.script.sbx_unavailable = Some("sbx-identity-missing".into()),
            calls: vec![call(None)],
            check: |w, s| {
                let mut errors = complete(w, s, 5);
                expect(
                    &mut errors,
                    count(s, "sbx-qualification") == 0
                        && count(s, "native-run native.macos-arm64-sbx ") == 0,
                    "release required SBX identity/provider setup",
                );
                errors
            },
        },
        Scenario {
            name: "sbx-qualification-changed-on-resume",
            setup: |s| s.faults.push(fault("checkpoint:seal", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| s.script.sbx_version = Some("0.54.2".into()));
                    },
                    opts: plain,
                    expect: None,
                },
            ],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64-sbx ") == 2
                        && count(s, "native-run native.macos-arm64 on") == 1
                        && count(s, "native-run native.linux-x64 ") == 1,
                    "requalification did not rerun only SBX",
                );
                expect(
                    &mut e,
                    std::fs::read_dir(w.join(format!("state/{VERSION}/native")))
                        .unwrap()
                        .any(|e| {
                            e.unwrap()
                                .file_name()
                                .to_string_lossy()
                                .starts_with("native.macos-arm64-sbx-previous-")
                        }),
                    "old proof not archived",
                );
                e
            },
        },
        Scenario {
            name: "sbx-qualification-changed-before-seal",
            setup: |s| s.script.sbx_change_before_seal = true,
            calls: vec![call(Some("native-result-invalid"))],
            check: |w, s| {
                let mut e = vec![];
                expect(
                    &mut e,
                    journal(w)["release"]["seal_sha256"].is_null() && s.releases.is_empty(),
                    "changed qualification sealed",
                );
                e
            },
        },
        Scenario {
            name: "sbx-retained-proof-tampered-on-requalification",
            setup: |s| s.faults.push(fault("checkpoint:seal", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| s.script.sbx_label = Some("changed label".into()));
                        std::fs::write(w.join(format!("state/{VERSION}/native/native.macos-arm64-sbx/sbx-smoke.claude.json")), "{}").unwrap();
                    },
                    opts: plain,
                    expect: Some("native-result-invalid"),
                },
            ],
            check: |_, s| {
                let mut e = vec![];
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64-sbx ") == 1 && s.releases.is_empty(),
                    "tampered proof reran or sealed",
                );
                e
            },
        },
        Scenario {
            name: "sbx-pass",
            setup: |_| {},
            calls: vec![call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                let n = &journal(w)["native"]["native.macos-arm64-sbx"];
                expect(
                    &mut e,
                    n["outcome"] == "passed"
                        && n["sbx"]["cli_version"] == "0.54.1"
                        && n["sbx"]["operator_label"] == "release-test-account"
                        && n["sbx"]["cases"].as_array().is_some_and(|a| a.len() == 2),
                    "SBX qualification/cases not sealed",
                );
                expect(
                    &mut e,
                    std::fs::read_to_string(w.join("last-summary.txt"))
                        .unwrap()
                        .contains("native.macos-arm64-sbx"),
                    "SBX result missing from release summary",
                );
                e
            },
        },
        Scenario {
            name: "sbx-prerequisite-blocked",
            setup: |s| s.script.sbx_unavailable = Some("sbx-identity-missing".into()),
            calls: vec![call(Some("sbx-identity-missing"))],
            check: |_, s| {
                let mut e = vec![];
                expect(
                    &mut e,
                    count(s, "dispatch ") == 0 && s.releases.is_empty(),
                    "SBX missing prerequisite dispatched or sealed work",
                );
                e
            },
        },
        Scenario {
            name: "sbx-product-failed",
            setup: |s| {
                s.script
                    .native
                    .get_mut("native.macos-arm64-sbx")
                    .unwrap()
                    .outcome = Some("failed".into());
            },
            calls: vec![call(Some("native-failed")), call(Some("native-failed"))],
            check: |_, s| {
                let mut e = vec![];
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64-sbx ") == 1 && s.releases.is_empty(),
                    "failed SBX check retried or sealed",
                );
                e
            },
        },
        Scenario {
            name: "sbx-kit-blocked",
            setup: |s| {
                let n = s.script.native.get_mut("native.macos-arm64-sbx").unwrap();
                n.outcome = Some("blocked".into());
                n.reason_code = Some("sbx-not-signed-in".into());
            },
            calls: vec![call(Some("native-blocked"))],
            check: |_, s| {
                let mut e = vec![];
                expect(&mut e, s.releases.is_empty(), "blocked SBX check sealed");
                e
            },
        },
        Scenario {
            name: "sbx-interrupted",
            setup: |s| {
                s.script
                    .native
                    .get_mut("native.macos-arm64-sbx")
                    .unwrap()
                    .run_s = 600;
                s.faults
                    .push(fault("native_run:native.macos-arm64-sbx", 1, "crash"))
            },
            calls: vec![call(Some("killed")), call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64 on") == 1
                        && count(s, "native-run native.linux-x64 ") == 1,
                    "interrupted SBX reran a verified boot check",
                );
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64-sbx ") == 2,
                    "interrupted SBX did not recover exactly once",
                );
                e
            },
        },
        Scenario {
            name: "sbx-evidence-tampered",
            setup: |s| s.faults.push(fault("checkpoint:seal", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        std::fs::write(w.join(format!("state/{VERSION}/native/native.macos-arm64-sbx/sbx-smoke.claude.json")), "{}").unwrap();
                    },
                    opts: plain,
                    expect: Some("native-result-invalid"),
                },
            ],
            check: |_, s| {
                let mut e = vec![];
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64-sbx ") == 1 && s.releases.is_empty(),
                    "tampered SBX evidence reran or sealed",
                );
                e
            },
        },
        Scenario {
            name: "clean",
            setup: |_| {},
            calls: vec![call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "confirm") == 1,
                    "confirmation not asked exactly once",
                );
                e
            },
        },
        Scenario {
            name: "clean-reuse",
            setup: |s| {
                s.runs.push(forge_dir::Run {
                    info: super::forge::RunInfo {
                        id: 900,
                        workflow: "ci.yml".into(),
                        title: "CI push".into(),
                        head_sha: "2".repeat(40),
                        event: "push".into(),
                        status: "completed".into(),
                        conclusion: Some("success".into()),
                        attempt: 1,
                        created: s.clock,
                        updated: s.clock,
                    },
                    attempt_started: s.clock,
                    materialized: true,
                    ..Default::default()
                });
            },
            calls: vec![call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 4);
                expect(
                    &mut e,
                    journal(w)["ci"]["reused"] == true,
                    "CI was not reused",
                );
                e
            },
        },
        Scenario {
            name: "transient-ci",
            setup: |s| {
                s.script.ci.fail.insert(1, "infra".into());
            },
            calls: vec![call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "rerun ") == 1,
                    "failed CI jobs were not rerun exactly once",
                );
                expect(
                    &mut e,
                    journal(w)["ci"]["run_attempt"] == 2,
                    "CI attempt is not 2",
                );
                e
            },
        },
        Scenario {
            name: "product-ci",
            setup: |s| {
                s.script.ci.fail.insert(1, "product".into());
                s.script.ci.fail.insert(2, "product".into());
            },
            calls: vec![call(Some("ci-failed")), call(Some("ci-failed"))],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    count(s, "rerun ") == 0,
                    "a product failure was retried",
                );
                expect(
                    &mut e,
                    count(s, "dispatch ") == 5,
                    "resume dispatched again",
                );
                expect(
                    &mut e,
                    !published(s) && s.tags.is_empty(),
                    "something was published",
                );
                e
            },
        },
        Scenario {
            name: "build-startup-failure",
            setup: |s| {
                s.script
                    .builds
                    .get_mut("awman-linux-arm64")
                    .unwrap()
                    .fail
                    .insert(1, "startup".into());
            },
            calls: vec![call(Some("workflow-rejected"))],
            check: |w, s| {
                let mut e = Vec::new();
                let pinned = journal(w)["builds"]
                    .as_object()
                    .map(|b| {
                        b.values()
                            .filter(|v| v["dispatch"]["run_id"].is_u64())
                            .count()
                    })
                    .unwrap_or(0);
                expect(
                    &mut e,
                    pinned == 4,
                    "the other build runs were not kept pinned",
                );
                expect(
                    &mut e,
                    count(s, "cancel ") == 0,
                    "independent build runs were cancelled",
                );
                expect(&mut e, !published(s), "something was published");
                e
            },
        },
        Scenario {
            name: "host-unreachable-preflight",
            setup: |s| s.script.unreachable.push(LINUX_HOST.into()),
            calls: vec![call(Some("linux-host-unreachable"))],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    count(s, "dispatch ") == 0 && count(s, "push ") == 0,
                    "work started before preflight passed",
                );
                expect(
                    &mut e,
                    count(s, "claim ") == 0,
                    "a claim was created before preflight passed",
                );
                e
            },
        },
        Scenario {
            name: "host-lost-midway",
            setup: |s| {
                s.faults
                    .push(fault("native_run:native.linux-x64", 1, "error"))
            },
            calls: vec![call(Some("linux-host-unreachable")), call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64 on") == 1,
                    "the macOS result was not reused",
                );
                expect(
                    &mut e,
                    count(s, "native-run native.linux-x64 on") == 2,
                    "the Linux check did not rerun exactly once",
                );
                e
            },
        },
        Scenario {
            name: "native-interrupted",
            setup: |s| {
                s.faults
                    .push(fault("native_run:native.macos-arm64", 1, "crash"))
            },
            calls: vec![call(Some("killed")), call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    s.calls
                        .iter()
                        .filter(|c| c.as_str() == "native-orphan-stopped native.macos-arm64")
                        .count()
                        == 1,
                    "the orphan was not stopped",
                );
                expect(
                    &mut e,
                    count(s, "native-run native.linux-x64 on") == 1,
                    "the passed Linux check reran",
                );
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64 on") == 2,
                    "only the interrupted Mac check should restart",
                );
                e
            },
        },
        Scenario {
            name: "killed-before-claim",
            setup: |s| s.faults.push(fault("checkpoint:pin", 1, "crash")),
            calls: vec![call(Some("killed")), call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "claim v0.15.0 create") == 1,
                    "the claim was not created exactly once on resume",
                );
                e
            },
        },
        Scenario {
            name: "interrupted-collect",
            setup: |s| s.faults.push(fault("run", 4, "crash")),
            calls: vec![call(Some("killed")), call(None)],
            check: |w, s| complete(w, s, 5),
        },
        Scenario {
            name: "dispatch-response-lost",
            setup: |s| {
                s.faults
                    .push(fault("dispatch:release.yml", 2, "error-after-commit"))
            },
            calls: vec![call(None)],
            check: |w, s| complete(w, s, 5),
        },
        Scenario {
            name: "journal-lost",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: |w| {
                        let _ = std::fs::remove_dir_all(w.join(format!("state/{VERSION}")));
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| s.script.confirm = true);
                    },
                    opts: plain,
                    expect: None,
                },
            ],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "native-run") == 4,
                    "native checks reran after the seal",
                );
                e
            },
        },
        Scenario {
            name: "upload-partial",
            setup: |s| {
                s.faults
                    .push(fault("upload_asset", 2, "error-after-commit"));
                s.faults.push(fault("upload_asset", 3, "stub"));
            },
            calls: vec![call(Some("network")), call(Some("network")), call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "delete-asset") == 1,
                    "the owned stub was not replaced exactly once",
                );
                e
            },
        },
        Scenario {
            name: "tag-response-lost",
            setup: |s| s.faults.push(fault("create_tag", 1, "error-after-commit")),
            calls: vec![call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "tag ") == 1,
                    "the tag was created more than once",
                );
                e
            },
        },
        Scenario {
            name: "publish-retry",
            setup: |s| s.faults.push(fault("publish", 1, "error")),
            calls: vec![call(Some("publish-failed")), call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(&mut e, count(s, "tag ") == 1, "the tag was touched again");
                expect(
                    &mut e,
                    count(s, "native-run") == 4,
                    "promotion retry reran native checks",
                );
                e
            },
        },
        Scenario {
            name: "verify-retry",
            setup: |s| s.faults.push(fault("download_public", 4, "error")),
            calls: vec![call(None)],
            check: |w, s| complete(w, s, 5),
        },
        Scenario {
            name: "verify-mismatch",
            setup: |s| s.faults.push(fault("download_public", 4, "mismatch")),
            calls: vec![call(Some("verify-mismatch"))],
            check: |w, _| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    !w.join(format!("state/{VERSION}/evidence/receipt.json"))
                        .exists(),
                    "a receipt was written for mismatched bytes",
                );
                e
            },
        },
        Scenario {
            name: "main-moved",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: move_main::<false>,
                    opts: plain,
                    expect: None,
                },
            ],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(&mut e, count(s, "merge main") == 1, "main was not merged");
                e
            },
        },
        Scenario {
            name: "main-moved-policy",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: move_main::<true>,
                    opts: plain,
                    expect: Some("policy-changed"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    !published(s) && s.tags.is_empty(),
                    "published under a changed policy",
                );
                e
            },
        },
        Scenario {
            name: "main-conflict",
            setup: |s| {
                s.script.confirm = false;
                s.script.merge_conflict = true;
            },
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: move_main::<false>,
                    opts: plain,
                    expect: Some("land-conflict"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(&mut e, s.tags.is_empty(), "tagged before landing");
                e
            },
        },
        Scenario {
            name: "controller-checkout-changed",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: change_head,
                    opts: |o| o.supersede = true,
                    expect: Some("supersede-refused"),
                },
                Call {
                    before: confirm_yes,
                    opts: plain,
                    expect: None,
                },
            ],
            check: |w, s| complete(w, s, 5),
        },
        Scenario {
            name: "tag-elsewhere",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: |w| {
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| {
                            s.script.confirm = true;
                            s.tags.insert(
                                VERSION.into(),
                                super::forge::TagInfo {
                                    commit: "1".repeat(40),
                                    annotated: true,
                                    message: "other".into(),
                                },
                            );
                        })
                    },
                    opts: plain,
                    expect: Some("tag-foreign"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(&mut e, !published(s), "published despite a foreign tag");
                e
            },
        },
        Scenario {
            name: "missing-build-record",
            setup: |s| {
                s.script
                    .builds
                    .get_mut("awman-linux-amd64")
                    .unwrap()
                    .missing = Some("build.json".into())
            },
            calls: vec![call(Some("build-evidence-invalid"))],
            check: |_, s| {
                let mut e = Vec::new();
                expect(&mut e, !published(s), "published without build.json");
                e
            },
        },
        Scenario {
            name: "missing-native-test",
            setup: |s| {
                s.script
                    .native
                    .get_mut("native.linux-x64")
                    .unwrap()
                    .omit_test = true
            },
            calls: vec![call(Some("native-result-invalid"))],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    !published(s) && s.releases.is_empty(),
                    "sealed without a complete native result",
                );
                e
            },
        },
        Scenario {
            name: "native-failed",
            setup: |s| {
                s.script
                    .native
                    .get_mut("native.macos-arm64")
                    .unwrap()
                    .outcome = Some("failed".into())
            },
            calls: vec![call(Some("native-failed")), call(Some("native-failed"))],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    count(s, "native-run native.macos-arm64 on") == 1,
                    "a failed native check was retried",
                );
                expect(
                    &mut e,
                    s.releases.is_empty(),
                    "a draft exists after a native failure",
                );
                e
            },
        },
        Scenario {
            name: "second-owner",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: |w| {
                        let _ = std::fs::rename(w.join("state"), w.join("state-first"));
                    },
                    opts: plain,
                    expect: Some("attempt-locked"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    count(s, "claim ") >= 1 && !published(s),
                    "a second owner mutated the release",
                );
                e
            },
        },
        // ── adversarial-review regressions ──────────────────────────────
        Scenario {
            name: "transport-step-retry",
            setup: |s| {
                s.source
                    .commits
                    .get_mut(&"2".repeat(40))
                    .unwrap()
                    .files
                    .insert(".github/workflows/release.yml".into(), RELEASE_YML.into());
                s.script
                    .builds
                    .get_mut("awman-linux-amd64")
                    .unwrap()
                    .fail
                    .insert(1, "transport".into());
            },
            calls: vec![call(None)],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(
                    &mut e,
                    count(s, "rerun ") == 1,
                    "a failed upload step was not retried once as infrastructure",
                );
                e
            },
        },
        Scenario {
            name: "hosted-evidence-lost",
            setup: |s| s.faults.push(fault("checkpoint:seal", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: remove_hosted_evidence,
                    opts: plain,
                    expect: None,
                },
            ],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(&mut e, count(s, "rerun ") == 0, "hosted work was rerun");
                expect(
                    &mut e,
                    std::fs::read_dir(w.join(format!("state/{VERSION}/ci")))
                        .is_ok_and(|d| d.count() > 0),
                    "CI evidence was not re-verified from the pinned run",
                );
                e
            },
        },
        Scenario {
            name: "hosted-evidence-unrecoverable",
            setup: |s| s.faults.push(fault("checkpoint:seal", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        remove_hosted_evidence(w);
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| s.artifacts.values_mut().for_each(|a| a.expired = true));
                    },
                    opts: plain,
                    expect: Some("hosted-evidence-missing"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    s.releases.is_empty() && s.tags.is_empty(),
                    "sealed without the hosted evidence",
                );
                e
            },
        },
        Scenario {
            name: "expired-journal-lost",
            setup: |s| s.faults.push(fault("checkpoint:confirm", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        later(w, 30);
                        let _ = std::fs::remove_file(w.join(format!("state/{VERSION}/state.json")));
                    },
                    opts: plain,
                    expect: Some("evidence-expired"),
                },
            ],
            check: |w, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    !published(s) && s.tags.is_empty(),
                    "stale evidence was published after the journal was rebuilt",
                );
                expect(
                    &mut e,
                    journal(w)["native"]
                        .as_object()
                        .is_some_and(|n| n.len() == 4),
                    "the rebuilt journal lost the sealed native records",
                );
                e
            },
        },
        Scenario {
            name: "expired-after-tag",
            setup: |s| s.faults.push(fault("checkpoint:publish", 1, "crash")),
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| later(w, 30),
                    opts: plain,
                    expect: Some("evidence-expired"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    !published(s),
                    "stale evidence was published after tagging",
                );
                expect(&mut e, s.tags.contains_key(VERSION), "the tag was removed");
                e
            },
        },
        Scenario {
            name: "receipt-lost",
            setup: |_| {},
            calls: vec![
                call(None),
                Call {
                    before: |w| {
                        let _ = std::fs::remove_file(
                            w.join(format!("state/{VERSION}/evidence/receipt.json")),
                        );
                    },
                    opts: plain,
                    expect: None,
                },
            ],
            check: |w, s| {
                let mut e = complete(w, s, 5);
                expect(&mut e, count(s, "tag ") == 1, "the tag was touched again");
                e
            },
        },
        Scenario {
            name: "public-assets-lost",
            setup: |_| {},
            calls: vec![
                call(None),
                Call {
                    before: |w| {
                        let _ = std::fs::remove_file(
                            w.join(format!("state/{VERSION}/evidence/receipt.json")),
                        );
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| s.releases.iter_mut().for_each(|r| r.assets.clear()));
                    },
                    opts: plain,
                    expect: Some("seal-mismatch"),
                },
            ],
            check: |w, _| {
                let mut e = Vec::new();
                expect(
                    &mut e,
                    !w.join(format!("state/{VERSION}/evidence/receipt.json"))
                        .exists(),
                    "a receipt was written for a release without its assets",
                );
                e
            },
        },
        Scenario {
            name: "tag-annotation-lost",
            setup: |_| {},
            calls: vec![
                call(None),
                Call {
                    before: |w| {
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| {
                            let t = s.tags.get_mut(VERSION).unwrap();
                            t.annotated = false;
                            t.message.clear();
                        })
                    },
                    opts: plain,
                    expect: Some("tag-foreign"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(&mut e, count(s, "tag ") == 1, "the tag was rewritten");
                e
            },
        },
        Scenario {
            name: "merge-absorbs-policy",
            setup: |s| s.script.confirm = false,
            calls: vec![
                call(Some("not-confirmed")),
                Call {
                    before: |w| {
                        move_main::<false>(w);
                        Dir {
                            path: w.to_path_buf(),
                        }
                        .with(|s| s.script.merge_policy = Some("9".repeat(64)));
                    },
                    opts: plain,
                    expect: Some("policy-changed"),
                },
            ],
            check: |_, s| {
                let mut e = Vec::new();
                expect(&mut e, count(s, "merge main") == 1, "main was not merged");
                expect(
                    &mut e,
                    !published(s) && s.tags.is_empty(),
                    "tagged or published under a policy absorbed by the merge",
                );
                e
            },
        },
    ];
    scenarios.extend(super::runtime_smoke::rehearsal::scenarios());
    scenarios
}

/// The release workflow's transport steps as the classifier reads them at R.
const RELEASE_YML: &str = "jobs:\n  validate:\n    steps:\n      - name: Smoke\n        run: cargo xtask smoke\n      - name: Validated release asset\n        uses: actions/upload-artifact@v7\n";

/// Delete the retained hosted proof the seal depends on (verified CI JSON
/// and every `build.json`), keeping the journal.
fn remove_hosted_evidence(w: &Path) {
    let state = w.join(format!("state/{VERSION}"));
    if let Ok(d) = std::fs::read_dir(state.join("ci")) {
        for f in d.flatten() {
            let _ = std::fs::remove_file(f.path());
        }
    }
    if let Ok(d) = std::fs::read_dir(state.join("assets")) {
        for f in d.flatten().filter(|f| f.path().is_dir()) {
            let _ = std::fs::remove_file(f.path().join("build.json"));
        }
    }
}

/// Advance the world clock by `days`.
fn later(w: &Path, days: i64) {
    Dir {
        path: w.to_path_buf(),
    }
    .with(|s| s.clock += chrono::Duration::days(days));
}

fn confirm_yes(w: &Path) {
    Dir {
        path: w.to_path_buf(),
    }
    .with(|s| s.script.confirm = true);
}

fn change_head(w: &Path) {
    Dir {
        path: w.to_path_buf(),
    }
    .with(|s| {
        let other = "3".repeat(40);
        let mut c = s.source.commits[&"2".repeat(40)].clone();
        c.parents = vec!["2".repeat(40)];
        c.tree = "c".repeat(40);
        s.source.commits.insert(other.clone(), c);
        s.source.head = other;
    });
}

/// main advances with an unrelated commit; `POLICY` also changes the plan.
fn move_main<const POLICY: bool>(w: &Path) {
    Dir {
        path: w.to_path_buf(),
    }
    .with(|s| {
        s.script.confirm = true;
        let m2 = "4".repeat(40);
        s.source.commits.insert(
            m2.clone(),
            forge_dir::Commit {
                tree: "d".repeat(40),
                parents: vec!["1".repeat(40)],
                files: Default::default(),
            },
        );
        s.refs.insert("heads/main".into(), m2.clone());
        // Another developer pushed it: the controller's clone has not
        // fetched it yet (finding: the policy recheck must fetch).
        s.source.remote_only.insert(m2.clone());
        if POLICY {
            let mut p = s.plans["default"].clone();
            p["policy_sha256"] = "9".repeat(64).into();
            s.plans.insert(m2, p);
        }
    });
}

/// Deterministic xorshift for modelled latencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

/// Latencies sampled from the measured v0.14 ranges (design 1.1), plus the
/// observed 603 s hosted macOS queue in one run of ten. Modelled only.
fn sample(s: &mut Store, rng: &mut Rng) {
    s.script.ci.queue_s = rng.range(2, 15);
    s.script.ci.run_s = rng.range(600, 840);
    for (asset, (lo, hi)) in [
        ("awman-linux-amd64", (540, 780)),
        ("awman-linux-arm64", (480, 600)),
        ("awman-macos-arm64", (900, 1500)),
        ("awman-windows-amd64.exe", (840, 1260)),
    ] {
        let b = s.script.builds.get_mut(asset).unwrap();
        b.queue_s = if asset == "awman-macos-arm64" && rng.range(1, 10) == 1 {
            603
        } else {
            rng.range(2, 15)
        };
        b.run_s = rng.range(lo, hi);
    }
    for n in s.script.native.values_mut() {
        n.run_s = rng.range(180, 300);
    }
}

pub struct Outcome {
    pub failures: Vec<String>,
    pub codes: Vec<String>,
    pub minutes: f64,
    pub dispatches: usize,
    pub reruns: usize,
    pub wall_ms: f64,
    pub recovery_wall_ms: f64,
    pub recovery_modelled_minutes: Option<f64>,
    pub stops: Vec<Value>,
}

fn tagged_fingerprint(s: &Store) -> Value {
    let tag = s.tags.get(VERSION);
    let assets: Vec<_> = s
        .releases
        .iter()
        .filter(|r| r.tag == VERSION)
        .flat_map(|r| r.assets.iter())
        .map(|a| json!({"name": a.name, "size": a.size, "digest": a.digest}))
        .collect();
    match tag {
        None => Value::Null,
        Some(tag) => json!({"commit": tag.commit, "annotated": tag.annotated,
            "message": tag.message, "assets": assets}),
    }
}

/// Run one scenario once in `world`.
pub fn run_one(sc: &Scenario, world: &Path, seed: Option<u64>) -> Outcome {
    let plan: Value = serde_json::from_str(if sc.name.starts_with("sbx-") {
        PLAN_WITH_SBX
    } else {
        PLAN
    })
    .expect("fixture plan");
    let _ = std::fs::remove_dir_all(world);
    std::fs::create_dir_all(world).expect("world dir");
    forge_dir::seed(world, VERSION, plan);
    let dir = Dir {
        path: world.to_path_buf(),
    };
    dir.with(|s| {
        if let Some(seed) = seed {
            sample(s, &mut Rng(seed | 1));
        }
        (sc.setup)(s);
    });
    let start = dir.load().clock;
    let initial_source = dir.load().source;
    let initial_head = initial_source.head.clone();
    let mut initial_commits: Vec<_> = initial_source.commits.keys().cloned().collect();
    initial_commits.sort();
    let wall_start = Instant::now();
    let mut failures = Vec::new();
    let mut codes = Vec::new();
    let mut stops = Vec::new();
    let mut recovery_wall_ms = 0.0;
    let mut expected_tag: Option<Value> = None;
    for (i, c) in sc.calls.iter().enumerate() {
        (c.before)(world);
        // A scenario's own hook may damage the published tag or assets (an
        // external actor); the invariant is that the *driver* never does.
        if expected_tag.is_some() {
            let after_hook = tagged_fingerprint(&dir.load());
            if !after_hook.is_null() {
                expected_tag = Some(after_hook);
            }
        }
        let mut opts = Options {
            linux_host: Some(LINUX_HOST.into()),
            ..Default::default()
        };
        (c.opts)(&mut opts);
        let w = forge_dir::world(world, Arc::new(AtomicBool::new(false)));
        let call_start = Instant::now();
        let result =
            StateRoot::new(world.join("state")).and_then(|root| run::run(&w, &root, &opts));
        let call_wall = call_start.elapsed().as_secs_f64() * 1000.0;
        if i > 0 && sc.calls[..i].iter().any(|prior| prior.expect.is_some()) {
            recovery_wall_ms += call_wall;
        }
        let got = result.as_ref().err().map(|s: &Stop| s.code.clone());
        if let Err(stop) = &result {
            stops.push(
                json!({"call": i + 1, "code": stop.code, "message": stop.message,
                "recovery": stop.remedy, "formatted_lines": stop.to_string().lines().count()}),
            );
        }
        codes.push(got.clone().unwrap_or_else(|| "ok".into()));
        if got.as_deref() != c.expect {
            failures.push(format!(
                "call {}: expected {}, got {}",
                i + 1,
                c.expect.unwrap_or("success"),
                result
                    .err()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "success".into())
            ));
        }
        let current_tag = tagged_fingerprint(&dir.load());
        if !current_tag.is_null() {
            if let Some(previous) = &expected_tag {
                if previous != &current_tag {
                    failures.push(format!(
                        "call {} changed the existing tag or its asset identity",
                        i + 1
                    ));
                }
            } else {
                expected_tag = Some(current_tag);
            }
        }
        if matches!(
            sc.name,
            "clean"
                | "transient-ci"
                | "host-unreachable-preflight"
                | "host-lost-midway"
                | "native-interrupted"
                | "publish-retry"
        ) {
            let source = dir.load().source;
            let mut commits: Vec<_> = source.commits.keys().cloned().collect();
            commits.sort();
            if source.head != initial_head || commits != initial_commits {
                failures.push(format!(
                    "call {} changed the source head or created a commit",
                    i + 1
                ));
            }
        }
    }
    let store = dir.load();
    let recovery_modelled_minutes = if sc.name == "transient-ci" {
        Some(
            store
                .runs
                .iter()
                .filter(|r| r.info.workflow == "ci.yml" && r.info.attempt > 1)
                .map(|r| (r.queue_s + r.run_s) as f64 / 60.0)
                .sum(),
        )
    } else {
        None
    };
    failures.extend((sc.check)(world, &store));
    Outcome {
        failures,
        codes,
        minutes: (store.clock - start).num_seconds() as f64 / 60.0,
        dispatches: count(&store, "dispatch "),
        reruns: count(&store, "rerun "),
        wall_ms: wall_start.elapsed().as_secs_f64() * 1000.0,
        recovery_wall_ms,
        recovery_modelled_minutes,
        stops,
    }
}

fn nearest_rank(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

pub fn run(args: &[String]) -> anyhow::Result<std::process::ExitCode> {
    let get = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let repeat: usize = get("--repeat")
        .map(|r| r.parse())
        .transpose()?
        .unwrap_or(10);
    let out = PathBuf::from(get("--out").unwrap_or_else(|| "target/release-rehearsal".into()));
    let only = get("--scenario");
    if args.iter().any(|a| a == "--list") {
        for s in scenarios() {
            println!("{}", s.name);
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }
    std::fs::create_dir_all(&out)?;
    let work = tempfile::Builder::new().prefix("work-").tempdir_in(&out)?;
    let mut report = Vec::new();
    let mut md = String::from(
        "# Release rehearsal (layer 1, fault-injected, measured overhead and modelled time)\n\n",
    );
    md.push_str("Orchestration wall columns are measured with a monotonic clock around the production driver in this container. Virtual-clock release durations are **modelled**, not wall-clock publication evidence.\n\n");
    md.push_str("| scenario | runs | passed | exit codes (run 1) | dispatches | reruns | orchestration wall p50/p95 ms | recovery wall p50/p95 ms | modelled p50/p95 min |\n|---|---|---|---|---|---|---|---|---|\n");
    let mut all_ok = true;
    for sc in scenarios()
        .iter()
        .filter(|s| only.as_deref().is_none_or(|o| o == s.name))
    {
        let mut minutes = Vec::new();
        let mut wall_ms = Vec::new();
        let mut recovery_wall_ms = Vec::new();
        let mut recovery_modelled_minutes = Vec::new();
        let mut failures = Vec::new();
        let mut first: Option<Outcome> = None;
        for i in 0..repeat {
            let o = run_one(
                sc,
                &work.path().join(sc.name),
                Some(0x9e37_79b9 + i as u64 * 7919),
            );
            minutes.push(o.minutes);
            wall_ms.push(o.wall_ms);
            recovery_wall_ms.push(o.recovery_wall_ms);
            if let Some(m) = o.recovery_modelled_minutes {
                recovery_modelled_minutes.push(m);
            }
            failures.extend(o.failures.iter().map(|f| format!("run {}: {f}", i + 1)));
            first.get_or_insert(o);
        }
        minutes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        wall_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        recovery_wall_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        recovery_modelled_minutes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let first = first.expect("at least one run");
        let passed = failures.is_empty();
        all_ok &= passed;
        let recovery = if sc.name == "transient-ci" {
            json!([{"primary_error": "CI infrastructure attempt 1 failed", "action": "rerun only failed CI jobs on the same run; keep the four release builds", "mode": "automatic-retry"}])
        } else {
            Value::Array(
                first
                    .stops
                    .iter()
                    .map(|stop| {
                        json!({
                            "primary_error": stop["code"],
                            "action": stop["recovery"],
                            "mode": "operator-resume"
                        })
                    })
                    .collect(),
            )
        };
        md.push_str(&format!(
            "| {} | {repeat} | {} | {} | {} | {} | {:.1}/{:.1} | {:.1}/{:.1} | {:.1}/{:.1} |\n",
            sc.name,
            if passed { "yes" } else { "**no**" },
            first.codes.join(" → "),
            first.dispatches,
            first.reruns,
            nearest_rank(&wall_ms, 0.5),
            nearest_rank(&wall_ms, 0.95),
            nearest_rank(&recovery_wall_ms, 0.5),
            nearest_rank(&recovery_wall_ms, 0.95),
            nearest_rank(&minutes, 0.5),
            nearest_rank(&minutes, 0.95)
        ));
        for f in failures.iter().take(5) {
            eprintln!("rehearse {}: {f}", sc.name);
        }
        report.push(json!({"scenario": sc.name, "runs": repeat, "passed": passed, "failures": failures,
            "exit_codes": first.codes, "stops": first.stops, "recovery": recovery,
            "dispatches": first.dispatches, "reruns": first.reruns,
            "orchestration_wall_ms": {"samples": wall_ms, "p50": nearest_rank(&wall_ms, 0.5), "p95": nearest_rank(&wall_ms, 0.95)},
            "recovery_wall_ms": {"samples": recovery_wall_ms, "p50": nearest_rank(&recovery_wall_ms, 0.5), "p95": nearest_rank(&recovery_wall_ms, 0.95)},
            "recovery_modelled_minutes": if recovery_modelled_minutes.is_empty() { Value::Null } else { json!({"samples": recovery_modelled_minutes, "p50": nearest_rank(&recovery_modelled_minutes, 0.5), "p95": nearest_rank(&recovery_modelled_minutes, 0.95)}) },
            "modelled_minutes": {"samples": minutes, "p50": nearest_rank(&minutes, 0.5), "p95": nearest_rank(&minutes, 0.95)}}));
    }
    let doc = json!({"schema": 1, "kind": "mixed", "orchestration_wall": "measured-monotonic", "virtual_clock": "modelled", "percentile": "nearest-rank ceil(p*N)", "scenarios": report});
    std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&doc)?)?;
    std::fs::write(out.join("report.md"), &md)?;
    print!("{md}");
    println!("\nreport: {}", out.join("report.json").display());
    Ok(if all_ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    })
}
