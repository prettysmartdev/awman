//! The opt-in builtin gate is wired the way the Docker/Apple gates are: off in
//! `make test`/`test-fast`/`test-full`, on only in `make test-builtin`, with
//! `builtin_hw_*` tests skipped by name in the fast tier.

use std::path::Path;
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

/// The recipe lines of a Makefile target.
fn recipe(makefile: &str, target: &str) -> String {
    let mut lines = makefile.lines();
    let mut out = String::new();
    while let Some(line) = lines.next() {
        if line.starts_with(&format!("{target}:")) {
            for recipe in lines.by_ref() {
                if recipe.starts_with('\t') {
                    out.push_str(recipe);
                    out.push('\n');
                } else {
                    break;
                }
            }
            break;
        }
    }
    assert!(!out.is_empty(), "Makefile target {target} not found");
    out
}

#[test]
fn builtin_makefile_gate_is_opt_in_only_through_test_builtin() {
    let makefile = read("Makefile");
    for target in ["test", "test-fast", "test-full"] {
        assert!(
            !recipe(&makefile, target).contains("AWMAN_TEST_BUILTIN"),
            "`make {target}` must never opt into the builtin runtime"
        );
    }
    let fast = recipe(&makefile, "test-fast");
    assert!(
        fast.contains("--skip builtin_hw"),
        "fast tests must skip real-guest tests by name"
    );
    let builtin = recipe(&makefile, "test-builtin");
    assert!(builtin.contains("AWMAN_TEST_BUILTIN=1"));
    assert!(builtin.contains("--features builtin-runtime"));
    assert!(
        builtin.contains("tools/isolated-test.sh"),
        "builtin tests still run isolated"
    );
}

#[test]
fn builtin_isolated_script_documents_and_conditions_the_gate() {
    let script = read("tools/isolated-test.sh");
    assert!(script.contains("AWMAN_TEST_BUILTIN"));
    assert!(
        !script.contains("export AWMAN_TEST_BUILTIN"),
        "the gate is the caller's choice"
    );
    for variable in [
        "MSB_PATH",
        "MSB_LIBKRUNFW_PATH",
        "MSB_AGENTD_PATH",
        "MSB_HOME",
        "MSB_BACKEND",
    ] {
        assert!(
            script.contains(variable),
            "{variable} must be cleared from the test environment"
        );
    }
    let assignments: Vec<&str> = script
        .lines()
        .filter(|l| l.contains("export AWMAN_BUILTIN_STATE_DIR"))
        .collect();
    assert_eq!(
        assignments.len(),
        1,
        "only the gated branch may set the state root"
    );
}

/// Run the *real* script with a stub `cargo` that reports the environment it
/// would test in.
fn run_isolated(extra_env: &[(&str, &str)]) -> (String, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("awb-gate.")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let stub = bin.join("cargo");
    std::fs::write(
        &stub,
        "#!/bin/sh\nfor v in AWMAN_TEST_ISOLATION AWMAN_TEST_BUILTIN AWMAN_BUILTIN_STATE_DIR MSB_PATH MSB_HOME HOME HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY http_proxy https_proxy all_proxy no_proxy DOCKER_CONFIG CODEX_HOME; do\n  eval \"value=\\${$v-<unset>}\"; printf '%s=%s\\n' \"$v\" \"$value\"\ndone\nif [ -n \"${AWMAN_BUILTIN_STATE_DIR:-}\" ]; then ls -ld \"$AWMAN_BUILTIN_STATE_DIR\" | cut -c1-10; fi\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let real_path = std::env::var("PATH").unwrap_or_default();
    let mut command = Command::new("bash");
    command
        .arg(root().join("tools/isolated-test.sh"))
        .env_clear()
        .env("PATH", format!("{}:{real_path}", bin.display()))
        .env("HOME", dir.path().join("real-home"))
        .env("AWMAN_TEST_TMPROOT", dir.path().join("fixtures"))
        .env("MSB_PATH", "/developer/msb")
        .env("MSB_HOME", "/developer/msb-home");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().expect("run tools/isolated-test.sh");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (String::from_utf8_lossy(&output.stdout).into_owned(), dir)
}

#[test]
fn builtin_isolated_run_leaves_the_builtin_runtime_unavailable_by_default() {
    let (seen, dir) = run_isolated(&[]);
    assert!(seen.contains("AWMAN_TEST_ISOLATION=1"), "{seen}");
    assert!(seen.contains("AWMAN_TEST_BUILTIN=<unset>"), "{seen}");
    assert!(seen.contains("AWMAN_BUILTIN_STATE_DIR=<unset>"), "{seen}");
    assert!(
        seen.contains("MSB_PATH=<unset>") && seen.contains("MSB_HOME=<unset>"),
        "developer MSB_* must not leak: {seen}"
    );
    assert!(!seen.contains("real-home"), "HOME is a throwaway: {seen}");
    drop(dir);
}

#[test]
fn builtin_isolated_run_with_the_gate_gets_a_short_private_state_root_and_cleans_it_up() {
    let (seen, _dir) = run_isolated(&[("AWMAN_TEST_BUILTIN", "1")]);
    assert!(seen.contains("AWMAN_TEST_BUILTIN=1"), "{seen}");
    let state = seen
        .lines()
        .find_map(|l| l.strip_prefix("AWMAN_BUILTIN_STATE_DIR="))
        .expect("state dir");
    assert!(state.starts_with("/tmp/awman-b."), "{state}");
    // `<root>/run/agent/<32 hex>.control.sock` is 56 bytes; it must fit in 103.
    assert!(
        state.len() + 56 <= 103,
        "{state} leaves no room for the control socket"
    );
    assert!(
        seen.contains("drwx------"),
        "the state root must be private (0700): {seen}"
    );
    assert!(
        !Path::new(state).exists(),
        "the script removes its state root on exit"
    );
    assert!(seen.contains("MSB_PATH=<unset>"), "{seen}");
}

#[test]
fn builtin_isolated_run_normalizes_a_falsy_gate_to_unset() {
    let (seen, _dir) = run_isolated(&[("AWMAN_TEST_BUILTIN", "0")]);
    assert!(seen.contains("AWMAN_TEST_BUILTIN=<unset>"), "{seen}");
    assert!(seen.contains("AWMAN_BUILTIN_STATE_DIR=<unset>"), "{seen}");
}

#[test]
fn builtin_isolated_run_scrubs_proxy_and_credential_directories() {
    let names = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "DOCKER_CONFIG",
        "CODEX_HOME",
    ];
    let inputs: Vec<_> = names
        .iter()
        .map(|name| (*name, "sentinel-not-a-real-service"))
        .collect();
    let (seen, _dir) = run_isolated(&inputs);
    for name in names {
        assert!(seen.contains(&format!("{name}=<unset>")), "{seen}");
    }
}

#[test]
fn builtin_isolated_run_preserves_proxy_only_for_explicit_registry_services() {
    let proxy = "http://127.0.0.1:9";
    for gates in [
        vec![],
        vec![("AWMAN_TEST_IMAGE_STORES", "1")],
        vec![("AWMAN_TEST_REGISTRY", "1")],
    ] {
        let mut inputs = gates;
        inputs.push(("HTTP_PROXY", proxy));
        let (seen, _dir) = run_isolated(&inputs);
        assert!(seen.contains("HTTP_PROXY=<unset>"), "{seen}");
    }
    let (seen, _dir) = run_isolated(&[
        ("AWMAN_TEST_IMAGE_STORES", "1"),
        ("AWMAN_TEST_REGISTRY", "1"),
        ("HTTP_PROXY", proxy),
    ]);
    assert!(seen.contains(&format!("HTTP_PROXY={proxy}")), "{seen}");
}

#[test]
fn builtin_documented_network_example_validates() {
    use awman::data::config::builtin_network::BuiltinNetworkConfig;
    let docs = read("docs/11-runtimes.md");
    let network = docs.split("### Network").nth(1).unwrap();
    let json = network
        .split("```json\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    let config: BuiltinNetworkConfig =
        serde_json::from_value(value["builtin"]["network"].clone()).unwrap();
    BuiltinNetworkConfig::resolve(Some(&config), None).unwrap();
}

#[test]
fn builtin_hardware_tests_are_named_so_the_fast_tier_skips_them() {
    let dir = root().join("tests/builtin_runtime");
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).unwrap();
        let mut lines = source.lines().peekable();
        while let Some(line) = lines.next() {
            if line.trim() != "#[test]" {
                continue;
            }
            let Some(name) = lines
                .peek()
                .and_then(|next| next.trim().strip_prefix("fn "))
                .and_then(|rest| rest.split('(').next())
            else {
                continue;
            };
            let hardware = name.starts_with("builtin_hw_");
            if file == "hardware.rs" {
                assert!(
                    hardware,
                    "{file}: {name}: guest tests must use builtin_hw_*"
                );
            }
            if hardware {
                let module = file.trim_end_matches(".rs");
                assert!(
                    read("tests/builtin_runtime/main.rs").contains(&format!("mod {module};")),
                    "{file}: guest module is not registered"
                );
                let head = lines.clone().take(3).collect::<Vec<_>>().join("\n");
                assert!(
                    head.contains("scenario(")
                        || head.contains("gate(")
                        || head.contains("hardware_or_skip("),
                    "{file}: {name}: guest test must start with its prerequisite gate: {head}"
                );
            }
        }
    }
    // ... and each of them starts by checking the hardware prerequisites.
    let hardware = read("tests/builtin_runtime/hardware.rs");
    let bodies: Vec<&str> = hardware.split("fn builtin_hw_").skip(1).collect();
    assert!(bodies.len() >= 10, "{} hardware tests", bodies.len());
    for body in bodies {
        let name = body.split('(').next().unwrap();
        let head = body.lines().take(3).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("scenario("),
            "builtin_hw_{name} must start with the gate: {head}"
        );
    }
}

#[test]
fn builtin_ci_runs_the_hermetic_builtin_tier_and_has_explicit_hardware_jobs() {
    let workflow = read(".github/workflows/test.yml");
    assert!(workflow.contains("make test-builtin"));
    assert!(
        workflow.contains("builtin-hardware"),
        "real-hardware jobs must exist"
    );
    assert!(
        workflow.contains("AWMAN_TEST_BUILTIN_REQUIRE_HW"),
        "hardware jobs must fail when hardware is missing"
    );
    for needle in ["/dev/kvm", "hv_support"] {
        assert!(
            workflow.contains(needle),
            "prerequisite check for {needle} is missing"
        );
    }
}
