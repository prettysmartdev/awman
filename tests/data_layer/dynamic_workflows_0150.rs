//! WI 0150 Layer 0 dynamic-workflow schema, merge and migration coverage.

use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
use awman::data::config::global::GlobalConfig;
use awman::data::config::migrations::RepoConfigMigration;
use awman::data::config::repo::RepoConfig;
use awman::data::config::{EffectiveConfig, FlagConfig};

#[test]
fn isolated_repo_and_global_dynamic_settings_resolve_with_sources_and_migrate_idempotently() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.path().to_str().unwrap())]);
    let repo_path = RepoConfig::path(root.path());
    std::fs::create_dir_all(repo_path.parent().unwrap()).unwrap();
    std::fs::write(&repo_path, r#"{"dynamicWorkflows":{"defaultLeader":"repo::model","guidance":["same","repo"]},"opaque":{"keep":true}}"#).unwrap();
    let global_path = GlobalConfig::path_with(&env).unwrap();
    std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
    std::fs::write(&global_path, r#"{"workflows":{"dynamic":{"defaultLeader":"global::model","guidance":["global","same"]}}}"#).unwrap();

    let repo = RepoConfig::load(root.path()).unwrap();
    let global = GlobalConfig::load_with(&env).unwrap();
    let resolved = EffectiveConfig::new(FlagConfig::default(), env, repo, global)
        .dynamic_workflows()
        .unwrap();
    assert_eq!(resolved.default_leader.unwrap().value, "repo::model");
    assert_eq!(
        resolved
            .guidance
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["global", "same", "repo"]
    );
    assert_eq!(
        resolved.guidance[0].source.key_prefix(),
        "workflows.dynamic"
    );
    assert_eq!(resolved.guidance[2].source.key_prefix(), "dynamicWorkflows");

    RepoConfigMigration::plan(root.path())
        .unwrap()
        .apply()
        .unwrap();
    let migrated = std::fs::read(&repo_path).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&migrated).unwrap();
    assert!(json.get("dynamicWorkflows").is_none());
    assert_eq!(json["opaque"]["keep"], true);
    assert!(RepoConfigMigration::plan(root.path()).unwrap().is_empty());
    assert_eq!(
        RepoConfigMigration::plan(root.path()).unwrap().preview(),
        repo_path.display().to_string()
    );
}
