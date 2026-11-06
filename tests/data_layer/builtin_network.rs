//! `runtime.builtin.network` and builtin resource settings as they travel from the
//! global and repo config files to the effective policy (D-05). Hermetic: the
//! files live in a temp directory and nothing is booted or dialled.

use std::path::Path;

use awman::data::config::builtin_network::MAX_NETWORK_ENTRIES;
use awman::data::config::effective::EffectiveConfig;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::fields::{resolve, ValueKind};
use awman::data::config::units::Size;
use awman::data::config::{
    BuiltinRuntimeConfig, ConfigFile, FileKind, FlagConfig, NetworkAllowEntry, NetworkMode,
};

/// The fixtures below are written in the compact `{"builtin": …}` form; nest
/// them under `runtime` (and `"runtime": "<kind>"` under `runtime.kind`).
fn nest(json: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(json).unwrap();
    let object = value.as_object_mut().unwrap();
    let mut runtime = serde_json::Map::new();
    if let Some(kind) = object.remove("runtime") {
        runtime.insert("kind".into(), kind);
    }
    if let Some(builtin) = object.remove("builtin") {
        runtime.insert("builtin".into(), builtin);
    }
    if !runtime.is_empty() {
        object.insert("runtime".into(), runtime.into());
    }
    value.to_string()
}

fn load(path: &Path, kind: FileKind) -> Result<ConfigFile, awman::data::error::DataError> {
    ConfigFile::load(path, kind)
}

fn write(dir: &Path, name: &str, json: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, json).unwrap();
    path
}

fn effective(global: &str, repo: &str) -> EffectiveConfig {
    let dir = tempfile::tempdir().unwrap();
    let global = load(
        &write(dir.path(), "global.json", &nest(global)),
        FileKind::Global,
    )
    .unwrap();
    let repo = load(&write(dir.path(), "repo.json", &nest(repo)), FileKind::Repo).unwrap();
    EffectiveConfig::from_files(global, repo, FlagConfig::default(), EnvSnapshot::default())
        .unwrap()
}

#[test]
fn network_config_merge_and_unknown_fields() {
    let eff = effective(
        r#"{"runtime":"builtin","builtin":{"network":{"mode":"allowlist",
            "allow":["*.example.com","api.anthropic.com"],"hostPorts":[8765,9000],
            "nameservers":["1.1.1.1"],"trustHostCas":true}}}"#,
        r#"{"builtin":{"vcpus":4,"network":{"allow":["api.example.com"],"hostPorts":[8765]}}}"#,
    );
    let network = eff.builtin_network().unwrap();
    assert_eq!(network.mode, NetworkMode::Allowlist);
    assert_eq!(
        network.allow,
        vec![NetworkAllowEntry::Domain("api.example.com".into())]
    );
    assert_eq!(network.host_ports, vec![8765]);
    assert_eq!(network.nameservers, vec!["1.1.1.1:53".parse().unwrap()]);
    assert!(network.trust_host_cas);
    // The rest of the builtin block still merges per field.
    assert_eq!(eff.builtin_runtime().vcpus, Some(4));

    // Unknown keys inside the block are a parse error for the whole file.
    let dir = tempfile::tempdir().unwrap();
    let bad = write(
        dir.path(),
        "g.json",
        &nest(r#"{"builtin":{"network":{"mode":"public","proxy":"socks5://x"}}}"#),
    );
    assert!(load(&bad, FileKind::Global).is_err());
    let bad_mode = write(
        dir.path(),
        "m.json",
        &nest(r#"{"builtin":{"network":{"mode":"host"}}}"#),
    );
    assert!(load(&bad_mode, FileKind::Global).is_err());
}

#[test]
fn default_policy_is_public_without_host_access() {
    let network = effective(r#"{"runtime":"builtin"}"#, "{}")
        .builtin_network()
        .unwrap();
    assert_eq!(network.mode, NetworkMode::Public);
    assert!(network.allow.is_empty() && network.host_ports.is_empty());
    assert!(network.nameservers.is_empty() && !network.trust_host_cas);
}

#[test]
fn a_repository_cannot_widen_the_global_network_policy() {
    for (global, repo, needle) in [
        (
            r#"{"builtin":{"network":{"mode":"none"}}}"#,
            r#"{"builtin":{"network":{"mode":"public"}}}"#,
            "less restrictive",
        ),
        (
            r#"{"builtin":{"network":{"mode":"allowlist","allow":["a.example.com"]}}}"#,
            r#"{"builtin":{"network":{"allow":["b.example.com"]}}}"#,
            "not permitted",
        ),
        (
            r#"{}"#,
            r#"{"builtin":{"network":{"hostPorts":[22]}}}"#,
            "not authorised",
        ),
        (
            r#"{}"#,
            r#"{"builtin":{"network":{"nameservers":["9.9.9.9"]}}}"#,
            "global config only",
        ),
        (
            r#"{}"#,
            r#"{"builtin":{"network":{"trustHostCas":true}}}"#,
            "global config only",
        ),
    ] {
        let error = effective(global, repo).builtin_network().unwrap_err();
        assert!(error.contains(needle), "{repo}: {error}");
    }
}

#[test]
fn network_unsupported_policy_is_an_explicit_error() {
    for (block, needle) in [
        (r#"{"allow":["10.0.0.0/8"]}"#, "address"),
        (r#"{"mode":"allowlist","allow":["192.168.1.1"]}"#, "address"),
        (r#"{"mode":"allowlist","allow":["localhost"]}"#, "hostPorts"),
        (r#"{"mode":"none","hostPorts":[8765]}"#, "none"),
        (r#"{"hostPorts":[53]}"#, "53"),
        (r#"{"nameservers":["dns.google"]}"#, "IP"),
        (
            r#"{"mode":"public","allow":["a.example.com"]}"#,
            "allowlist",
        ),
    ] {
        // A block that is invalid on its own fails when its file loads, so
        // no launch can ever see it.
        let dir = tempfile::tempdir().unwrap();
        let global = nest(&format!(r#"{{"builtin":{{"network":{block}}}}}"#));
        let error = load(&write(dir.path(), "g.json", &global), FileKind::Global)
            .unwrap_err()
            .to_string();
        assert!(error.contains(needle), "{block}: {error}");
        assert!(
            error.contains("runtime.builtin.network"),
            "{block}: {error}"
        );
    }
    let too_many: Vec<String> = (0..=MAX_NETWORK_ENTRIES)
        .map(|i| format!("\"h{i}.example.com\""))
        .collect();
    let global = format!(
        r#"{{"builtin":{{"network":{{"mode":"allowlist","allow":[{}]}}}}}}"#,
        too_many.join(",")
    );
    let dir = tempfile::tempdir().unwrap();
    assert!(load(
        &write(dir.path(), "g.json", &nest(&global)),
        FileKind::Global
    )
    .is_err());
}

#[test]
fn resource_limits_reject_fractional_invalid_and_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        r#"{"builtin":{"vcpus":1.5}}"#,
        r#"{"builtin":{"vcpus":256}}"#,
        r#"{"builtin":{"vcpus":-1}}"#,
        r#"{"builtin":{"memory":"4294967296TiB"}}"#,
        r#"{"builtin":{"memory":"512.5MiB"}}"#,
    ] {
        assert!(
            load(&write(dir.path(), "r.json", &nest(bad)), FileKind::Global).is_err(),
            "{bad}"
        );
    }
    for (vcpus, memory, ok) in [
        (Some(0), None, false),
        (None, Some(0), false),
        (None, Some(127), false),
        (Some(1), Some(128), true),
        (Some(255), Some(u64::from(u32::MAX)), true),
    ] {
        let config = BuiltinRuntimeConfig {
            vcpus,
            memory: memory.map(Size::from_mib),
            ..Default::default()
        };
        assert_eq!(config.validate().is_ok(), ok, "{vcpus:?} {memory:?}");
    }
}

#[test]
fn builtin_network_is_listed_read_only_for_config_show() {
    // Each network leaf is a key-table row; the image source is an object
    // edited in the JSON file.
    assert!(resolve("runtime.builtin.network.mode").is_ok());
    assert_eq!(
        resolve("runtime.builtin.imageSource").unwrap().key.kind,
        ValueKind::Object
    );
}
