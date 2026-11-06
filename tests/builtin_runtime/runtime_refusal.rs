//! Public-API refusal policy of the builtin runtime. None of these tests may
//! start a guest, touch the state directory of a real installation, or fall
//! back to another runtime.

use awman::data::config::effective::EffectiveConfig;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::flags::FlagConfig;
use awman::data::config::runtime::RuntimeSelection;
use awman::data::config::{ConfigFile, FileKind};
use awman::engine::agent_runtime::detect_effective;
use awman::engine::container::runtime::BuiltinRuntimeSettings;
use awman::engine::container::ContainerRuntime;
use awman::engine::error::EngineError;

#[test]
fn startup_overrides_use_the_json_frontend_and_hide_values() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        config.join("config.json"),
        r#"{"runtime":{"kind":"builtin"}}"#,
    )
    .unwrap();
    for variable in awman::data::config::env::AMBIENT_MSB_OVERRIDES {
        use std::os::unix::ffi::OsStringExt;
        for value in [
            std::ffi::OsString::from(""),
            std::ffi::OsString::from("private-override-value"),
            std::ffi::OsString::from_vec(vec![0xff]),
        ] {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_awman"))
                .args(["ready", "--json"])
                .current_dir(tmp.path())
                .env("AWMAN_HOME", &config)
                .env(variable, &value)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            let json: serde_json::Value =
                serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
                    panic!(
                        "{variable}: stdout={:?}, stderr={:?}",
                        output.stdout, output.stderr
                    )
                });
            let error = json["error"].as_str().unwrap();
            assert!(error.contains(variable), "{error}");
            assert!(!error.contains("private-override-value"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private-override-value"));
        }
    }
}

fn settings(state: &std::path::Path) -> BuiltinRuntimeSettings {
    BuiltinRuntimeSettings {
        state_dir: state.into(),
        ambient_overrides: Vec::new(),
        test_isolation: true,
    }
}

fn open(settings: BuiltinRuntimeSettings) -> Result<ContainerRuntime, EngineError> {
    ContainerRuntime::builtin(settings)
}

fn builtin_config(env: EnvSnapshot) -> EffectiveConfig {
    let global = ConfigFile::from_value(
        serde_json::json!({"runtime": {"kind": "builtin"}}),
        FileKind::Global,
    )
    .unwrap();
    EffectiveConfig::from_files(global, ConfigFile::default(), FlagConfig::default(), env).unwrap()
}

#[test]
fn builtin_ambient_override_is_refused_before_anything_else() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    for variable in [
        "MSB_PATH",
        "MSB_LIBKRUNFW_PATH",
        "MSB_AGENTD_PATH",
        "MSB_HOME",
        "MSB_BACKEND",
    ] {
        let mut s = settings(&state);
        s.ambient_overrides = vec![variable];
        match open(s) {
            Err(EngineError::AmbientRuntimeOverride { variable: named }) => {
                assert_eq!(named, variable)
            }
            other => panic!(
                "{variable}: expected AmbientRuntimeOverride, got {:?}",
                other.map(|_| ())
            ),
        }
    }
    assert!(
        !state.exists(),
        "a refusal must not create the state directory"
    );
}

#[test]
fn builtin_is_unavailable_under_test_isolation_and_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    // An unavailable builtin runtime still constructs (its open is retried on
    // use), so the refusal is observed through `availability()`.
    match open(settings(&state)).and_then(|runtime| runtime.availability()) {
        Err(EngineError::BuiltinRuntimeUnavailable { reason }) => {
            assert!(!reason.is_empty());
            // Only the released target (macOS on Apple Silicon) reaches the
            // test-isolation / feature check; every other host, including
            // the Linux targets WI 0124 is bringing up, is refused first.
            let released = matches!(
                (std::env::consts::OS, std::env::consts::ARCH),
                ("macos", "aarch64")
            );
            if released {
                assert!(
                    reason.contains("test isolation") || reason.contains("does not include"),
                    "{reason}"
                );
            } else {
                assert!(
                    reason.contains("only available on") || reason.contains("does not include"),
                    "{reason}"
                );
                assert!(
                    reason.contains("does not include") || reason.contains("macos/aarch64"),
                    "{reason}"
                );
            }
        }
        // Every host but the released target is refused with its own typed
        // error, before the test-isolation / feature check.
        Err(e @ EngineError::BuiltinTargetUnreleased { .. }) => {
            let reason = e.to_string();
            assert!(reason.contains("only available on"), "{reason}");
            assert!(reason.contains("macos/aarch64"), "{reason}");
        }
        other => panic!(
            "the builtin runtime must be unavailable by default: {:?}",
            other.map(|_| ())
        ),
    }
    assert!(
        !state.exists(),
        "an unavailable runtime must not create state"
    );
}

#[test]
fn builtin_test_gate_is_off_by_default_in_resolved_settings() {
    // Without AWMAN_TEST_BUILTIN the resolved settings mark the runtime
    // isolated (unavailable) whenever the suite runs under isolation.
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_string_lossy().into_owned();
    let config = builtin_config(EnvSnapshot::with_overrides([
        ("AWMAN_HOME", dir.as_str()),
        ("AWMAN_TEST_ISOLATION", "1"),
    ]));
    let resolved = BuiltinRuntimeSettings::resolve(&config.builtin_process_settings().unwrap());
    let isolated = std::env::var("AWMAN_TEST_ISOLATION").is_ok();
    if isolated {
        assert!(
            resolved.test_isolation,
            "isolation without the gate must disable builtin"
        );
    }
    let launch = config.builtin_launch_settings().unwrap();
    assert_eq!(
        (launch.vcpus, launch.memory_mib),
        (4, 4096),
        "documented per-launch defaults"
    );
    assert!(resolved.ambient_overrides.is_empty());

    let gated = builtin_config(EnvSnapshot::with_overrides([
        ("AWMAN_HOME", dir.as_str()),
        ("AWMAN_TEST_BUILTIN", "1"),
    ]));
    let resolved = BuiltinRuntimeSettings::resolve(&gated.builtin_process_settings().unwrap());
    assert!(
        !resolved.test_isolation,
        "AWMAN_TEST_BUILTIN=1 opts back in"
    );
}

#[test]
fn builtin_ambient_msb_variables_reach_the_settings_and_block_detection() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_string_lossy().into_owned();
    let config = builtin_config(EnvSnapshot::with_overrides([
        ("AWMAN_HOME", dir.as_str()),
        ("MSB_PATH", "/somewhere/else/msb"),
        ("MSB_LIBKRUNFW_PATH", "/somewhere/else/libkrunfw"),
    ]));
    let resolved = BuiltinRuntimeSettings::resolve(&config.builtin_process_settings().unwrap());
    assert_eq!(
        resolved.ambient_overrides,
        vec!["MSB_PATH", "MSB_LIBKRUNFW_PATH"]
    );
    match detect_effective(&config) {
        Err(EngineError::AmbientRuntimeOverride { variable }) => assert_eq!(variable, "MSB_PATH"),
        other => panic!(
            "detect must refuse, never fall back: {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn builtin_never_falls_back_to_docker_and_a_typo_is_fatal() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_string_lossy().into_owned();
    let env = EnvSnapshot::with_overrides([("AWMAN_HOME", dir.as_str()), ("MSB_HOME", "/x")]);
    // A failing builtin selection is an error value, not a Docker runtime.
    assert!(detect_effective(&builtin_config(env)).is_err());

    // A typo never reaches detection: the config file does not load.
    let typo = ConfigFile::from_value(
        serde_json::json!({"runtime": {"kind": "builtn"}}),
        FileKind::Global,
    )
    .unwrap_err()
    .to_string();
    assert!(
        typo.contains("runtime.kind") && typo.contains("builtin"),
        "{typo}"
    );
}

#[test]
fn builtin_selection_parses_and_round_trips() {
    assert_eq!(
        RuntimeSelection::parse(Some("builtin")),
        Ok(RuntimeSelection::BuiltinExperimental)
    );
    assert_eq!(RuntimeSelection::BuiltinExperimental.as_str(), "builtin");
    assert_eq!(
        RuntimeSelection::parse(None),
        Ok(RuntimeSelection::Docker),
        "default unchanged"
    );
    assert_eq!(
        RuntimeSelection::parse(Some(" builtin ")),
        Ok(RuntimeSelection::BuiltinExperimental),
        "whitespace is trimmed"
    );
    assert!(
        RuntimeSelection::parse(Some("Builtin")).is_err(),
        "spellings are exact, never guessed"
    );
    assert!(
        RuntimeSelection::parse(Some("builtin-experimental")).is_err(),
        "the pre-0139 spelling is not accepted"
    );
    assert_eq!(
        RuntimeSelection::valid_values(),
        "docker, apple, sbx, builtin"
    );
}

#[test]
fn adopted_sdk_ambient_override_set_and_conflict_precedence_are_preserved() {
    use awman::data::config::env::AMBIENT_MSB_OVERRIDES;
    let expected = [
        "MSB_PATH",
        "MSB_LIBKRUNFW_PATH",
        "MSB_AGENTD_PATH",
        "MSB_HOME",
        "MSB_BACKEND",
        "MSB_CONFIG_PATH",
        "MSB_PROFILE",
        "MSB_CACHE_DIR",
        "MSB_SANDBOXES_DIR",
        "MSB_VOLUMES_DIR",
        "MSB_SNAPSHOTS_DIR",
        "MSB_LOGS_DIR",
        "MSB_SECRETS_DIR",
    ];
    assert_eq!(AMBIENT_MSB_OVERRIDES, expected);
    // Conflicting selectors in reverse order still use the refusal order,
    // including empty values, before any runtime state is created.
    let env = EnvSnapshot::with_overrides(expected.iter().rev().map(|name| (*name, "")));
    assert_eq!(env.ambient_msb_overrides(), expected);
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let mut s = settings(&state);
    s.ambient_overrides = env.ambient_msb_overrides();
    assert!(matches!(
        open(s),
        Err(EngineError::AmbientRuntimeOverride { variable }) if variable == "MSB_PATH"
    ));
    assert!(!state.exists());
}
