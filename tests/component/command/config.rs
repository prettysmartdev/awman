//! Command projections derive coverage from every config-key registration.
use awman::command::commands::config::{collect_config_rows, ConfigDocs};
use awman::data::config::{fields::CONFIG_KEYS, Layer};
use std::collections::BTreeSet;

#[test]
fn every_config_key_has_a_command_row_or_a_declared_map_container() {
    let mut empty = serde_json::json!({});
    for key in CONFIG_KEYS {
        awman::data::config::config_json::set_config_field(
            &mut empty,
            &key.path.replace('*', "example"),
            key.default_value(),
        );
    }
    let rows = collect_config_rows(&ConfigDocs {
        global: &empty,
        repo: &empty,
        effective: &empty,
        source: &|_| Layer::Default,
    });
    let names: BTreeSet<_> = rows.iter().map(|r| r.field.as_str()).collect();
    for key in CONFIG_KEYS {
        let concrete = key.path.replace('*', "example");
        let expected = if key.path == "workServers.*.labels.*" {
            "workServers.example.labels"
        } else {
            &concrete
        };
        assert!(
            names.contains(expected)
                || rows
                    .iter()
                    .any(|r| r.field.starts_with(&format!("{expected}."))),
            "missing command row for {}",
            key.path
        );
    }
    assert_eq!(names.len(), rows.len(), "duplicate config rows");
    assert!(rows.iter().all(|r| !r.type_name.is_empty()));
}
