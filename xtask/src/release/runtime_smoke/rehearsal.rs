//! Release-contract faults for the lightweight obligations (no service claims).
use crate::release::{
    forge_dir::{Dir, Fault, NativeScript, Store},
    rehearse::{Call, Scenario, VERSION},
    run::Options,
};
use std::path::Path;
fn noop(_: &Path) {}
fn plain(_: &mut Options) {}
fn call(expect: Option<&'static str>) -> Call {
    Call {
        before: noop,
        opts: plain,
        expect,
    }
}
fn inject(s: &mut Store, outcome: &str, reason: &str) {
    s.script.sbx_smoke.insert(
        "sbx.linux-x64".into(),
        NativeScript {
            outcome: Some(outcome.into()),
            reason_code: Some(reason.into()),
            ..Default::default()
        },
    );
}
fn halted(_: &Path, s: &Store) -> Vec<String> {
    if s.releases.iter().any(|r| !r.draft) {
        vec!["published with incomplete runtime evidence".into()]
    } else {
        vec![]
    }
}
fn freeze(s: &mut Store) {
    s.faults.push(Fault {
        op: "checkpoint:seal".into(),
        nth: 1,
        effect: "crash".into(),
    });
}
fn path(w: &Path, rel: &str) -> std::path::PathBuf {
    w.join(format!("state/{VERSION}/{rel}"))
}
fn count(s: &Store, prefix: &str) -> usize {
    s.calls.iter().filter(|s| s.starts_with(prefix)).count()
}
fn expect(errors: &mut Vec<String>, ok: bool, message: &str) {
    if !ok {
        errors.push(message.into());
    }
}
fn once(_: &Path, s: &Store) -> Vec<String> {
    let mut errors = vec![];
    for id in ["sbx.macos-arm64", "sbx.linux-x64"] {
        if count(s, &format!("sbx-smoke-run {id}")) != 1 {
            errors.push(format!("{id} reran or was skipped"));
        }
    }
    if !s.releases.iter().any(|r| !r.draft) {
        errors.push("release was not published".into());
    }
    errors
}
pub(crate) fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "runtime-sbx-supersede-interrupted",
            setup: |s| {
                s.faults.push(Fault {
                    op: "sbx_smoke:sbx.linux-x64".into(),
                    nth: 1,
                    effect: "crash".into(),
                })
            },
            calls: vec![
                call(Some("killed")),
                Call {
                    opts: |o| o.supersede = true,
                    ..call(None)
                },
            ],
            check: |_, s| {
                if count(s, "sbx-smoke-reconcile sbx.linux-x64") == 1
                    && count(s, "sbx-smoke-run sbx.linux-x64") == 2
                {
                    vec![]
                } else {
                    vec!["supersede discarded unfinished SBX ownership".into()]
                }
            },
        },
        Scenario {
            name: "runtime-native-supersede-interrupted",
            setup: |s| {
                s.faults.push(Fault {
                    op: "native_run:native.linux-x64-docker".into(),
                    nth: 1,
                    effect: "crash".into(),
                })
            },
            calls: vec![
                call(Some("killed")),
                Call {
                    opts: |o| o.supersede = true,
                    ..call(None)
                },
            ],
            check: |_, s| {
                if count(s, "native-cancel native.linux-x64-docker") == 1 {
                    vec![]
                } else {
                    vec!["supersede discarded unfinished Docker ownership".into()]
                }
            },
        },
        Scenario {
            name: "runtime-native-duplicate-test",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        native_result(w, |v| {
                            let first = v["tests"][0].clone();
                            v["tests"].as_array_mut().unwrap().push(first);
                        })
                    },
                    ..call(Some("native-result-invalid"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-native-stale-kit",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        native_result(w, |v| v["kit_sha256"] = serde_json::json!("0".repeat(64)))
                    },
                    ..call(Some("native-result-invalid"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-native-stale-asset",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        native_result(w, |v| {
                            v["artifact_sha256"] = serde_json::json!("0".repeat(64))
                        })
                    },
                    ..call(Some("native-result-invalid"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-duplicate-step",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        sbx_result(w, |v| {
                            let first = v["steps"][0].clone();
                            v["steps"].as_array_mut().unwrap().push(first);
                        })
                    },
                    ..call(Some("sbx-smoke-invalid"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-stale-asset",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        sbx_result(w, |v| {
                            v["binding"]["asset_sha256"] = serde_json::json!("0".repeat(64))
                        })
                    },
                    ..call(Some("sbx-smoke-invalid"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-packet-tampered-before-seal",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        let p = w.join(format!(
                            "state/{VERSION}/sbx-smoke/sbx.macos-arm64/report.json"
                        ));
                        let mut v: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
                        v["cli_sha256"] = serde_json::json!("0".repeat(64));
                        std::fs::write(p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
                    },
                    ..call(Some("sbx-smoke-invalid"))
                },
            ],
            check: |w, s| {
                let mut e = halted(w, s);
                expect(
                    &mut e,
                    count(s, "sbx-smoke-run sbx.macos-arm64") == 1,
                    "seal-time SBX verification reran the check",
                );
                expect(
                    &mut e,
                    !s.calls.iter().any(|c| c.starts_with("draft-upload ")),
                    "tampered SBX packet reached draft upload",
                );
                e
            },
        },
        Scenario {
            name: "runtime-smokes-pass-and-resume",
            setup: freeze,
            calls: vec![call(Some("killed")), call(None)],
            check: once,
        },
        Scenario {
            name: "runtime-sbx-mac-only-with-saved-linux-host",
            setup: |s| {
                s.source
                    .config
                    .insert("awman.release.linuxHost".into(), "build@kvm1".into());
            },
            calls: vec![Call {
                opts: |o| o.linux_host = None,
                ..call(None)
            }],
            check: |_, s| {
                if count(s, "sbx-smoke-run sbx.macos-arm64") == 1
                    && count(s, "sbx-smoke-run sbx.linux-x64") == 0
                    && count(s, "native-run native.linux-x64-docker") == 1
                {
                    vec![]
                } else {
                    vec!["saved host selected Linux SBX or omitted Docker".into()]
                }
            },
        },
        Scenario {
            name: "runtime-sbx-linux-blocked",
            setup: |s| inject(s, "blocked", "sbx-not-signed-in"),
            calls: vec![
                call(Some("sbx-smoke-blocked")),
                Call {
                    before: |w| Dir { path: w.into() }.with(|s| s.script.sbx_smoke.clear()),
                    ..call(None)
                },
            ],
            check: |_, s| {
                if count(s, "sbx-smoke-run sbx.macos-arm64") == 1
                    && count(s, "sbx-smoke-run sbx.linux-x64") == 2
                {
                    vec![]
                } else {
                    vec!["blocked Linux SBX was dropped or Mac reran".into()]
                }
            },
        },
        Scenario {
            name: "runtime-sbx-linux-preflight-blocked",
            setup: |s| {
                s.script
                    .sbx_smoke_unavailable
                    .insert("sbx.linux-x64".into(), "sbx-not-signed-in".into());
            },
            calls: vec![call(Some("sbx-smoke-blocked"))],
            check: |_, s| {
                if count(s, "dispatch ") == 0 {
                    vec![]
                } else {
                    vec!["dispatched before selected SBX probe passed".into()]
                }
            },
        },
        Scenario {
            name: "runtime-sbx-failed-task",
            setup: |s| inject(s, "failed", "readiness"),
            calls: vec![
                call(Some("sbx-smoke-failed")),
                call(Some("sbx-smoke-failed")),
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-timeout",
            setup: |s| inject(s, "failed", "timeout"),
            calls: vec![
                call(Some("sbx-smoke-failed")),
                call(Some("sbx-smoke-failed")),
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-cleanup-failure",
            setup: |s| inject(s, "failed", "cleanup"),
            calls: vec![call(Some("sbx-smoke-failed"))],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-missing-step",
            setup: |s| {
                s.script.sbx_smoke.insert(
                    "sbx.linux-x64".into(),
                    NativeScript {
                        omit_test: true,
                        ..Default::default()
                    },
                );
            },
            calls: vec![call(Some("sbx-smoke-invalid"))],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-interrupted",
            setup: |s| {
                s.faults.push(Fault {
                    op: "sbx_smoke:sbx.linux-x64".into(),
                    nth: 1,
                    effect: "crash".into(),
                })
            },
            calls: vec![call(Some("killed")), call(None)],
            check: |_, s| {
                if count(s, "sbx-smoke-reconcile sbx.linux-x64") == 1
                    && count(s, "sbx-smoke-run sbx.macos-arm64") == 1
                {
                    vec![]
                } else {
                    vec!["interrupted SBX did not reconcile only the unfinished host".into()]
                }
            },
        },
        Scenario {
            name: "runtime-sbx-selection-substitution",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    opts: |o| o.linux_host = Some("build@other".into()),
                    ..call(Some("sbx-selection-changed"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-selection-drop-unfinished",
            setup: |s| inject(s, "blocked", "sbx-not-signed-in"),
            calls: vec![
                call(Some("sbx-smoke-blocked")),
                Call {
                    opts: |o| o.linux_host = None,
                    ..call(Some("sbx-selection-changed"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-selection-omit-complete",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    opts: |o| o.linux_host = None,
                    ..call(None)
                },
            ],
            check: once,
        },
        Scenario {
            name: "runtime-sbx-selection-add",
            setup: |s| {
                freeze(s);
                s.source
                    .config
                    .insert("awman.release.linuxHost".into(), "build@kvm1".into());
            },
            calls: vec![
                Call {
                    opts: |o| o.linux_host = None,
                    ..call(Some("killed"))
                },
                call(Some("sbx-selection-changed")),
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-tampered",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        std::fs::write(path(w, "sbx-smoke/sbx.linux-x64/raw.json"), "{}").unwrap();
                    },
                    ..call(Some("sbx-smoke-invalid"))
                },
            ],
            check: halted,
        },
        Scenario {
            name: "runtime-sbx-cli-changed",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        Dir { path: w.into() }
                            .with(|s| s.script.sbx_smoke_version = Some("0.54.2".into()))
                    },
                    ..call(None)
                },
            ],
            check: |_, s| {
                if count(s, "sbx-smoke-run sbx.macos-arm64") == 2
                    && count(s, "sbx-smoke-run sbx.linux-x64") == 2
                    && count(s, "native-run ") == 4
                {
                    vec![]
                } else {
                    vec!["CLI change did not rerun only SBX".into()]
                }
            },
        },
        Scenario {
            name: "runtime-apple-missing-test",
            setup: |s| {
                s.script
                    .native
                    .get_mut("native.macos-arm64-apple")
                    .unwrap()
                    .omit_test = true
            },
            calls: vec![call(Some("native-result-invalid"))],
            check: halted,
        },
        Scenario {
            name: "runtime-docker-blocked",
            setup: |s| {
                let n = s.script.native.get_mut("native.linux-x64-docker").unwrap();
                n.outcome = Some("blocked".into());
                n.reason_code = Some("docker-daemon-unavailable".into());
            },
            calls: vec![call(Some("native-blocked"))],
            check: halted,
        },
        Scenario {
            name: "runtime-docker-tampered",
            setup: freeze,
            calls: vec![
                call(Some("killed")),
                Call {
                    before: |w| {
                        std::fs::write(
                            path(w, "native/native.linux-x64-docker/runtime-smoke.json"),
                            "{}",
                        )
                        .unwrap();
                    },
                    ..call(Some("native-result-invalid"))
                },
            ],
            check: halted,
        },
    ]
}

fn native_result(w: &Path, change: fn(&mut serde_json::Value)) {
    let file = path(w, "native/native.linux-x64-docker/result.json");
    let mut v: serde_json::Value = crate::release::util::read_json(&file).unwrap();
    change(&mut v);
    crate::release::util::write_json(&file, &v).unwrap();
}
fn sbx_result(w: &Path, change: fn(&mut serde_json::Value)) {
    let file = path(w, "sbx-smoke/sbx.linux-x64/raw.json");
    let mut v: serde_json::Value = crate::release::util::read_json(&file).unwrap();
    change(&mut v);
    crate::release::util::write_json(&file, &v).unwrap();
}
