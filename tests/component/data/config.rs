//! Config schema, effective precedence, and secret masking contracts.

use awman::data::config::effective::{ConfigLayers, EffectiveConfig};
use awman::data::config::env::EnvSnapshot;
use awman::data::config::fields::{self, ValueKind, CONFIG_KEYS};
use awman::data::config::file::{ConfigFile, FileKind};
use awman::data::config::flags::FlagConfig;
use awman::data::config::overrides::ConfigOverrides;
use awman::data::fs::data_paths::{config_home, data_home, AWMAN_HOME};
use serde_json::{json, Value};

#[test]
fn every_key_default_is_valid_and_round_trips_through_file_validation() {
    assert!(!CONFIG_KEYS.is_empty());
    for key in CONFIG_KEYS {
        let default = key.default_value();
        fields::validate_value(key, &default)
            .unwrap_or_else(|error| panic!("{} default invalid: {error}", key.path));
        if !key.path.contains('*') {
            let mut document = Value::Object(Default::default());
            let mut cursor = &mut document;
            let segments: Vec<_> = key.path.split('.').collect();
            for segment in &segments[..segments.len() - 1] {
                cursor = cursor
                    .as_object_mut()
                    .unwrap()
                    .entry(*segment)
                    .or_insert_with(|| json!({}));
            }
            cursor
                .as_object_mut()
                .unwrap()
                .insert(segments[segments.len() - 1].to_owned(), default);
            // Scope-specific keys cannot appear in every file kind. Validate in
            // the broadest permitted file, while the key-level validator above
            // checks every row independently.
            let kind = match key.scope {
                fields::KeyScope::Global => FileKind::Global,
                fields::KeyScope::Repo => FileKind::Repo,
                fields::KeyScope::Task => FileKind::Task,
            };
            ConfigFile::from_value(document, kind)
                .unwrap_or_else(|error| panic!("{} default document invalid: {error}", key.path));
        }
    }
}

#[test]
fn every_key_has_a_deterministic_default_and_key_table_validation_contract() {
    for key in CONFIG_KEYS {
        let default = key.default_value();
        assert_eq!(
            key.default_value(),
            default,
            "unstable default for {}",
            key.path
        );
        fields::validate_value(key, &default).unwrap_or_else(|error| {
            panic!("{} default rejected by its validator: {error}", key.path)
        });
        assert!(
            !key.doc.trim().is_empty(),
            "{} has no schema description",
            key.path
        );
    }
}

#[test]
fn integer_key_boundaries_and_wrong_shapes_are_rejected_by_the_schema() {
    for key in CONFIG_KEYS {
        if let ValueKind::Int { min, max } = key.kind {
            if let Some(minimum) = min {
                fields::validate_value(key, &json!(minimum)).unwrap_or_else(|error| {
                    panic!("{} rejected inclusive minimum {minimum}: {error}", key.path)
                });
                if let Some(outside) = minimum.checked_sub(1) {
                    assert!(
                        fields::validate_value(key, &json!(outside)).is_err(),
                        "{} accepted below minimum",
                        key.path
                    );
                }
            }
            if let Some(maximum) = max {
                fields::validate_value(key, &json!(maximum)).unwrap_or_else(|error| {
                    panic!("{} rejected inclusive maximum {maximum}: {error}", key.path)
                });
                if let Some(outside) = maximum.checked_add(1) {
                    assert!(
                        fields::validate_value(key, &json!(outside)).is_err(),
                        "{} accepted above maximum",
                        key.path
                    );
                }
            }
        }

        let invalid = match key.kind {
            ValueKind::Bool => Some(json!("true")),
            ValueKind::Int { .. } => Some(json!("one")),
            ValueKind::Enum(_) => Some(json!("not-a-declared-enum-member")),
            ValueKind::Duration => Some(json!("tomorrow")),
            ValueKind::Size => Some(json!("1000MB")),
            ValueKind::StringList | ValueKind::IntList => Some(json!({"not":"an array"})),
            ValueKind::StringMap => Some(json!(["not", "an object"])),
            ValueKind::String => Some(json!(false)),
            ValueKind::Object => Some(json!(["not", "an object"])),
        };
        if let Some(invalid) = invalid {
            assert!(
                fields::validate_value(key, &invalid).is_err(),
                "{} accepted invalid shape {invalid}",
                key.path
            );
        }
    }
}

#[test]
fn every_key_obeys_the_declared_global_repo_and_task_scope_order() {
    for key in CONFIG_KEYS {
        assert!(
            FileKind::Global.admits(key.scope),
            "{} missing global scope",
            key.path
        );
        assert_eq!(
            FileKind::Repo.admits(key.scope),
            key.scope >= fields::KeyScope::Repo,
            "{} repo scope",
            key.path
        );
        assert_eq!(
            FileKind::Task.admits(key.scope),
            key.scope == fields::KeyScope::Task,
            "{} task scope",
            key.path
        );
    }
}

#[test]
fn global_repo_environment_set_and_flag_layers_follow_declared_precedence() {
    let global =
        ConfigFile::from_value(json!({"agents":{"default":"global"}}), FileKind::Global).unwrap();
    let repo =
        ConfigFile::from_value(json!({"agents":{"default":"repo"}}), FileKind::Repo).unwrap();
    let repo_wins = EffectiveConfig::from_files(
        global.clone(),
        repo.clone(),
        FlagConfig::default(),
        EnvSnapshot::empty(),
    )
    .unwrap();
    assert_eq!(repo_wins.value()["agents"]["default"], "repo");
    let global_only = EffectiveConfig::from_files(
        global.clone(),
        ConfigFile::default(),
        FlagConfig::default(),
        EnvSnapshot::empty(),
    )
    .unwrap();
    assert_eq!(global_only.value()["agents"]["default"], "global");
    let env = EnvSnapshot::with_overrides([("AWMAN_AGENTS_DEFAULT", "environment")]);
    let env_layer = ConfigOverrides::from_env(&env, |_| Vec::new()).unwrap();
    let layers = ConfigLayers {
        global: global.clone(),
        repo: repo.clone(),
        env: env_layer,
        set: ConfigOverrides::new(vec![("agents.default".into(), json!("set"))]),
        ..Default::default()
    };
    let resolved = EffectiveConfig::resolve(layers, FlagConfig::default(), env).unwrap();
    assert_eq!(resolved.value()["agents"]["default"], "set");

    let no_set = EffectiveConfig::resolve(
        ConfigLayers {
            set: ConfigOverrides::default(),
            ..resolved.layers().clone()
        },
        FlagConfig::default(),
        EnvSnapshot::empty(),
    )
    .unwrap();
    assert_eq!(no_set.value()["agents"]["default"], "environment");

    let flag = EffectiveConfig::from_files(
        global,
        repo,
        FlagConfig {
            agent: Some("flag".into()),
            ..Default::default()
        },
        EnvSnapshot::empty(),
    )
    .unwrap();
    assert_eq!(flag.agent().as_deref(), Some("flag"));
}

#[test]
fn invalid_config_is_rejected_before_save_and_sensitive_values_are_masked() {
    let bad = ConfigFile::from_value(json!({"tui":{"scrollbackLines":-1}}), FileKind::Global);
    assert!(bad.is_err());

    let config = ConfigFile::from_value(
        json!({"remote":{"targets":{"prod":{"url":"https://example.test","apiKey":"token-not-for-output"}}}}),
        FileKind::Global,
    ).unwrap();
    let effective = EffectiveConfig::from_files(
        config,
        ConfigFile::default(),
        FlagConfig::default(),
        EnvSnapshot::empty(),
    )
    .unwrap();
    let masked = effective.masked_value().to_string();
    assert!(!masked.contains("token-not-for-output"));
}

#[test]
fn older_config_documents_keep_their_wire_values_when_rewritten() {
    // This is the schema-1 file shape persisted by the previous config writer.
    let old = r#"{"agents":{"default":"codex"},"tui":{"scrollbackLines":4096}}"#;
    let parsed = ConfigFile::parse(
        old,
        FileKind::Global,
        std::path::Path::new("config-v1.json"),
    )
    .unwrap();
    let emitted: Value = serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
    assert_eq!(emitted["agents"]["default"], "codex");
    assert_eq!(emitted["tui"]["scrollbackLines"], 4096);
}

#[test]
fn explicit_home_and_xdg_roots_resolve_independently_without_process_environment() {
    let split = EnvSnapshot::with_overrides([
        ("XDG_CONFIG_HOME", "/memory/xdg-config"),
        ("XDG_DATA_HOME", "/memory/xdg-data"),
    ]);
    assert_eq!(
        config_home(&split).unwrap(),
        std::path::PathBuf::from("/memory/xdg-config/awman")
    );
    assert_eq!(
        data_home(&split).unwrap(),
        std::path::PathBuf::from("/memory/xdg-data/awman")
    );

    let unified = EnvSnapshot::with_overrides([
        (AWMAN_HOME, "/memory/awman"),
        ("XDG_CONFIG_HOME", "/ignored/config"),
        ("XDG_DATA_HOME", "/ignored/data"),
    ]);
    assert_eq!(
        config_home(&unified).unwrap(),
        std::path::PathBuf::from("/memory/awman")
    );
    assert_eq!(
        data_home(&unified).unwrap(),
        std::path::PathBuf::from("/memory/awman")
    );
}

/// The shipped configuration reference must match the schema, including markers.
#[test]
fn configuration_reference_key_table_matches_the_schema() {
    let text = include_str!("../../../docs/07-configuration.md");
    let start = "<!-- key-table:start -->";
    let end = "<!-- key-table:end -->";
    let body = text
        .split_once(start)
        .expect("start marker")
        .1
        .split_once(end)
        .expect("end marker")
        .0;
    assert_eq!(
        body,
        format!("\n{}", fields::render_docs_table()),
        "regenerate the configuration reference key table"
    );
}

#[test]
fn unified_home_roots_every_store_and_removed_overrides_are_inert() {
    use awman::data::fs::{ApiPaths, DataPaths, SquadPaths};
    use std::path::Path;
    let unified = EnvSnapshot::with_overrides([
        (AWMAN_HOME, "/aw"),
        ("XDG_CONFIG_HOME", "/ignored-config"),
        ("XDG_DATA_HOME", "/ignored-data"),
    ]);
    let data = DataPaths::from_env(&unified).unwrap();
    let api = ApiPaths::from_env(&unified).unwrap();
    let squad = SquadPaths::from_env(&unified).unwrap();
    assert_eq!(data.daemon().root(), Path::new("/aw/data/daemon"));
    assert_eq!(data.db_path(), Path::new("/aw/data/awman.db"));
    assert_eq!(api.root(), Path::new("/aw/api"));
    assert_eq!(squad.root(), Path::new("/aw/squad"));
    assert_eq!(api.daemon(), squad.daemon());
    assert_eq!(
        awman::data::config::global_config_path(&unified).unwrap(),
        Path::new("/aw/config.json")
    );
    let split = EnvSnapshot::with_overrides([
        ("XDG_CONFIG_HOME", "/xc"),
        ("XDG_DATA_HOME", "/xd"),
        ("AWMAN_CONFIG_HOME", "/old"),
        ("AWMAN_API_ROOT", "/old-api"),
        ("AWMAN_SQUAD_ROOT", "/old-squad"),
        ("AWMAN_ATTACH_DIR", "/old-attach"),
    ]);
    assert_eq!(
        awman::data::config::global_config_path(&split).unwrap(),
        Path::new("/xc/awman/config.json")
    );
    assert_eq!(
        ApiPaths::from_env(&split).unwrap().root(),
        Path::new("/xd/awman/api")
    );
    assert_eq!(
        SquadPaths::from_env(&split).unwrap().root(),
        Path::new("/xd/awman/squad")
    );
}
