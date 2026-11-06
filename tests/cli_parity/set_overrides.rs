//! WI 0139: the override channel. `--set key=value` and derived `AWMAN_*`
//! variables, resolved against the key table and applied above the files.

use awman::data::config::effective::{ConfigLayers, EffectiveConfig, Layer};
use awman::data::config::env::{EnvSnapshot, AWMAN_HOME};
use awman::data::config::fields::KeyError;
use awman::data::config::overrides::ConfigOverrides;
use serde_json::json;

fn set(args: &[&str]) -> Result<ConfigOverrides, KeyError> {
    ConfigOverrides::parse_set_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn resolve(home: &std::path::Path, env: &[(&str, &str)], set_args: &[&str]) -> EffectiveConfig {
    let mut vars = vec![(AWMAN_HOME.to_string(), home.display().to_string())];
    vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let env = EnvSnapshot::with_overrides(vars);
    let layers = ConfigLayers::load(&env, None, set(set_args).unwrap()).unwrap();
    EffectiveConfig::resolve(layers, Default::default(), env).unwrap()
}

#[test]
fn precedence_is_default_then_file_then_env_then_set() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.json"),
        r#"{"tui":{"scrollbackLines":100}}"#,
    )
    .unwrap();
    let path = "tui.scrollbackLines";
    let value = |ec: &EffectiveConfig| ec.value()["tui"]["scrollbackLines"].clone();

    let ec = resolve(home.path(), &[], &[]);
    assert_eq!(
        (value(&ec), ec.source_of(path)),
        (json!(100), Layer::Global)
    );
    let env = [("AWMAN_TUI_SCROLLBACK_LINES", "200")];
    let ec = resolve(home.path(), &env, &[]);
    assert_eq!((value(&ec), ec.source_of(path)), (json!(200), Layer::Env));
    let ec = resolve(home.path(), &env, &["tui.scrollbackLines=300"]);
    assert_eq!((value(&ec), ec.source_of(path)), (json!(300), Layer::Set));
}

#[test]
fn set_parses_by_value_type() {
    let cases: &[(&str, serde_json::Value)] = &[
        ("agents.auth.refresh.enabled=true", json!(true)),
        ("workflows.maxConcurrentSteps=2", json!(2)),
        ("runtime.dockerfile=Dockerfile.ci", json!("Dockerfile.ci")),
        ("agents.launchMode=acp", json!("acp")),
        ("agents.stuckTimeout=2m", json!("2m")),
        ("runtime.builtin.rootDisk=64GiB", json!("64GiB")),
        ("workflows.dynamic.guidance=a,b", json!(["a", "b"])),
        ("runtime.builtin.network.hostPorts=1,2", json!([1, 2])),
        ("workServers.a.labels=x=1,y=2", json!({"x": "1", "y": "2"})),
        (
            "daemon.features.squad.interaction=headless",
            json!("headless"),
        ),
    ];
    for (arg, want) in cases {
        let overrides = set(&[arg]).unwrap_or_else(|e| panic!("{arg}: {e}"));
        assert_eq!(&overrides.0[0].1, want, "{arg}");
    }
    assert!(set(&["daemon.features.squad.interaction=non-interactive"]).is_err());
    assert!(matches!(
        set(&["runtime.builtin.images.x=y"]),
        Err(KeyError::ObjectNotSettable { .. })
    ));
}

#[test]
fn unknown_keys_and_removed_variables_are_rejected_or_ignored() {
    let err = set(&["launchMode=acp"]).unwrap_err().to_string();
    assert!(err.contains("unknown config key `launchMode`"), "{err}");
    assert!(err.contains("agents.launchMode"), "{err}");

    let home = tempfile::tempdir().unwrap();
    let ec = resolve(
        home.path(),
        &[
            ("AWMAN_LAUNCH_MODE", "acp"),
            ("AWMAN_MAX_CONCURRENT_AGENTS", "1"),
        ],
        &[],
    );
    assert_eq!(ec.source_of("agents.launchMode"), Layer::Default);
    assert_eq!(ec.max_concurrent_steps(), None);
}
