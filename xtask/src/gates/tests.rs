use super::*;
use crate::manifest::{self, Manifest};
use chrono::NaiveDate;
use std::collections::BTreeMap;
pub fn manifest() -> Manifest {
    manifest::parse(include_bytes!("../../../ci/gates.toml")).unwrap()
}
pub fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
}
pub fn request(event: &str, paths: &[&str]) -> plan::Request {
    plan::Request {
        repository_id: "42".into(),
        event: event.into(),
        head_sha: "a".repeat(40),
        base_sha: Some("b".repeat(40)),
        paths: paths.iter().map(|s| s.to_string()).collect(),
        diff_complete: true,
        admitted_platforms: Default::default(),
        suite_durations_ms: Default::default(),
        suite_test_counts: Default::default(),
        main_durations_ms: Default::default(),
    }
}
fn error(m: &Manifest) -> String {
    manifest::validate::validate(m, today())
        .unwrap_err()
        .to_string()
}
#[test]
fn target_manifest_is_structurally_valid() {
    manifest::validate::validate(&manifest(), today()).unwrap();
}
#[test]
fn unknown_keys_and_schema_fail() {
    let text = String::from_utf8(include_bytes!("../../../ci/gates.toml").to_vec()).unwrap();
    assert!(manifest::parse(format!("unknown = 1\n{text}").as_bytes()).is_err());
    assert!(
        manifest::parse(text.replacen("schema = 1", "schema = 2", 1).as_bytes())
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
}
#[test]
fn manifest_version_is_exact_bytes() {
    let a = include_bytes!("../../../ci/gates.toml");
    let mut b = a.to_vec();
    b.push(b'\n');
    assert_ne!(
        manifest::parse(a).unwrap().version(),
        manifest::parse(&b).unwrap().version()
    );
}
#[test]
fn removed_required_suite_is_red() {
    let mut m = manifest();
    m.suites.remove("component-engine");
    assert!(error(&m).contains("removed required suite component-engine"));
}
#[test]
fn removed_gate_membership_is_red() {
    let mut m = manifest();
    for c in &mut m.gates.get_mut("pr").unwrap().cells {
        c.suites.retain(|s| s != "component-engine");
    }
    assert!(error(&m).contains("required"));
}
#[test]
fn dropped_release_critical_id_is_red() {
    let mut m = manifest();
    m.release_critical
        .retain(|s| s.id != "guest.actual-awman-boot");
    assert!(error(&m).contains("dropped release-critical scenario ID"));
}
#[test]
fn removed_runner_capability_is_red() {
    let mut m = manifest();
    m.platforms
        .get_mut("linux-x64")
        .unwrap()
        .provides
        .retain(|s| s != "docker");
    assert!(error(&m).contains("required capability docker removed"));
}
#[test]
fn removed_tier_capability_is_red() {
    let mut m = manifest();
    m.tiers
        .get_mut("guest")
        .unwrap()
        .guarantees
        .retain(|s| s != "hypervisor");
    assert!(error(&m).contains("capability hypervisor removed from tier"));
}
#[test]
fn expired_quarantine_is_red_in_plan() {
    let mut m = manifest();
    m.quarantine.push(manifest::Quarantine {
        test: "case".into(),
        owner: "@owner".into(),
        issue: "https://example.org/1".into(),
        expires: "2026-11-11".into(),
    });
    let e = plan::create(
        &m,
        "pr",
        request("pull_request", &["src/data/config.rs"]),
        today(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("expired quarantine"));
}
#[test]
fn future_quarantine_does_not_deselect_test() {
    let mut m = manifest();
    m.quarantine.push(manifest::Quarantine {
        test: "case".into(),
        owner: "@owner".into(),
        issue: "https://example.org/1".into(),
        expires: "2026-12-01".into(),
    });
    let p = plan::create(&m, "main", request("push", &[]), today()).unwrap();
    assert_eq!(p.quarantine, vec!["case"]);
    assert!(p.cells.iter().all(|c| c.selected));
}
#[test]
fn required_hardware_override_is_red() {
    let mut m = manifest();
    m.overrides.push(manifest::Override {
        gate: "candidate".into(),
        cell: "guest-boot@macos-arm64-release-builtin".into(),
        reason: "unavailable".into(),
        expires: "2026-12-01".into(),
        approved_by: "@owner".into(),
    });
    assert!(error(&m).contains("override forbidden"));
}
#[test]
fn unknown_and_shared_paths_force_full_run() {
    for path in [
        "README.md",
        "new/unknown.rs",
        "Cargo.toml",
        "crates/awman-test-support/src/lib.rs",
        "xtask/src/main.rs",
        "tests/component/support/mod.rs",
    ] {
        let p = plan::create(&manifest(), "pr", request("pull_request", &[path]), today()).unwrap();
        assert!(p.cells.iter().all(|c| c.selected), "{path}");
        assert!(p.cells.iter().any(|c| c.reason.contains("fallback")));
    }
}
#[test]
fn known_path_selects_dependencies_and_keeps_policy_suites() {
    let mut m = manifest();
    for s in m.suites.values_mut() {
        s.always = false;
    }
    let p = plan::create(
        &m,
        "pr",
        request("pull_request", &["src/data/config.rs"]),
        today(),
    )
    .unwrap();
    assert!(p
        .cells
        .iter()
        .filter(|c| m
            .suites
            .get(&c.cell.suite)
            .is_some_and(|s| s.tier == "unit" || c.cell.suite.starts_with("component-parity-")))
        .all(|c| c.selected));
    assert!(p
        .cells
        .iter()
        .filter(|c| c.cell.suite == "component-data")
        .all(|c| c.selected));
    assert!(
        p.cells.iter().all(|c| c.selected),
        "source changes select all dependent suites"
    );
}
#[test]
fn merge_queue_main_and_empty_diff_are_full() {
    for (gate, event) in [
        ("pr", "merge_group"),
        ("main", "push"),
        ("pr", "pull_request"),
    ] {
        let p = plan::create(&manifest(), gate, request(event, &[]), today()).unwrap();
        assert!(p.cells.iter().all(|c| c.selected));
    }
}
#[test]
fn incomplete_diff_and_missing_base_fail() {
    let mut r = request("pull_request", &[]);
    r.diff_complete = false;
    assert!(plan::create(&manifest(), "pr", r.clone(), today())
        .unwrap_err()
        .to_string()
        .contains("truncated"));
    r.diff_complete = true;
    r.base_sha = None;
    assert!(plan::create(&manifest(), "pr", r, today())
        .unwrap_err()
        .to_string()
        .contains("base SHA"));
}
#[test]
fn malformed_paths_fail() {
    assert!(plan::create(
        &manifest(),
        "pr",
        request("pull_request", &["../secret"]),
        today()
    )
    .is_err());
}
#[test]
fn glob_segments_are_explicit() {
    assert!(selection::matches("src/**", "src/a/b.rs"));
    assert!(!selection::matches("src/*", "src/a/b.rs"));
    assert!(selection::matches("crates/*/build.rs", "crates/a/build.rs"));
    assert!(selection::matches(
        "tests/**/support/**",
        "tests/support/x.rs"
    ));
}
#[test]
fn inheritance_cycle_and_self_hosted_pr_fail() {
    let mut m = manifest();
    m.gates.get_mut("pr").unwrap().includes.push("main".into());
    assert!(error(&m).contains("cycle"));
    let mut m = manifest();
    m.platforms.get_mut("linux-x64").unwrap().trust = "release".into();
    assert!(error(&m).contains("cannot schedule self-hosted"));
}
#[test]
fn renamed_job_missing_timeout_and_unpinned_action_are_red() {
    let m = manifest();
    let w = &m.workflows[".github/workflows/ci.yml"];
    let text = "jobs:\n  plan-renamed:\n    steps:\n      - uses: actions/checkout@v4\n";
    let errors = render::workflows::verify_text("ci.yml", text, w).join("\n");
    assert!(errors.contains("missing or renamed required job plan"));
    assert!(errors.contains("timeout-minutes"));
    assert!(errors.contains("not pinned"));
}
#[test]
fn make_target_drift_is_red() {
    assert!(check::make_findings("check:\n\tcargo test\n")
        .iter()
        .any(|s| s.contains("check must invoke cargo xtask lint all")));
    assert!(check::make_findings(include_str!("../../../Makefile")).is_empty());
}
#[test]
fn rendered_outputs_are_deterministic_and_zero_retry() {
    let m = manifest();
    assert_eq!(render::outputs(&m).unwrap(), render::outputs(&m).unwrap());
    let text = render::nextest::render(&m).unwrap();
    assert!(text.contains("retries = 0"));
    toml::from_str::<toml::Value>(&text).unwrap();
    assert!(render::docs::render(&m).contains("gates:begin contract"));
}
#[test]
fn pending_pins_are_clear_and_real_nextest_hashes_valid() {
    let m = manifest();
    let errors = manifest::validate::pins(&m);
    assert!(errors.iter().any(|s| s.contains("previous release")));
    assert!(!errors.iter().any(|s| s.contains("tool nextest:")));
}
#[test]
fn unregistered_test_file_is_red() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();
    std::fs::create_dir_all(dir.path().join("crates")).unwrap();
    std::fs::write(
        dir.path().join("tests/unregistered.rs"),
        "#[test] fn lost() {}\n",
    )
    .unwrap();
    let meta = discovery::Metadata {
        packages: vec![],
        workspace_members: vec![],
    };
    let d = discovery::audit(dir.path(), &manifest(), &meta).unwrap();
    assert!(d
        .findings
        .iter()
        .any(|f| f.contains("unregistered test file") && f.contains("unregistered.rs")));
}
#[test]
fn module_walk_includes_host_cfg_hidden_modules() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "#[cfg(windows)] mod hidden;\n").unwrap();
    std::fs::write(
        dir.path().join("hidden.rs"),
        "#[test] fn windows_contract() { assert!(true); }\n",
    )
    .unwrap();
    let d = discovery::walk(&dir.path().join("main.rs"), "pkg", "target").unwrap();
    assert_eq!(d.reachable.len(), 2);
    assert!(d.tests.iter().any(|t| t.contains("windows_contract")));
}
#[test]
fn unsupported_include_fails_discovery() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/tests.rs\"));",
    )
    .unwrap();
    let d = discovery::walk(&dir.path().join("main.rs"), "pkg", "target").unwrap();
    assert!(d.findings.iter().any(|f| f.contains("unsupported include")));
}
#[test]
fn compiled_partition_detects_empty_missing_and_overlap() {
    let tests = vec!["a".into(), "b".into()];
    let mut suites = BTreeMap::from([("one".into(), vec!["a".into()])]);
    assert!(discovery::partition(&tests, &suites)
        .unwrap_err()
        .to_string()
        .contains("0 suites"));
    suites.insert("two".into(), tests.clone());
    assert!(discovery::partition(&tests, &suites)
        .unwrap_err()
        .to_string()
        .contains("2 suites"));
    suites.insert("two".into(), vec!["b".into()]);
    discovery::partition(&tests, &suites).unwrap();
}
#[test]
fn early_return_and_quiet_conditional_skips_are_red() {
    for body in [
        "if !available() { return; } assert!(true);",
        "if available() { assert!(works()); }",
        "if let Ok(value) = operation() { assert!(value); }",
        "operation().ok();",
    ] {
        let f: syn::ItemFn = syn::parse_str(&format!("#[test] fn case() {{ {body} }}")).unwrap();
        assert!(
            crate::lint::hygiene::test_findings(&f)
                .iter()
                .any(|f| f.contains("H5")),
            "{body}"
        );
    }
}
#[test]
fn renamed_helper_skips_are_red() {
    let source = "fn helper() { if !available() { return; } assert!(true); } use helper as renamed; #[test] fn contract() { renamed(); }";
    assert!(
        crate::lint::hygiene::helper_findings(&syn::parse_file(source).unwrap().items, true)
            .iter()
            .any(|s| s.contains("helper"))
    );
}
#[test]
fn measured_shards_and_recovery_history_are_bound_to_plan() {
    let m = manifest();
    let mut r = request("push", &[]);
    r.suite_durations_ms
        .insert("component-data".into(), 250_000);
    r.suite_test_counts.insert("component-data".into(), 20);
    let p = plan::create(&m, "main", r.clone(), today()).unwrap();
    assert!(p
        .cells
        .iter()
        .filter(|c| c.cell.suite == "component-data")
        .all(|c| c.shards == 3));
    assert!(p.tests.iter().any(|c| c.shards == 3));
    r.main_durations_ms = vec![1_200_001; 3];
    let recovery = plan::create(&m, "main", r.clone(), today()).unwrap();
    assert_eq!(recovery.request.main_durations_ms, r.main_durations_ms);
    r.event = "pull_request".into();
    assert!(plan::create(&m, "pr", r, today()).is_ok());
}
#[test]
fn hardware_without_admission_emits_hosted_blocked_rows() {
    let m = manifest();
    let p = plan::create(&m, "main", request("push", &[]), today()).unwrap();
    let rows: Vec<_> = p.tests.iter().filter(|r| r.tier == "guest").collect();
    assert!(!rows.is_empty());
    assert!(rows
        .iter()
        .all(|r| !r.self_hosted && r.blocked_reason.is_some()));
    assert!(
        plan::create(&m, "main", request("pull_request", &[]), today())
            .unwrap_err()
            .to_string()
            .contains("hosted PR gate")
    );
}
#[test]
fn relative_path_modules_resolve_from_source_directory() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("shared")).unwrap();
    std::fs::write(d.path().join("main.rs"), "mod domain;").unwrap();
    std::fs::write(
        d.path().join("domain.rs"),
        "#[path = \"shared/cases.rs\"] mod cases;",
    )
    .unwrap();
    std::fs::write(
        d.path().join("shared/cases.rs"),
        "#[test] fn contract() { assert!(true); }",
    )
    .unwrap();
    let discovered = discovery::walk(&d.path().join("main.rs"), "p", "t").unwrap();
    assert!(discovered.tests.contains("p::t domain::cases::contract"));
}
#[test]
fn manifest_cannot_erase_job_or_tier_promises() {
    let mut m = manifest();
    m.workflows
        .get_mut(".github/workflows/ci.yml")
        .unwrap()
        .jobs
        .retain(|j| j != "plan");
    assert!(error(&m).contains("removed required workflow job expectation plan"));
    let mut m = manifest();
    m.tiers
        .get_mut("component")
        .unwrap()
        .guarantees
        .retain(|c| c != "loopback");
    assert!(error(&m).contains("capability loopback removed from tier component"));
}

#[test]
fn scenario_test_removal_changes_the_independent_contract() {
    let mut m = manifest();
    assert!(check::scenario_findings(&root(), &m).is_empty());
    m.release_critical[0].tests.pop();
    assert!(!check::scenario_findings(&root(), &m).is_empty());
}

#[test]
fn legacy_publisher_is_dispatch_only_and_guarded_by_explicit_activation() {
    let workflow = std::fs::read_to_string(root().join(".github/workflows/release.yml")).unwrap();
    assert!(!workflow.contains("  push:"));
    assert!(workflow.contains("workflow_dispatch:"));
    assert!(workflow.contains("AWMAN_LEGACY_RELEASE_ENABLED"));
    assert!(workflow.contains("github.ref == 'refs/heads/main'"));
    assert!(workflow.contains("git merge-base --is-ancestor HEAD origin/main"));
}

#[test]
fn step_timeouts_cannot_satisfy_a_missing_job_timeout() {
    let w = crate::manifest::Workflow {
        jobs: vec!["test".into()],
        commands: vec![],
        timeout_minutes: 60,
    };
    let errors = render::workflows::verify_text(
        "w.yml",
        "jobs:\n  test:\n    steps:\n      - run: true\n        timeout-minutes: 5\n",
        &w,
    );
    assert!(errors.iter().any(|e| e.contains("missing timeout-minutes")));
}

#[test]
fn all_source_layers_and_root_select_their_dependents() {
    for path in [
        "src/lib.rs",
        "src/data/config/env.rs",
        "src/engine/git/mod.rs",
        "src/command/dispatch/mod.rs",
        "src/frontend/api/mod.rs",
    ] {
        let p = plan::create(&manifest(), "pr", request("pull_request", &[path]), today()).unwrap();
        assert!(
            p.cells.iter().all(|c| c.selected),
            "dependent suites missing for {path}"
        );
    }
}

#[test]
fn fixtures_workflow_supplies_the_verified_base_recipe_inputs() {
    let workflow = std::fs::read_to_string(root().join(".github/workflows/fixtures.yml")).unwrap();
    assert!(workflow.contains("fixtures publish-plan"));
    assert!(workflow.contains(
        "fixtures build guest-oci-base --base base.tar --sha256 \"$BASE_SHA256\" --out built"
    ));
    assert!(workflow.contains("github.ref == 'refs/heads/main'"));
    assert!(workflow.contains("Refusing to replace an existing fixture release"));
}

#[test]
fn empty_prefix_suite_is_detected_without_a_compiled_inventory() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("component.rs");
    std::fs::write(&source, "#[test] fn unrelated() { assert!(true); }").unwrap();
    let mut m = manifest();
    m.suites.retain(|id, _| id == "component-parity-builtin");
    let metadata = discovery::Metadata {
        workspace_members: vec!["fixture".into()],
        packages: vec![discovery::Package {
            id: "fixture".into(),
            name: "awman-cli".into(),
            targets: vec![discovery::Target {
                name: "component".into(),
                kind: vec!["test".into()],
                src_path: source.clone(),
                required_features: vec![],
                test: true,
            }],
        }],
    };
    let missing = discovery::audit(root.path(), &m, &metadata).unwrap();
    assert!(missing
        .findings
        .iter()
        .any(|f| f.contains("empty required suite component-parity-builtin")));
    std::fs::write(
        source,
        "mod parity { mod builtin { #[test] fn actual() { assert!(true); } } }",
    )
    .unwrap();
    let present = discovery::audit(root.path(), &m, &metadata).unwrap();
    assert!(!present
        .findings
        .iter()
        .any(|f| f.contains("empty required suite component-parity-builtin")));
}

#[test]
fn sbx_nightly_requires_a_provisioned_native_runner_and_admission() {
    let m = manifest();
    let sbx = &m.platforms["macos-arm64-sbx"];
    assert_eq!(sbx.trust, "ci-guest");
    assert!(sbx.runs_on.contains(&"self-hosted".into()));
    let p = plan::create(&m, "nightly", request("schedule", &[]), today()).unwrap();
    let job = p
        .tests
        .iter()
        .find(|job| job.platform == "macos-arm64-sbx")
        .unwrap();
    assert!(job.blocked_reason.as_deref().unwrap().contains("admission"));
}
fn fake_checkout(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("ci")).unwrap();
    std::fs::create_dir_all(dir.join("xtask/src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(dir.join("ci/gates.toml"), "").unwrap();
}
#[test]
fn root_prefers_runtime_manifest_dir_over_a_stale_build_path() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("host-worktree");
    fake_checkout(&checkout);
    let stale = dir.path().join("workspace/xtask");
    let found = resolve_root(Some(checkout.join("xtask")), None, &stale);
    assert_eq!(found, checkout);
}
#[test]
fn root_falls_back_to_the_nearest_checkout_above_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("repo");
    fake_checkout(&checkout);
    let stale = dir.path().join("gone/xtask");
    let found = resolve_root(
        Some(dir.path().join("not-a-checkout/xtask")),
        Some(checkout.join("xtask/src")),
        &stale,
    );
    assert_eq!(found, checkout);
}
#[test]
fn root_uses_the_build_path_only_when_nothing_else_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let compiled = dir.path().join("built/xtask");
    let found = resolve_root(None, Some(dir.path().to_path_buf()), &compiled);
    assert_eq!(found, dir.path().join("built"));
}
#[test]
fn root_resolves_this_checkout() {
    let found = root();
    assert!(found.join("ci/gates.toml").is_file(), "{}", found.display());
}
