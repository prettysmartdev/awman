use super::*;
use serde_yaml::Value;

fn yaml(rel: &str) -> Value {
    serde_yaml::from_str(&read(rel)).unwrap()
}

fn dispatch_inputs(doc: &Value) -> Vec<String> {
    doc["on"]["workflow_dispatch"]["inputs"]
        .as_mapping()
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn release_workflow_accepts_exactly_the_inputs_the_command_dispatches() {
    let doc = yaml(".github/workflows/release.yml");
    let mut inputs = dispatch_inputs(&doc);
    inputs.sort();
    // xtask/src/release/phases/hosted.rs `inputs()` sends exactly these.
    assert_eq!(inputs, ["asset", "commit", "key", "version"]);
    let run_name = doc["run-name"].as_str().unwrap();
    assert!(
        run_name.contains("${{ inputs.key }}"),
        "the key makes a run discoverable"
    );
    let ci = yaml(".github/workflows/ci.yml");
    let ci_inputs = dispatch_inputs(&ci);
    for i in ["gate", "key"] {
        assert!(
            ci_inputs.iter().any(|x| x == i),
            "ci.yml workflow_dispatch lacks {i}"
        );
    }
    assert!(ci["run-name"].as_str().unwrap().contains("inputs.key"));
    let options =
        serde_yaml::to_string(&ci["on"]["workflow_dispatch"]["inputs"]["gate"]["options"]).unwrap();
    assert!(
        options.contains("release"),
        "ci.yml cannot be dispatched for the release gate"
    );
}

#[test]
fn partial_production_checkpoints_never_rebuild_retained_bytes() {
    let doc = yaml(".github/workflows/release.yml");
    let steps = doc["jobs"]["produce"]["steps"].as_sequence().unwrap();
    for (name, key) in [
        ("Build once (cache-free) and sign once", "reuse_asset"),
        ("Attest the shipping bytes", "reuse_asset"),
        ("Checkpoint the shipping bytes", "reuse_asset"),
        (
            "Native test kit (selected artifact target only)",
            "reuse_kit",
        ),
        ("Attest the native kit", "reuse_kit"),
        ("Checkpoint the native kit", "reuse_kit"),
    ] {
        let step = steps.iter().find(|s| s["name"] == name).unwrap();
        let condition = step["if"].as_str().unwrap();
        assert!(
            condition.contains(&format!("steps.checkpoint.outputs.{key} != 'true'")),
            "{name}: {condition}"
        );
    }
}

#[test]
fn check_workflows_accepts_the_release_workflow_and_rejects_a_self_hosted_or_nested_one() {
    let s = sandbox("release_contract::check_workflows");
    let check = |text: &str| {
        let dir = s.work().join("repo");
        let wf = dir.join(".github/workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("release.yml"), text).unwrap();
        s.command(awman_test_support::cargo_bin!("xtask"))
            .args(["release", "check-workflows", "--root"])
            .arg(&dir)
            .run()
    };
    let good = read(".github/workflows/release.yml");
    check(&good).assert_success();
    for bad in [
        good.replace("'ubuntu-22.04' }}", "'self-hosted' }}"),
        good.replace(
            "    timeout-minutes: 45\n",
            "    timeout-minutes: 45\n    environment: release\n",
        ),
        good.replace(
            "permissions:\n  contents: read\n\nenv:",
            "permissions:\n  contents: write\n\nenv:",
        ),
        good.replace("      asset:\n", "      target:\n"),
    ] {
        assert_ne!(bad, good, "mutation did not apply");
        let out = check(&bad);
        assert!(
            !out.exit.success(),
            "accepted:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn fixture_publication_is_prepared_infrastructure_with_one_pin_per_architecture() {
    let text = read(".github/workflows/fixtures.yml");
    assert!(
        !text.contains("refs/heads/release/v"),
        "fixtures must be dispatchable from any branch"
    );
    let doc: Value = serde_yaml::from_str(&text).unwrap();
    let arches =
        serde_yaml::to_string(&doc["jobs"]["build"]["strategy"]["matrix"]["arch"]).unwrap();
    assert!(arches.contains("arm64") && arches.contains("amd64"));
    assert!(text.contains("docker pull --platform \"linux/$ARCH\""));
    assert!(text.contains("fixtures verify-build --base base.tar"));
    assert!(text.contains("fixture-oci-amd64.tar"));
    assert!(text.contains("Refusing to replace an existing fixture release"));
    assert!(text.contains("--prerelease --latest=false"));
    assert!(text.contains("releases/latest"));
    assert!(!text.contains("--clobber"));
    // Publication alone holds contents: write; the builders attest.
    assert_eq!(doc["jobs"]["publish"]["permissions"]["contents"], "write");
    assert_eq!(doc["jobs"]["build"]["permissions"]["contents"], "read");
}

#[test]
fn no_workflow_on_the_release_path_uses_self_hosted_runners_environments_or_secrets() {
    for wf in [
        ".github/workflows/release.yml",
        ".github/workflows/fixtures.yml",
    ] {
        let text = read(wf);
        assert!(
            !text
                .lines()
                .any(|l| l.contains("runs-on") && l.contains("self-hosted")),
            "{wf}"
        );
        assert!(!text.contains("environment:"), "{wf}");
        assert!(!text.contains("secrets."), "{wf}");
        assert!(!text.contains("uses: ./"), "{wf}");
    }
}

#[test]
fn the_actual_gate_plan_is_consumed_by_the_release_command() {
    let s = sandbox("release_contract::actual_plan");
    let out = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["gates", "release-plan", "--root"])
        .arg(root())
        .run();
    out.assert_success();
    assert_eq!(out.stdout.last(), Some(&b'\n'));
    let mut plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // Publication of the amd64 fixture is a rollout prerequisite. First prove
    // pending pins are understood (rather than rejected as a schema mismatch).
    let path = s.work().join("plan.json");
    std::fs::write(&path, &out.stdout).unwrap();
    let pending = plan["native"].as_array().unwrap().iter().any(|n| {
        n["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["sha256"].is_null())
    });
    if pending {
        let out = s
            .command(awman_test_support::cargo_bin!("xtask"))
            .args([
                "release",
                "produce-native",
                "--asset",
                "awman-linux-amd64",
                "--plan",
            ])
            .arg(&path)
            .run();
        assert!(!out.exit.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("pin-pending"));
    }
    // Supply only fixture transport pins in this component world; preserve the
    // actual producer's policy, assets, target/feature and native selections.
    for n in plan["native"].as_array_mut().unwrap() {
        for f in n["fixtures"].as_array_mut().unwrap() {
            if f["sha256"].is_null() {
                f["sha256"] = serde_json::json!("a".repeat(64));
            }
            if f["url"].is_null() {
                f["url"] = serde_json::json!("https://fixtures.invalid/test");
            }
        }
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    for asset in plan["assets"].as_array().unwrap() {
        let out = s
            .command(awman_test_support::cargo_bin!("xtask"))
            .args([
                "release",
                "produce-native",
                "--asset",
                asset["asset"].as_str().unwrap(),
                "--plan",
            ])
            .arg(&path)
            .run();
        out.assert_success();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            asset["native"].as_str().unwrap_or("")
        );
    }
}

#[test]
fn experimental_sbx_does_not_add_a_provider_kit_or_credentials_to_release() {
    let doc = yaml(".github/workflows/release.yml");
    let steps = doc["jobs"]["produce"]["steps"].as_sequence().unwrap();
    for name in [
        "Native test kit native.macos-arm64-sbx",
        "Attest native kit native.macos-arm64-sbx",
        "Checkpoint native kit native.macos-arm64-sbx",
    ] {
        assert!(
            steps.iter().all(|s| s["name"] != name),
            "optional SBX diagnostic appeared on the release critical path: {name}"
        );
    }
    let text = read(".github/workflows/release.yml");
    assert!(!text.contains("test kit-run"));
    assert!(!text.contains("AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE"));
}
