use awman::data::session::AgentName;
use awman::data::{
    config::{
        agent_definition::FIRST_PARTY_AGENT_NAMES,
        fields,
        file::{ConfigFile, FileKind},
    },
    fs::auth_paths::AuthPathResolver,
};
use awman::engine::agent::{
    first_party,
    registry::{AgentRegistry, TrustStatus},
};

#[test]
fn first_party_catalogue_and_opencode_majors_are_distinct() {
    let sandbox = crate::support::sandbox(
        "data::agent_catalogue::first_party_catalogue_and_opencode_majors_are_distinct",
    );
    assert_eq!(
        FIRST_PARTY_AGENT_NAMES,
        &[
            "claude",
            "codex",
            "opencode",
            "opencode-v2",
            "cursor",
            "copilot",
            "cline",
            "antigravity",
        ]
    );
    let v1 = first_party::first_party("opencode").expect("OpenCode 1.x built-in");
    let v2 = first_party::first_party("opencode-v2").expect("OpenCode 2.x built-in");
    assert_ne!(v1.name, v2.name);
    assert_eq!(
        v1.interactive_entrypoint.first().map(String::as_str),
        Some("opencode")
    );
    assert_eq!(
        v2.interactive_entrypoint.first().map(String::as_str),
        Some("opencode")
    );
    assert_ne!(
        v1.cli_version
            .as_ref()
            .map(|requirement| requirement.required_major),
        v2.cli_version
            .as_ref()
            .map(|requirement| requirement.required_major)
    );
    assert!(first_party::first_party("gemini").is_none());
    sandbox.finish();
}

#[test]
fn custom_gemini_definition_uses_normal_review_and_approval_path() {
    let sandbox = crate::support::sandbox(
        "data::agent_catalogue::custom_gemini_definition_uses_normal_review_and_approval_path",
    );
    let root = sandbox.work();
    std::fs::create_dir_all(root.join(".awman/agents")).unwrap();
    std::fs::write(
        root.join(".awman/agents/gemini.toml"),
        r#"schema = 1
display_name = "Gemini CLI"

[run]
command = ["gemini"]

[image]
install = ["npm install -g @google/gemini-cli"]
"#,
    )
    .unwrap();
    let env = crate::support::env_snapshot(&sandbox);
    let name = AgentName::new("gemini").unwrap();
    let untrusted = AgentRegistry::load_untrusted(&env, &name, Some(&root)).unwrap();
    assert!(matches!(
        AgentRegistry::trust_status(&env, untrusted.definition(), Some(&root)).unwrap(),
        TrustStatus::Pending
    ));
    assert!(matches!(
        AgentRegistry::resolve(&env, &name, Some(&root)),
        Err(awman::engine::error::EngineError::AgentDefinitionUntrusted { .. })
    ));
    AgentRegistry::record_approval(&env, untrusted.definition(), Some(&root)).unwrap();
    let resolved = AgentRegistry::resolve(&env, &name, Some(&root)).unwrap();
    assert!(resolved.is_custom());
    assert_eq!(resolved.name, "gemini");
    sandbox.finish();
}

#[test]
fn retired_gemini_error_keeps_migration_guidance_and_antigravity_auth_path() {
    let sandbox = crate::support::sandbox(
        "data::agent_catalogue::retired_gemini_error_keeps_migration_guidance_and_antigravity_auth_path",
    );
    let env = crate::support::env_snapshot(&sandbox);
    let root = sandbox.work();
    let error = AgentRegistry::unknown(&env, "gemini", Some(&root), false).to_string();
    assert!(
        error.contains("'gemini' is no longer a built-in agent"),
        "{error}"
    );
    assert!(error.contains("built-in 'antigravity'"), "{error}");
    assert!(error.contains("awman new agent gemini"), "{error}");
    let auth = AuthPathResolver::at_home(sandbox.home()).resolve("antigravity");
    let expected_settings = sandbox.home().join(".gemini");
    assert_eq!(
        auth.settings_dir.as_deref(),
        Some(expected_settings.as_path())
    );
    assert_eq!(auth.config_file, None);
    sandbox.finish();
}

#[test]
fn legacy_agent_keys_keep_the_wi_0139_moved_key_hints() {
    let sandbox = crate::support::sandbox(
        "data::agent_catalogue::legacy_agent_keys_keep_the_wi_0139_moved_key_hints",
    );
    assert_eq!(
        fields::moved_hint("agent"),
        "; `agent` moved to `agents.default`"
    );
    assert_eq!(
        fields::moved_hint("default_agent"),
        "; `default_agent` moved to `agents.default`"
    );
    for key in ["agent", "default_agent"] {
        let error = ConfigFile::from_value(serde_json::json!({ (key): "gemini" }), FileKind::Repo)
            .unwrap_err()
            .to_string();
        assert!(error.contains("moved to `agents.default`"), "{error}");
    }
    sandbox.finish();
}

/// Design 8.3/8.4: every built-in has a definition and a Dockerfile; exactly
/// the agents with a supported Docker Sandboxes route have a v3 descriptor and
/// recipe (refused routes have none); `gemini` has nothing; and every
/// major-specific definition's tested version is the version its bundled
/// Dockerfile and owned SBX recipe install and assert, probed through the same
/// binary the launch runs.
#[test]
fn every_built_in_has_artifacts_and_only_opencode_requires_a_major() {
    use awman::data::templates::{
        agent_dockerfile_for, pinned_cli_version, sbx_apply_script, sbx_dockerfile_for,
        sbx_kit_template_for, sbx_launch_script,
    };
    use awman::engine::agent::sbx_contract::sbx_route;
    let sandbox = crate::support::sandbox(
        "data::agent_catalogue::every_built_in_has_artifacts_and_only_opencode_requires_a_major",
    );
    assert!(!sbx_apply_script().is_empty());
    assert!(!sbx_launch_script().is_empty());
    for agent in FIRST_PARTY_AGENT_NAMES {
        let def =
            first_party::first_party(agent).unwrap_or_else(|| panic!("{agent}: no definition"));
        assert!(
            agent_dockerfile_for(agent).is_some(),
            "{agent}: no Dockerfile"
        );
        let supported = sbx_route(agent).is_some_and(|route| route.is_supported());
        assert_eq!(
            sbx_kit_template_for(agent).is_some(),
            supported,
            "{agent}: v3 descriptor presence disagrees with the SBX matrix"
        );
        assert_eq!(
            sbx_dockerfile_for(agent).is_some(),
            supported,
            "{agent}: v3 recipe presence disagrees with the SBX matrix"
        );
        if let Some(requirement) = &def.cli_version {
            assert_eq!(
                Some(&requirement.binary),
                def.interactive_entrypoint.first(),
                "{agent}: probe binary is not the launched binary"
            );
            if let Some(recipe) = sbx_dockerfile_for(agent) {
                assert!(
                    recipe.contains(&format!(
                        "expected opencode {}.x",
                        requirement.required_major
                    )),
                    "{agent}: owned recipe does not assert the required major"
                );
            }
            // Major-specific agents are probed inside the image, never on the host.
            assert!(def.ping_argv.is_none(), "{agent}: host ping");
        }
    }
    for pinned in ["opencode", "opencode-v2"] {
        assert!(pinned_cli_version(pinned).is_some(), "{pinned}: unpinned");
    }
    assert!(first_party::first_party("gemini").is_none());
    assert!(agent_dockerfile_for("gemini").is_none());
    assert!(sbx_kit_template_for("gemini").is_none());
    assert!(sbx_dockerfile_for("gemini").is_none());
    assert!(pinned_cli_version("gemini").is_none());
    assert!(pinned_cli_version("cursor").is_none());
    sandbox.finish();
}
