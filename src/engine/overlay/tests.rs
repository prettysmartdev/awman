//! Tests for `engine::overlay` (WI 0114 F-51: moved out of the module
//! file, unchanged).

use super::*;
use crate::data::session::AgentName;

/// Set `AWMAN_CONFIG_HOME` to `home`, run `f`, then restore the previous value.
///
/// The lock and the restore both belong to `ConfigHomeGuard`: a mutex private
/// to this file would serialise these tests against each other only, and the
/// `dispatch` and `clean` tests mutate the very same process-wide variable.
fn with_awman_config_home<F, R>(home: &Path, f: F) -> R
where
    F: FnOnce() -> R,
{
    let _g = crate::data::config::env::ConfigHomeGuard::set(home);
    f()
}

fn make_engine(home: &Path) -> OverlayEngine {
    // Default test engine substitutes a no-op host-keychain reader so the
    // suite stays deterministic on dev macOS machines that may actually
    // have antigravity/claude credentials in their real keychain. Tests
    // that want to exercise the file-seed path inject their own provider
    // via `OverlayEngine::with_secret_files_provider`.
    OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
        .with_secret_files_provider(std::sync::Arc::new(|_| Vec::new()))
}

/// Build an engine with an explicit stub for file-form keychain artifacts.
fn make_engine_with_secrets(
    home: &Path,
    files: Vec<crate::engine::auth::keychain::AgentSecretFile>,
) -> OverlayEngine {
    OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
        .with_secret_files_provider(std::sync::Arc::new(move |_| files.clone()))
}

// ─── skill_overlays ───────────────────────────────────────────────────────

/// Create a temp dir, make `<dir>/skills/` exist, and return both.
fn make_home_with_skills() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let skills_canon = std::fs::canonicalize(&skills).unwrap_or(skills);
    (tmp, skills_canon)
}

// ─── antigravity agent_settings_overlays ─────────────────────────────────

#[test]
fn antigravity_settings_overlay_when_dir_exists() {
    let tmp = tempfile::tempdir().unwrap();
    // Create ~/.gemini/ with a config file so the overlay fires.
    let gemini_dir = tmp.path().join(".gemini");
    std::fs::create_dir_all(&gemini_dir).unwrap();
    std::fs::write(gemini_dir.join("settings.json"), r#"{"key":"val"}"#).unwrap();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("antigravity").unwrap();

    let overlays = engine
        .agent_settings_overlays_with(&agent, false, tmp.path(), None)
        .unwrap();

    assert_eq!(
        overlays.len(),
        1,
        "exactly one overlay expected when ~/.gemini exists; got {overlays:?}"
    );
    assert!(
        overlays[0]
            .container_path
            .to_string_lossy()
            .ends_with(".gemini"),
        "container_path must end with .gemini; got {:?}",
        overlays[0].container_path
    );
    // Must be a temp-dir copy, not the original.
    assert_ne!(
        overlays[0].host_path, gemini_dir,
        "host_path must be a temp-dir copy, not the original ~/.gemini"
    );
    // The copied content must be present.
    assert!(
        overlays[0].host_path.join("settings.json").exists(),
        "copied settings.json must exist in the temp-dir overlay"
    );
}

#[test]
fn antigravity_settings_overlay_empty_when_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    // Deliberately do NOT create ~/.gemini/.
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("antigravity").unwrap();

    let overlays = engine
        .agent_settings_overlays_with(&agent, false, tmp.path(), None)
        .unwrap();

    assert!(
        overlays.is_empty(),
        "overlay list must be empty when ~/.gemini does not exist and no \
             keychain credential is available; got {overlays:?}"
    );
}

#[test]
fn antigravity_settings_overlay_plants_keychain_token_file_alongside_host_copy() {
    use crate::engine::auth::keychain::AgentSecretFile;
    let tmp = tempfile::tempdir().unwrap();
    let gemini_dir = tmp.path().join(".gemini");
    std::fs::create_dir_all(&gemini_dir).unwrap();
    std::fs::write(gemini_dir.join("settings.json"), r#"{"model":"flash"}"#).unwrap();
    let token_payload = br#"{"token":{"access_token":"a","token_type":"Bearer",
            "refresh_token":"r","expiry":"2099-01-01T00:00:00Z"},"auth_method":"consumer"}"#;
    let engine = make_engine_with_secrets(
        tmp.path(),
        vec![AgentSecretFile {
            relative_path: std::path::PathBuf::from("antigravity-cli")
                .join("antigravity-oauth-token"),
            contents: token_payload.to_vec(),
            mode: 0o600,
        }],
    );
    let agent = AgentName::new("antigravity").unwrap();

    let overlays = engine
        .agent_settings_overlays_with(&agent, false, tmp.path(), None)
        .unwrap();

    assert_eq!(overlays.len(), 1, "expected one .gemini overlay");
    let staged = &overlays[0].host_path;
    let staged_token = staged.join("antigravity-cli/antigravity-oauth-token");
    assert!(
        staged_token.exists(),
        "staged dir must contain antigravity-cli/antigravity-oauth-token; \
             listed under {:?}",
        std::fs::read_dir(staged).map(|d| d
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect::<Vec<_>>()),
    );
    assert_eq!(
        std::fs::read(&staged_token).unwrap(),
        token_payload.to_vec(),
        "staged token contents must round-trip"
    );
    // Host copy is preserved alongside the planted secret.
    assert!(staged.join("settings.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&staged_token)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "token file must be mode 0600; got {mode:o}");
    }
}

#[test]
fn antigravity_settings_overlay_synthesizes_dir_when_only_keychain_present() {
    use crate::engine::auth::keychain::AgentSecretFile;
    let tmp = tempfile::tempdir().unwrap();
    // No host ~/.gemini, but keychain has a token. This mirrors the
    // first-time-container-user path where the host never ran agy
    // directly but did authorize it through some other route.
    let token_payload = br#"{"token":{"access_token":"a","token_type":"Bearer",
            "refresh_token":"r","expiry":"2099-01-01T00:00:00Z"},"auth_method":"consumer"}"#;
    let engine = make_engine_with_secrets(
        tmp.path(),
        vec![AgentSecretFile {
            relative_path: std::path::PathBuf::from("antigravity-cli")
                .join("antigravity-oauth-token"),
            contents: token_payload.to_vec(),
            mode: 0o600,
        }],
    );
    let agent = AgentName::new("antigravity").unwrap();

    let overlays = engine
        .agent_settings_overlays_with(&agent, false, tmp.path(), None)
        .unwrap();

    assert_eq!(overlays.len(), 1, "expected synthesized overlay");
    let staged_token = overlays[0]
        .host_path
        .join("antigravity-cli/antigravity-oauth-token");
    assert!(staged_token.exists(), "synthesized dir must hold the token");
}

// ─── antigravity skill_overlays ───────────────────────────────────────────

#[test]
fn skill_overlays_returns_single_ro_spec_for_claude() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1, "expected 1 OverlaySpec; got {specs:?}");
    assert_eq!(
        specs[0].host_path, skills_canon,
        "host path must be global skills dir"
    );
    assert_eq!(
        specs[0].permission,
        OverlayPermission::ReadOnly,
        "must be :ro"
    );
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.claude/commands"),
        "claude container path must contain /.claude/commands; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_codex() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("codex").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.codex/skills"),
        "codex container path must contain /.codex/skills; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_gemini() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("gemini").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.gemini/commands"),
        "gemini container path must contain /.gemini/commands; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_antigravity() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("antigravity").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .ends_with(".gemini/antigravity-cli/skills"),
        "antigravity container path must end with .gemini/antigravity-cli/skills; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_opencode() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("opencode").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.config/opencode/commands"),
        "opencode container path must contain /.config/opencode/commands; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_copilot() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("copilot").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.copilot/instructions"),
        "copilot container path must contain /.copilot/instructions; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_crush() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("crush").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.config/crush/commands"),
        "crush container path must contain /.config/crush/commands; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_single_ro_spec_for_cline() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("cline").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].host_path, skills_canon);
    assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.cline/skills"),
        "cline container path must contain /.cline/skills; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_returns_empty_when_skills_dir_does_not_exist() {
    let tmp = tempfile::tempdir().unwrap();
    // Deliberately do NOT create <tmp>/skills/.
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert!(
        specs.is_empty(),
        "must return empty vec when skills dir is absent; got {specs:?}"
    );
}

#[test]
fn skill_overlays_returns_empty_for_maki_no_error() {
    let (tmp, _) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("maki").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert!(
        specs.is_empty(),
        "maki must produce no skills mount; got {specs:?}"
    );
}

#[test]
fn skill_overlays_uses_container_home_override_when_set() {
    let (tmp, _) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let override_home = Some("/home/appuser".to_string());

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &override_home, Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .starts_with("/home/appuser/"),
        "container path must use the override home '/home/appuser'; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_defaults_to_root_when_no_dockerfile_present() {
    let (tmp, _) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, tmp.path())
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .starts_with("/root/"),
        "container path must default to /root/ when detect_container_home returns None; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn resolve_user_overlay_rejects_relative_container_path() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine(tmp.path());
    let spec = DirectorySpec {
        host: "/h".into(),
        container: "rel/path".into(),
        permission: OverlayPermission::ReadOnly,
    };
    let err = engine
        .resolve_user_overlay(&spec, Path::new("/"), None)
        .unwrap_err();
    assert!(matches!(err, EngineError::Other(_)));
}

#[test]
fn agent_settings_synthesized_when_no_files_present() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let out = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    assert!(
        out.iter().any(|o| o
            .container_path
            .to_string_lossy()
            .ends_with("/.claude.json")),
        "expected synthesized .claude.json overlay for first-time user, got {out:?}"
    );
}

#[test]
fn agent_settings_overlays_claude_config_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    // Create ~/.claude.json so the overlay resolver picks it up.
    let config_file = tmp.path().join(".claude.json");
    std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    // The overlay engine sanitizes the .claude.json file (strips
    // oauthAccount) and writes it to a temp path; we expect at least one
    // overlay mounting a file as `/root/.claude.json`.
    assert!(
        overlays.iter().any(|o| o
            .container_path
            .to_string_lossy()
            .ends_with("/.claude.json")),
        "expected overlay targeting /root/.claude.json, got {overlays:?}"
    );
}

#[test]
fn build_overlays_deduplicates_overlapping_host_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = tmp.path().join("shared");
    std::fs::create_dir_all(&host_dir).unwrap();
    let engine = make_engine(tmp.path());
    // Fake a session — overlay engine doesn't use it in this path.
    let session_tmp = tempfile::tempdir().unwrap();
    let session = {
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
        let resolver = StaticGitRootResolver::new(session_tmp.path());
        crate::data::session::Session::open(
            session_tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    };
    let request = OverlayRequest {
        directories: vec![
            DirectorySpec {
                host: host_dir.to_str().unwrap().to_string(),
                container: "/app/data".into(),
                permission: OverlayPermission::ReadWrite,
            },
            DirectorySpec {
                host: host_dir.to_str().unwrap().to_string(),
                container: "/app/data".into(),
                permission: OverlayPermission::ReadOnly,
            },
        ],
        include_all_skills: false,
        named_skills: vec![],
        agent: None,
        yolo: false,
        container_home: None,
        context_overlays: vec![],
        materialize_credentials: false,
    };
    let overlays = engine.build_overlays(&session, &request).unwrap();
    // The two entries sharing the same canonicalized host path must collapse.
    let matches: Vec<_> = overlays
        .iter()
        .filter(|o| o.host_path == host_dir.canonicalize().unwrap_or(host_dir.clone()))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "duplicate host path must be deduplicated, got {overlays:?}"
    );
}

#[test]
fn resolve_user_overlay_rejects_missing_container_path() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine(tmp.path());
    let spec = DirectorySpec {
        host: tmp.path().to_str().unwrap().to_string(),
        container: "relative/path".into(),
        permission: OverlayPermission::ReadOnly,
    };
    assert!(engine
        .resolve_user_overlay(&spec, Path::new("/"), None)
        .is_err());
}

#[test]
fn sanitize_claude_config_strips_oauth_account() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join(".claude.json");
    std::fs::write(
        &config_file,
        r#"{"model":"claude-sonnet-4-6","oauthAccount":{"token":"secret"}}"#,
    )
    .unwrap();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    // One overlay for the config file.
    let config_overlay = overlays
        .iter()
        .find(|o| {
            o.container_path
                .to_string_lossy()
                .ends_with("/.claude.json")
        })
        .expect("must have .claude.json overlay");
    // The sanitized file must not contain oauthAccount.
    let sanitized = std::fs::read_to_string(&config_overlay.host_path).unwrap();
    assert!(
        !sanitized.contains("oauthAccount"),
        "oauthAccount must be stripped from sanitized config: {sanitized}"
    );
    assert!(
        sanitized.contains("claude-sonnet-4-6"),
        "model field must be preserved: {sanitized}"
    );
}

#[test]
fn sanitize_claude_config_injects_workspace_trust_dialog_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join(".claude.json");
    std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    let config_overlay = overlays
        .iter()
        .find(|o| {
            o.container_path
                .to_string_lossy()
                .ends_with("/.claude.json")
        })
        .expect("must have .claude.json overlay");
    let sanitized = std::fs::read_to_string(&config_overlay.host_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
    assert_eq!(
        parsed["projects"]["/workspace"]["hasTrustDialogAccepted"],
        serde_json::Value::Bool(true),
        "trust dialog must be accepted for /workspace: {sanitized}"
    );
}

#[test]
fn sanitize_claude_settings_dir_filters_denylist_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // Create a denylisted entry.
    std::fs::create_dir_all(claude_dir.join("projects")).unwrap();
    // Create an allowed entry.
    std::fs::write(claude_dir.join("allowed.json"), r#"{"foo":"bar"}"#).unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    let sanitized_root = &dir_overlay.host_path;
    assert!(
        !sanitized_root.join("projects").exists(),
        "denylisted 'projects' dir must be excluded from sanitized overlay"
    );
    assert!(
        sanitized_root.join("allowed.json").exists(),
        "allowed file must be present in sanitized overlay"
    );
}

#[test]
fn sanitize_claude_settings_dir_suppresses_lsp_banner() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    let settings_path = dir_overlay.host_path.join("settings.json");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(
        settings["lspRecommendationDismissed"],
        serde_json::Value::Bool(true),
        "lspRecommendationDismissed must be true in sanitized settings"
    );
}

#[test]
fn sanitize_claude_settings_dir_injects_yolo_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine
        .agent_settings_overlays_with(&agent, true, tmp.path(), None)
        .unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    let settings_path = dir_overlay.host_path.join("settings.json");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(
        settings["permissionMode"],
        serde_json::Value::String("bypassPermissions".into()),
        "permissionMode must be bypassPermissions when yolo=true"
    );
}

#[test]
fn detect_container_home_finds_user_directive() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("Dockerfile.claude"),
        "FROM ubuntu:22.04\nRUN apt-get update\nUSER appuser\nWORKDIR /home/appuser\n",
    )
    .unwrap();

    let result = detect_container_home(tmp.path(), "claude", tmp.path());

    assert_eq!(
        result,
        Some("/home/appuser".to_string()),
        "detect_container_home must return /home/appuser for USER appuser"
    );
}

#[test]
fn detect_container_home_returns_none_when_no_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    let result = detect_container_home(tmp.path(), "claude", tmp.path());
    assert!(
        result.is_none(),
        "detect_container_home must return None when no Dockerfile found"
    );
}

#[test]
fn detect_container_home_returns_none_for_root_user() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("Dockerfile.claude"),
        "FROM ubuntu:22.04\nUSER root\n",
    )
    .unwrap();

    let result = detect_container_home(tmp.path(), "claude", tmp.path());

    assert!(
        result.is_none(),
        "detect_container_home must return None when USER is root"
    );
}

#[test]
fn detect_container_home_returns_none_for_user_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("Dockerfile.claude"),
        "FROM ubuntu:22.04\nUSER 0\n",
    )
    .unwrap();

    let result = detect_container_home(tmp.path(), "claude", tmp.path());

    assert!(
        result.is_none(),
        "detect_container_home must return None when USER is 0"
    );
}

// ─── detect_home_from_dockerfile ──────────────────────────────────────────

#[test]
fn detect_home_from_dockerfile_finds_non_root_user() {
    let tmp = tempfile::tempdir().unwrap();
    let df = tmp.path().join("Dockerfile.dev");
    std::fs::write(
        &df,
        "FROM debian:bookworm\nUSER awman\nWORKDIR /workspace\n",
    )
    .unwrap();
    assert_eq!(
        detect_home_from_dockerfile(&df),
        Some("/home/awman".to_string()),
    );
}

#[test]
fn detect_home_from_dockerfile_returns_none_for_root() {
    let tmp = tempfile::tempdir().unwrap();
    let df = tmp.path().join("Dockerfile.dev");
    std::fs::write(&df, "FROM debian:bookworm\nUSER root\n").unwrap();
    assert!(detect_home_from_dockerfile(&df).is_none());
}

#[test]
fn detect_home_from_dockerfile_returns_none_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(detect_home_from_dockerfile(&tmp.path().join("nonexistent")).is_none());
}

#[test]
fn detect_home_from_dockerfile_uses_last_non_root_user() {
    let tmp = tempfile::tempdir().unwrap();
    let df = tmp.path().join("Dockerfile");
    std::fs::write(&df, "FROM debian\nUSER builder\nRUN make\nUSER runner\n").unwrap();
    assert_eq!(
        detect_home_from_dockerfile(&df),
        Some("/home/runner".to_string()),
    );
}

#[test]
fn detect_home_from_dockerfile_resets_on_root_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let df = tmp.path().join("Dockerfile");
    std::fs::write(&df, "FROM debian\nUSER builder\nRUN make\nUSER root\n").unwrap();
    assert!(detect_home_from_dockerfile(&df).is_none());
}

// ─── resolve_user_overlay missing-host fail-fast ─────────────────────────

#[test]
fn resolve_user_overlay_errors_when_host_path_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-dir");
    let engine = make_engine(tmp.path());

    let spec = DirectorySpec {
        host: missing.to_str().unwrap().to_string(),
        container: "/workspace/data".into(),
        permission: OverlayPermission::ReadOnly,
    };

    let err = engine
        .resolve_user_overlay(&spec, Path::new("/"), None)
        .expect_err("missing host path must surface an EngineError");
    let msg = err.to_string();
    assert!(
        msg.contains("does not exist"),
        "error must say the host path doesn't exist; got: {msg}"
    );
    assert!(
        msg.contains("no-such-dir"),
        "error must name the offending host path; got: {msg}"
    );
}

#[test]
fn resolve_user_overlay_errors_when_ssh_dir_missing() {
    // The realistic `ssh()` case: ~/.ssh doesn't exist on the host.
    let tmp = tempfile::tempdir().unwrap();
    let ssh_dir = tmp.path().join(".ssh"); // deliberately not created
    let engine = make_engine(tmp.path());

    let spec = DirectorySpec {
        host: ssh_dir.to_str().unwrap().to_string(),
        container: "~/.ssh".into(),
        permission: OverlayPermission::ReadOnly,
    };

    let err = engine
        .resolve_user_overlay(&spec, Path::new("/"), None)
        .expect_err("missing ~/.ssh must surface an EngineError");
    assert!(
        err.to_string().contains("does not exist"),
        "ssh() with missing ~/.ssh must fail fast; got: {err}"
    );
}

// ─── resolve_user_overlay tilde expansion ────────────────────────────────

#[test]
fn resolve_user_overlay_expands_tilde_with_container_home() {
    let tmp = tempfile::tempdir().unwrap();
    let ssh_dir = tmp.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    let engine = make_engine(tmp.path());

    let spec = DirectorySpec {
        host: ssh_dir.to_str().unwrap().to_string(),
        container: "~/.ssh".to_string(),
        permission: OverlayPermission::ReadOnly,
    };

    let result = engine
        .resolve_user_overlay(&spec, Path::new("/"), Some("/home/alice"))
        .unwrap();
    assert_eq!(
        result.container_path,
        std::path::PathBuf::from("/home/alice/.ssh"),
        "~/.ssh must expand to /home/alice/.ssh when container_home is /home/alice"
    );
}

#[test]
fn resolve_user_overlay_expands_tilde_without_container_home_defaults_to_root() {
    let tmp = tempfile::tempdir().unwrap();
    let ssh_dir = tmp.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    let engine = make_engine(tmp.path());

    let spec = DirectorySpec {
        host: ssh_dir.to_str().unwrap().to_string(),
        container: "~/.ssh".to_string(),
        permission: OverlayPermission::ReadOnly,
    };

    let result = engine
        .resolve_user_overlay(&spec, Path::new("/"), None)
        .unwrap();
    assert_eq!(
        result.container_path,
        std::path::PathBuf::from("/root/.ssh"),
        "~/.ssh must default to /root/.ssh when container_home is None"
    );
}

// ─── skill_overlays: named skills ─────────────────────────────────────────

#[test]
fn skill_overlays_named_only_emits_that_skill() {
    let (tmp, _) = make_home_with_skills();
    // Create a named skill directory inside the global skills dir.
    let lint_dir = tmp.path().join("skills").join("lint");
    std::fs::create_dir_all(&lint_dir).unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, false, &["lint".to_string()], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(
        specs.len(),
        1,
        "only the named skill must be emitted; got {specs:?}"
    );
    assert!(
        specs[0].container_path.to_string_lossy().ends_with("/lint"),
        "container path must include the skill name 'lint'; got {:?}",
        specs[0].container_path
    );
    assert_eq!(
        specs[0].permission,
        OverlayPermission::ReadOnly,
        "named skill must be mounted read-only"
    );
}

#[test]
fn skill_overlays_nonexistent_named_skill_returns_engine_error() {
    let (tmp, _) = make_home_with_skills();
    // Deliberately do NOT create a "nonexistent" skill directory.
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let result = with_awman_config_home(tmp.path(), || {
        engine.skill_overlays(
            &agent,
            false,
            &["nonexistent".to_string()],
            &None,
            Path::new("/"),
        )
    });

    assert!(
        result.is_err(),
        "nonexistent named skill must return EngineError"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("nonexistent"),
        "error must name the missing skill; got: {msg}"
    );
}

// ─── skill_overlays: pulled libraries (WI-0103) ──────────────────────────

/// Seed a pulled library at `<home>/skills/.library/<slug>/` with the given
/// `subdir` and skill names (each a `<skill>/SKILL.md`), plus `.awman.json`.
fn seed_library(home: &Path, slug: &str, subdir: &str, skills: &[&str]) {
    let lib_dir = home.join("skills").join(".library").join(slug);
    for skill in skills {
        let skill_dir = lib_dir.join(subdir).join(skill);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), format!("# {skill}")).unwrap();
    }
    crate::data::fs::skill_library::write_library_meta(
        &lib_dir,
        &crate::data::fs::skill_library::SkillLibraryMeta {
            source: format!("https://github.com/someone/{slug}.git"),
            owner: "someone".to_string(),
            repo: slug.to_string(),
            subdir: subdir.to_string(),
        },
    )
    .unwrap();
}

#[test]
fn skill_named_plain_skill_wins_over_same_named_library() {
    let (tmp, _) = make_home_with_skills();
    // A hand-authored plain skill named 'superpowers'.
    let plain = tmp.path().join("skills").join("superpowers");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("SKILL.md"), "# plain").unwrap();
    // A pulled library ALSO named 'superpowers'.
    seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(
                &agent,
                false,
                &["superpowers".to_string()],
                &None,
                Path::new("/"),
            )
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].host_path,
        std::fs::canonicalize(&plain).unwrap(),
        "the plain skill must win over a same-named pulled library"
    );
}

#[test]
fn skill_named_whole_library_mounts_subdir_at_library_container_path() {
    let (tmp, _) = make_home_with_skills();
    seed_library(
        tmp.path(),
        "superpowers",
        "skills",
        &["brainstorming", "debugging"],
    );

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(
                &agent,
                false,
                &["superpowers".to_string()],
                &None,
                Path::new("/"),
            )
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    let expected_host = std::fs::canonicalize(
        tmp.path()
            .join("skills")
            .join(".library")
            .join("superpowers")
            .join("skills"),
    )
    .unwrap();
    assert_eq!(
        specs[0].host_path, expected_host,
        "whole-library mount must point at .library/<slug>/<subdir>"
    );
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .ends_with("/superpowers"),
        "container path must namespace the whole library under its name; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_named_single_library_skill_mounts_that_skill_dir() {
    let (tmp, _) = make_home_with_skills();
    seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let specs = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(
                &agent,
                false,
                &["superpowers/brainstorming".to_string()],
                &None,
                Path::new("/"),
            )
            .unwrap()
    });

    assert_eq!(specs.len(), 1);
    let expected_host = std::fs::canonicalize(
        tmp.path()
            .join("skills")
            .join(".library")
            .join("superpowers")
            .join("skills")
            .join("brainstorming"),
    )
    .unwrap();
    assert_eq!(
        specs[0].host_path, expected_host,
        "single-skill mount must point at the individual skill directory"
    );
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .ends_with("/superpowers/brainstorming"),
        "container path must preserve the library namespace; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_named_library_present_but_skill_missing_gives_distinct_error() {
    let (tmp, _) = make_home_with_skills();
    seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let result = with_awman_config_home(tmp.path(), || {
        engine.skill_overlays(
            &agent,
            false,
            &["superpowers/ghost".to_string()],
            &None,
            Path::new("/"),
        )
    });

    let msg = result
        .expect_err("a missing skill in a present library must error")
        .to_string();
    assert!(
        msg.contains("not found in library")
            && msg.contains("superpowers")
            && msg.contains("ghost"),
        "error must name both the library and the missing skill; got: {msg}"
    );
}

/// A skill is a directory holding a `SKILL.md`. An arbitrary directory
/// inside a library's subdir must not be mountable just because it exists
/// (WI-0103 remediation).
#[test]
fn skill_named_library_dir_without_skill_md_is_rejected() {
    let (tmp, _) = make_home_with_skills();
    seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);
    // A directory inside the library's subdir with no SKILL.md.
    let not_a_skill = tmp
        .path()
        .join("skills")
        .join(".library")
        .join("superpowers")
        .join("skills")
        .join("not-a-skill");
    std::fs::create_dir_all(&not_a_skill).unwrap();
    std::fs::write(not_a_skill.join("README.md"), "no skill here").unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let result = with_awman_config_home(tmp.path(), || {
        engine.skill_overlays(
            &agent,
            false,
            &["superpowers/not-a-skill".to_string()],
            &None,
            Path::new("/"),
        )
    });

    let msg = result
        .expect_err("a directory without SKILL.md is not a skill")
        .to_string();
    assert!(
        msg.contains("not-a-skill") && msg.contains("superpowers") && msg.contains("SKILL.md"),
        "error must name the library, the missing skill, and SKILL.md; got: {msg}"
    );
}

/// The parser rejects traversal segments, but named skills also arrive from
/// config files and the API, so `skill_overlays` re-checks containment
/// rather than joining `..` onto a host path (WI-0103 remediation).
#[test]
fn skill_named_traversal_segments_are_rejected_by_the_engine() {
    let (tmp, _) = make_home_with_skills();
    seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    for bad in ["superpowers/..", "superpowers/", "..", "../superpowers"] {
        let result = with_awman_config_home(tmp.path(), || {
            engine.skill_overlays(&agent, false, &[bad.to_string()], &None, Path::new("/"))
        });
        let msg = match result {
            Ok(specs) => panic!("'{bad}' must be rejected, but produced specs: {specs:?}"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("invalid path segment"),
            "'{bad}' must be rejected as an invalid segment; got: {msg}"
        );
    }
}

#[test]
fn skill_named_neither_plain_nor_library_names_both_locations() {
    let (tmp, _) = make_home_with_skills();
    // Neither a plain skill nor a library called 'ghost' exists.
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let result = with_awman_config_home(tmp.path(), || {
        engine.skill_overlays(&agent, false, &["ghost".to_string()], &None, Path::new("/"))
    });

    let msg = result
        .expect_err("an unresolvable name must error")
        .to_string();
    assert!(
        msg.contains("ghost"),
        "error must name the skill; got: {msg}"
    );
    assert!(
        msg.contains(&tmp.path().join("skills").display().to_string()),
        "error must name the global skills dir; got: {msg}"
    );
    assert!(
        msg.contains(".library"),
        "error must name the .library location; got: {msg}"
    );
}

#[test]
fn skill_star_is_identical_with_and_without_populated_library() {
    let (tmp, skills_canon) = make_home_with_skills();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let before = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    // Populate `.library/` — skill(*) must be entirely unaffected by it.
    seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

    let after = with_awman_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, true, &[], &None, Path::new("/"))
            .unwrap()
    });

    assert_eq!(
        before, after,
        "skill(*) must emit an identical OverlaySpec list regardless of .library/"
    );
    assert_eq!(before.len(), 1, "skill(*) is a single mount");
    assert_eq!(
        before[0].host_path, skills_canon,
        "skill(*) still mounts the global skills dir as-is"
    );
}

// ─── build_overlays: least-permissive-wins ────────────────────────────────

#[test]
fn build_overlays_least_permissive_wins_for_same_host_path() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = tmp.path().join("shared");
    std::fs::create_dir_all(&host_dir).unwrap();
    let engine = make_engine(tmp.path());

    let session_tmp = tempfile::tempdir().unwrap();
    let session = {
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
        let resolver = StaticGitRootResolver::new(session_tmp.path());
        crate::data::session::Session::open(
            session_tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    };

    let request = OverlayRequest {
        directories: vec![
            DirectorySpec {
                host: host_dir.to_str().unwrap().to_string(),
                container: "/app/data".into(),
                permission: OverlayPermission::ReadOnly,
            },
            DirectorySpec {
                host: host_dir.to_str().unwrap().to_string(),
                container: "/app/data".into(),
                permission: OverlayPermission::ReadWrite,
            },
        ],
        include_all_skills: false,
        named_skills: vec![],
        agent: None,
        yolo: false,
        container_home: None,
        context_overlays: vec![],
        materialize_credentials: false,
    };

    let overlays = engine.build_overlays(&session, &request).unwrap();
    let host_canon = host_dir.canonicalize().unwrap_or_else(|_| host_dir.clone());
    let matched: Vec<_> = overlays
        .iter()
        .filter(|o| o.host_path == host_canon)
        .collect();
    assert_eq!(
        matched.len(),
        1,
        "same host path must deduplicate; got {overlays:?}"
    );
    assert_eq!(
        matched[0].permission,
        OverlayPermission::ReadOnly,
        "ReadOnly must win over ReadWrite (least-permissive-wins); got {:?}",
        matched[0].permission
    );
}

#[test]
fn sanitize_claude_settings_dir_no_yolo_when_false() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine
        .agent_settings_overlays_with(&agent, false, tmp.path(), None)
        .unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    let settings_path = dir_overlay.host_path.join("settings.json");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert!(
        settings.get("permissionMode").is_none(),
        "permissionMode must NOT be set when yolo=false"
    );
}

// ─── WI-0087: context overlay mounts ──────────────────────────────────────

#[test]
fn build_overlays_context_overlay_produces_expected_container_path() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine(tmp.path());
    let session_tmp = tempfile::tempdir().unwrap();
    let session = crate::data::session::Session::for_tests(session_tmp.path());

    // Host path for the context dir (doesn't need to exist for context overlays).
    let ctx_host = tmp.path().join("context").join("global");

    let request = OverlayRequest {
        context_overlays: vec![ContextOverlay {
            scope: ContextScope::Global,
            host_path: ctx_host,
            container_path: std::path::PathBuf::from("/awman/context/global"),
            permission: crate::engine::container::options::OverlayPermission::ReadWrite,
        }],
        ..Default::default()
    };

    let specs = engine.build_overlays(&session, &request).unwrap();
    let ctx_spec = specs
        .iter()
        .find(|s| s.container_path == std::path::Path::new("/awman/context/global"));
    assert!(
        ctx_spec.is_some(),
        "build_overlays must produce an OverlaySpec with container path \
             /awman/context/global; got {specs:?}"
    );
    assert_eq!(
        ctx_spec.unwrap().permission,
        crate::engine::container::options::OverlayPermission::ReadWrite,
    );
}

#[test]
fn build_overlays_context_overlay_repo_scope_container_path() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine(tmp.path());
    let session_tmp = tempfile::tempdir().unwrap();
    let session = crate::data::session::Session::for_tests(session_tmp.path());

    let ctx_host = tmp
        .path()
        .join("context")
        .join("repo")
        .join("org")
        .join("myrepo");

    let request = OverlayRequest {
        context_overlays: vec![ContextOverlay {
            scope: ContextScope::Repo,
            host_path: ctx_host,
            container_path: std::path::PathBuf::from("/awman/context/repo"),
            permission: crate::engine::container::options::OverlayPermission::ReadOnly,
        }],
        ..Default::default()
    };

    let specs = engine.build_overlays(&session, &request).unwrap();
    let ctx_spec = specs
        .iter()
        .find(|s| s.container_path == std::path::Path::new("/awman/context/repo"));
    assert!(
        ctx_spec.is_some(),
        "build_overlays must produce an OverlaySpec with container path \
             /awman/context/repo; got {specs:?}"
    );
    assert_eq!(
        ctx_spec.unwrap().permission,
        crate::engine::container::options::OverlayPermission::ReadOnly,
    );
}

#[test]
fn build_overlays_context_overlay_collides_with_user_dir_most_restrictive_wins() {
    // A context overlay (ReadOnly) sharing a host path with a user dir(ReadWrite)
    // must merge to ReadOnly.
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine(tmp.path());
    let session_tmp = tempfile::tempdir().unwrap();
    let session = crate::data::session::Session::for_tests(session_tmp.path());

    // Shared host directory (must exist for the user dir overlay path check).
    let shared_host = tmp.path().join("shared");
    std::fs::create_dir_all(&shared_host).unwrap();
    let shared_host_str = shared_host.to_str().unwrap().to_string();

    let request = OverlayRequest {
        directories: vec![DirectorySpec {
            host: shared_host_str,
            container: "/app/data".to_string(),
            permission: crate::engine::container::options::OverlayPermission::ReadWrite,
        }],
        context_overlays: vec![ContextOverlay {
            scope: ContextScope::Global,
            host_path: shared_host.clone(),
            container_path: std::path::PathBuf::from("/awman/context/global"),
            permission: crate::engine::container::options::OverlayPermission::ReadOnly,
        }],
        ..Default::default()
    };

    let specs = engine.build_overlays(&session, &request).unwrap();

    // Both map to the same canonicalized host path, so they must merge to one entry.
    let shared_canon = shared_host
        .canonicalize()
        .unwrap_or_else(|_| shared_host.clone());
    let matching: Vec<_> = specs
        .iter()
        .filter(|s| s.host_path == shared_canon)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "user dir + context overlay with same host path must merge to one entry; \
             got {specs:?}"
    );
    assert_eq!(
        matching[0].permission,
        crate::engine::container::options::OverlayPermission::ReadOnly,
        "ReadOnly must win over ReadWrite (most-restrictive); got {:?}",
        matching[0].permission
    );
}

// ─── WI-0107: refresh-token denylist + credential-file staging ───────────

/// Build an engine with an explicit stub for the refreshable-credential
/// source. `None` means "no credential to plant" — the default for tests
/// that don't exercise materialization. Never touches a developer's real
/// host credential file or keychain.
fn make_engine_with_credential(home: &Path, file: Option<CredentialFile>) -> OverlayEngine {
    OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
        .with_secret_files_provider(std::sync::Arc::new(|_| Vec::new()))
        .with_credential_provider(std::sync::Arc::new(move |_| file.clone()))
}

/// INV-2 checkable: a host `.credentials.json` (always present on Linux,
/// and on macOS whenever a keychain write failed) must never be copied
/// into the staged overlay, even though the source dir is otherwise
/// mirrored verbatim.
#[test]
fn sanitize_claude_settings_dir_denylists_host_credentials_file() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-HOST","refreshToken":"SENTINEL-REFRESH-MUST-NOT-LEAK"}}"#,
        )
        .unwrap();
    std::fs::write(claude_dir.join("allowed.json"), r#"{"foo":"bar"}"#).unwrap();

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    let staged_credentials_file = dir_overlay.host_path.join(".credentials.json");
    assert!(
        !staged_credentials_file.exists(),
        "host .credentials.json must never be copied into the staged overlay"
    );
    assert!(
        dir_overlay.host_path.join("allowed.json").exists(),
        "non-denylisted files must still be mirrored"
    );
}

/// BLOCKING-1 (INV-2): the denylist must not be bypassable by a symlink
/// alias, a case variant, a nested copy, or a hard link to the host
/// `.credentials.json`. The refresh-token sentinel must appear in NO file of
/// the staged tree, at any depth.
#[test]
fn sanitize_claude_settings_dir_denies_symlink_case_and_nested_credential_variants() {
    const SENTINEL: &str = "SENTINEL-REFRESH-MUST-NOT-LEAK";
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let host_credential = claude_dir.join(".credentials.json");
    std::fs::write(
        &host_credential,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat-HOST","refreshToken":"{SENTINEL}"}}}}"#
        ),
    )
    .unwrap();
    // Case variant of the credential filename.
    std::fs::write(
        claude_dir.join(".Credentials.json"),
        format!(r#"{{"refreshToken":"{SENTINEL}"}}"#),
    )
    .unwrap();
    // Nested copy in a non-denylisted subdirectory.
    let nested = claude_dir.join("safe-subdir");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join(".credentials.json"),
        format!(r#"{{"refreshToken":"{SENTINEL}"}}"#),
    )
    .unwrap();
    // A benign file that MUST still be mirrored.
    std::fs::write(claude_dir.join("allowed.json"), r#"{"foo":"bar"}"#).unwrap();
    #[cfg(unix)]
    {
        // Symlink alias pointing straight at the host credential, plus a
        // hard link under an innocuous name.
        std::os::unix::fs::symlink(&host_credential, claude_dir.join("innocent-cache.json"))
            .unwrap();
        std::fs::hard_link(&host_credential, claude_dir.join("backup.json")).unwrap();
    }

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    // Walk the whole staged tree; no file may contain the sentinel.
    fn assert_no_sentinel(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            assert!(
                !meta.file_type().is_symlink(),
                "staged overlay must contain no symlinks; found {}",
                path.display()
            );
            if meta.is_dir() {
                assert_no_sentinel(&path);
            } else if let Ok(contents) = std::fs::read_to_string(&path) {
                assert!(
                    !contents.contains(SENTINEL),
                    "refresh-token sentinel leaked into staged file {}",
                    path.display()
                );
            }
        }
    }
    assert_no_sentinel(&dir_overlay.host_path);
    assert!(
        dir_overlay.host_path.join("allowed.json").exists(),
        "non-credential files must still be mirrored"
    );
}

/// The awman-authored, refresh-token-free credential file is planted in
/// the staged dir when `materialize_credentials` is requested and the
/// (stubbed) descriptor has a credential to offer — even though the host
/// dir's own `.credentials.json` was denylisted above.
#[test]
fn materialize_credentials_plants_awman_authored_file_present_and_0600() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // The host copy still carries a real (sentinel) refresh token; it must
    // be denylisted while the awman-authored file (below) lands instead.
    std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-HOST","refreshToken":"SENTINEL-REFRESH-MUST-NOT-LEAK"}}"#,
        )
        .unwrap();

    let materialized = CredentialFile {
        relative_path: PathBuf::from(".credentials.json"),
        contents: br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-AWMAN"}}"#.to_vec(),
        mode: 0o600,
    };
    let engine = make_engine_with_credential(tmp.path(), Some(materialized.clone()));
    let agent = AgentName::new("claude").unwrap();
    let session = crate::data::session::Session::for_tests(tmp.path());

    let request = OverlayRequest {
        agent: Some(agent),
        materialize_credentials: true,
        ..Default::default()
    };
    let (overlays, staged) = engine
        .build_overlays_with_credentials(&session, &request)
        .unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    let planted_path = dir_overlay.host_path.join(".credentials.json");
    let contents = std::fs::read_to_string(&planted_path).expect("planted file must exist");
    assert!(contents.contains("sk-ant-oat-AWMAN"));
    assert!(
        !contents.contains("SENTINEL-REFRESH-MUST-NOT-LEAK"),
        "the awman-authored file must have replaced the host's, not merged with it"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&planted_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "staged credential file must be mode 0600");
    }

    assert_eq!(staged.len(), 1, "exactly one credential file was planted");
    assert_eq!(staged[0].path, planted_path);
    assert_eq!(staged[0].root, dir_overlay.host_path);
}

/// Without `materialize_credentials`, no credential file is planted even
/// though the (stubbed) descriptor has one to offer — the flag is the
/// only gate.
#[test]
fn materialize_credentials_false_plants_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();

    let materialized = CredentialFile {
        relative_path: PathBuf::from(".credentials.json"),
        contents: br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-AWMAN"}}"#.to_vec(),
        mode: 0o600,
    };
    let engine = make_engine_with_credential(tmp.path(), Some(materialized));
    let agent = AgentName::new("claude").unwrap();
    let session = crate::data::session::Session::for_tests(tmp.path());

    let request = OverlayRequest {
        agent: Some(agent),
        materialize_credentials: false,
        ..Default::default()
    };
    let (overlays, staged) = engine
        .build_overlays_with_credentials(&session, &request)
        .unwrap();
    let dir_overlay = overlays
        .iter()
        .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
        .expect("must have .claude dir overlay");

    assert!(
        !dir_overlay.host_path.join(".credentials.json").exists(),
        "no credential file must be planted when materialize_credentials is false"
    );
    assert!(staged.is_empty());
}

// ─── write_credential_file_atomic ─────────────────────────────────────────

/// INV-7 (path check, the third independent defense): a staged root that
/// no longer exists is a skip (`Ok(false)`), never an `Err` — the monitor
/// treats this as a normal dropped-lease race.
#[test]
fn write_credential_file_atomic_missing_staged_root_is_a_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let missing_root = tmp.path().join("never-created");
    let file = CredentialFile {
        relative_path: PathBuf::from(".credentials.json"),
        contents: b"irrelevant".to_vec(),
        mode: 0o600,
    };
    let result = write_credential_file_atomic(&missing_root, &file);
    assert!(
        matches!(result, Ok(false)),
        "a missing staged root must be a skip, not an error: {result:?}"
    );
}

/// INV-3: an injected writer failure must never leave the target
/// truncated or partially written — the previous complete content stays
/// exactly as it was. Simulated by making the staged directory
/// unwritable, so the temp file used for the atomic rename can never even
/// be created.
#[test]
#[cfg(unix)]
fn write_credential_file_atomic_failure_leaves_target_byte_identical() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let staged_root = tmp.path().join("staged");
    std::fs::create_dir_all(&staged_root).unwrap();
    let target = staged_root.join(".credentials.json");
    let original = b"ORIGINAL-COMPLETE-CREDENTIAL".to_vec();
    std::fs::write(&target, &original).unwrap();

    // Revoke write permission on the staged dir so NamedTempFile::new_in
    // (the writer this call injects) fails before touching the target.
    let mut perms = std::fs::metadata(&staged_root).unwrap().permissions();
    perms.set_mode(0o500);
    std::fs::set_permissions(&staged_root, perms).unwrap();

    let file = CredentialFile {
        relative_path: PathBuf::from(".credentials.json"),
        contents: b"NEW-CONTENT-MUST-NOT-LAND".to_vec(),
        mode: 0o600,
    };
    let result = write_credential_file_atomic(&staged_root, &file);

    // Restore permissions so the TempDir can clean itself up.
    let mut restore = std::fs::metadata(&staged_root).unwrap().permissions();
    restore.set_mode(0o700);
    std::fs::set_permissions(&staged_root, restore).unwrap();

    assert!(
        result.is_err(),
        "the injected writer failure must surface as Err, not a silent skip"
    );
    let remaining = std::fs::read(&target).unwrap();
    assert_eq!(
        remaining, original,
        "target must retain its previous complete content on write failure, \
             never a truncated or partial file"
    );
}
