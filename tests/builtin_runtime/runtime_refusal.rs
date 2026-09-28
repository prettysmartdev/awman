//! Public-API refusal policy of the builtin runtime. None of these tests may
//! start a guest, touch the state directory of a real installation, or fall
//! back to another runtime.

use std::collections::BTreeMap;

use awman::data::config::effective::EffectiveConfig;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::flags::FlagConfig;
use awman::data::config::global::GlobalConfig;
use awman::data::config::repo::RepoConfig;
use awman::data::config::runtime_selection::RuntimeSelection;
use awman::engine::agent_runtime::detect_effective;
use awman::engine::container::runtime::BuiltinRuntimeSettings;
use awman::engine::container::ContainerRuntime;
use awman::engine::error::EngineError;

fn settings(state: &std::path::Path) -> BuiltinRuntimeSettings {
    BuiltinRuntimeSettings {
        state_dir: state.into(),
        vcpus: 2,
        memory_mib: 4096,
        image_source: None,
        images: BTreeMap::new(),
        registries: BTreeMap::new(),
        ambient_overrides: Vec::new(),
        test_isolation: true,
    }
}

fn open(settings: BuiltinRuntimeSettings) -> Result<ContainerRuntime, EngineError> {
    ContainerRuntime::builtin(settings)
}

fn builtin_config(env: EnvSnapshot) -> EffectiveConfig {
    EffectiveConfig::new(
        FlagConfig::default(),
        env,
        RepoConfig::default(),
        GlobalConfig {
            runtime: Some("builtin".into()),
            ..Default::default()
        },
    )
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
    match open(settings(&state)) {
        Err(EngineError::BuiltinRuntimeUnavailable { reason }) => {
            assert!(!reason.is_empty());
            let supported = matches!(
                (std::env::consts::OS, std::env::consts::ARCH),
                ("linux", "x86_64") | ("linux", "aarch64") | ("macos", "aarch64")
            );
            if supported {
                assert!(
                    reason.contains("test isolation") || reason.contains("does not include"),
                    "{reason}"
                );
            } else {
                assert!(reason.contains("not supported on"), "{reason}");
                assert!(
                    reason.contains("linux/x86_64") && reason.contains("macos/aarch64"),
                    "{reason}"
                );
            }
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
        ("AWMAN_CONFIG_HOME", dir.as_str()),
        ("AWMAN_TEST_ISOLATION", "1"),
    ]));
    let resolved = BuiltinRuntimeSettings::resolve(&config).unwrap();
    let isolated = std::env::var("AWMAN_TEST_ISOLATION").is_ok();
    if isolated {
        assert!(
            resolved.test_isolation,
            "isolation without the gate must disable builtin"
        );
    }
    assert_eq!(
        (resolved.vcpus, resolved.memory_mib),
        (2, 4096),
        "documented defaults"
    );
    assert!(resolved.ambient_overrides.is_empty());

    let gated = builtin_config(EnvSnapshot::with_overrides([
        ("AWMAN_CONFIG_HOME", dir.as_str()),
        ("AWMAN_TEST_BUILTIN", "1"),
    ]));
    let resolved = BuiltinRuntimeSettings::resolve(&gated).unwrap();
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
        ("AWMAN_CONFIG_HOME", dir.as_str()),
        ("MSB_PATH", "/somewhere/else/msb"),
        ("MSB_LIBKRUNFW_PATH", "/somewhere/else/libkrunfw"),
    ]));
    let resolved = BuiltinRuntimeSettings::resolve(&config).unwrap();
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
    let env =
        EnvSnapshot::with_overrides([("AWMAN_CONFIG_HOME", dir.as_str()), ("MSB_HOME", "/x")]);
    // A failing builtin selection is an error value, not a Docker runtime.
    assert!(detect_effective(&builtin_config(env)).is_err());

    let typo = EffectiveConfig::new(
        FlagConfig::default(),
        EnvSnapshot::empty(),
        RepoConfig::default(),
        GlobalConfig {
            runtime: Some("builtn".into()),
            ..Default::default()
        },
    );
    match detect_effective(&typo) {
        Err(EngineError::UnknownRuntime { value, valid }) => {
            assert_eq!(value, "builtn");
            assert!(valid.contains("builtin"), "{valid}");
        }
        other => panic!("expected UnknownRuntime: {:?}", other.map(|_| ())),
    }
}

#[test]
fn builtin_selection_parses_and_round_trips() {
    assert_eq!(
        RuntimeSelection::parse(Some("builtin")),
        Ok(RuntimeSelection::Builtin)
    );
    assert_eq!(RuntimeSelection::Builtin.as_str(), "builtin");
    assert_eq!(
        RuntimeSelection::parse(None),
        Ok(RuntimeSelection::Docker),
        "default unchanged"
    );
    assert_eq!(
        RuntimeSelection::parse(Some(" builtin ")),
        Ok(RuntimeSelection::Builtin),
        "whitespace is trimmed"
    );
    assert!(
        RuntimeSelection::parse(Some("Builtin")).is_err(),
        "spellings are exact, never guessed"
    );
    assert!(RuntimeSelection::valid_values().contains("builtin"));
}
