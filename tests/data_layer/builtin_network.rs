//! `builtin.network` and builtin resource settings as they travel from the
//! global and repo config files to the effective policy (D-05). Hermetic: the
//! files live in a temp directory and nothing is booted or dialled.

use std::path::Path;

use awman::data::config::builtin_network::MAX_NETWORK_ENTRIES;
use awman::data::config::effective::EffectiveConfig;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::fields::field_spec;
use awman::data::config::{
    BuiltinRuntimeConfig, FlagConfig, GlobalConfig, NetworkAllowEntry, NetworkMode, RepoConfig,
};

fn write(dir: &Path, name: &str, json: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, json).unwrap();
    path
}

fn effective(global: &str, repo: &str) -> EffectiveConfig {
    let dir = tempfile::tempdir().unwrap();
    let global = GlobalConfig::load_path(&write(dir.path(), "global.json", global)).unwrap();
    let git_root = dir.path().join("repo");
    std::fs::create_dir_all(git_root.join(".awman")).unwrap();
    write(&git_root.join(".awman"), "config.json", repo);
    let repo = RepoConfig::load(&git_root).unwrap();
    EffectiveConfig::new(FlagConfig::default(), EnvSnapshot::default(), repo, global)
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
        r#"{"builtin":{"network":{"mode":"public","proxy":"socks5://x"}}}"#,
    );
    assert!(GlobalConfig::load_path(&bad).is_err());
    let bad_mode = write(
        dir.path(),
        "m.json",
        r#"{"builtin":{"network":{"mode":"host"}}}"#,
    );
    assert!(GlobalConfig::load_path(&bad_mode).is_err());
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
        let global = format!(r#"{{"builtin":{{"network":{block}}}}}"#);
        let eff = effective(&global, "{}");
        let error = eff.builtin_network().unwrap_err();
        assert!(error.contains(needle), "{block}: {error}");
        // The block is also refused by the builtin block's own validation.
        assert!(eff.builtin_runtime().validate().is_err(), "{block}");
    }
    let too_many: Vec<String> = (0..=MAX_NETWORK_ENTRIES)
        .map(|i| format!("\"h{i}.example.com\""))
        .collect();
    let global = format!(
        r#"{{"builtin":{{"network":{{"mode":"allowlist","allow":[{}]}}}}}}"#,
        too_many.join(",")
    );
    assert!(effective(&global, "{}").builtin_network().is_err());
}

#[test]
fn resource_limits_reject_fractional_invalid_and_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        r#"{"builtin":{"vcpus":1.5}}"#,
        r#"{"builtin":{"vcpus":256}}"#,
        r#"{"builtin":{"vcpus":-1}}"#,
        r#"{"builtin":{"memoryMib":4294967296}}"#,
        r#"{"builtin":{"memoryMib":512.5}}"#,
    ] {
        assert!(
            GlobalConfig::load_path(&write(dir.path(), "r.json", bad)).is_err(),
            "{bad}"
        );
    }
    for (vcpus, memory, ok) in [
        (Some(0), None, false),
        (None, Some(0), false),
        (None, Some(127), false),
        (Some(1), Some(128), true),
        (Some(255), Some(u32::MAX), true),
    ] {
        let config = BuiltinRuntimeConfig {
            vcpus,
            memory_mib: memory,
            ..Default::default()
        };
        assert_eq!(config.validate().is_ok(), ok, "{vcpus:?} {memory:?}");
    }
}

#[test]
fn builtin_network_is_listed_read_only_for_config_show() {
    let spec = field_spec("builtin.network").expect("listed");
    assert!(
        spec.read_only,
        "the network object is edited in the JSON file"
    );
}
