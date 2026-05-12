//! OverlayEngine structural and integration tests (WI 0073 / WI 0075).
//!
//! Tests that need Docker have "docker" in their name and are skipped by
//! `make test-fast`. All other tests run under `make test-fast`.

use amux::data::fs::auth_paths::AuthPathResolver;
use amux::data::session::AgentName;
use amux::engine::container::options::OverlayPermission;
use amux::engine::overlay::{DirectorySpec, OverlayEngine, OverlayRequest, CLAUDE_DENYLIST};

/// Serialises tests that write to `AMUX_CONFIG_HOME` (a process-global env var).
static AMUX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set `AMUX_CONFIG_HOME` to `home`, run `f`, then restore the previous value.
fn with_amux_config_home<F, R>(home: &std::path::Path, f: F) -> R
where
    F: FnOnce() -> R,
{
    let _g = AMUX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("AMUX_CONFIG_HOME").ok();
    std::env::set_var("AMUX_CONFIG_HOME", home.to_str().unwrap());
    let result = f();
    match prev {
        Some(v) => std::env::set_var("AMUX_CONFIG_HOME", v),
        None => std::env::remove_var("AMUX_CONFIG_HOME"),
    }
    result
}

fn make_engine(home: &std::path::Path) -> OverlayEngine {
    OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
}

// ─── CLAUDE_DENYLIST integrity ────────────────────────────────────────────────

#[test]
fn claude_denylist_contains_projects() {
    assert!(CLAUDE_DENYLIST.contains(&"projects"));
}

#[test]
fn claude_denylist_contains_sessions() {
    assert!(CLAUDE_DENYLIST.contains(&"sessions"));
}

#[test]
fn claude_denylist_contains_history_jsonl() {
    assert!(CLAUDE_DENYLIST.contains(&"history.jsonl"));
}

#[test]
fn claude_denylist_contains_telemetry() {
    assert!(CLAUDE_DENYLIST.contains(&"telemetry"));
}

#[test]
fn claude_denylist_does_not_contain_settings_json() {
    // settings.json must NOT be on the denylist — it is the overlay file.
    assert!(!CLAUDE_DENYLIST.contains(&"settings.json"));
}

#[test]
fn claude_denylist_is_non_empty() {
    assert!(!CLAUDE_DENYLIST.is_empty());
}

// ─── OverlayRequest defaults ──────────────────────────────────────────────────

#[test]
fn overlay_request_default_has_no_agent() {
    let req = OverlayRequest::default();
    assert!(req.agent.is_none());
    assert!(!req.yolo);
    assert!(req.directories.is_empty());
}

// ─── DirectorySpec construction ───────────────────────────────────────────────

#[test]
fn directory_spec_fields_accessible() {
    let spec = DirectorySpec {
        host: "/host/path".to_string(),
        container: "/container/path".to_string(),
        permission: OverlayPermission::ReadOnly,
    };
    assert_eq!(spec.host, "/host/path");
    assert_eq!(spec.container, "/container/path");
    assert_eq!(spec.permission, OverlayPermission::ReadOnly);
}

#[test]
fn overlay_permission_variants_distinct() {
    assert_ne!(OverlayPermission::ReadOnly, OverlayPermission::ReadWrite);
}

// ─── skill_overlays integration tests ────────────────────────────────────────

#[test]
fn skill_overlays_claude_ro_mount_when_skills_dir_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let skills_canon = std::fs::canonicalize(&skills).unwrap_or(skills);

    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let specs = with_amux_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, &None, std::path::Path::new("/"))
            .unwrap()
    });

    assert_eq!(specs.len(), 1, "expected 1 OverlaySpec; got {specs:?}");
    assert_eq!(
        specs[0].host_path, skills_canon,
        "host path must equal global skills dir"
    );
    assert_eq!(
        specs[0].permission,
        OverlayPermission::ReadOnly,
        "skills mount must be read-only"
    );
    assert!(
        specs[0]
            .container_path
            .to_string_lossy()
            .contains("/.claude/commands"),
        "container path must target /.claude/commands; got {:?}",
        specs[0].container_path
    );
}

#[test]
fn skill_overlays_empty_when_global_skills_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    // Deliberately do NOT create <tmp>/skills/.
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let specs = with_amux_config_home(tmp.path(), || {
        engine
            .skill_overlays(&agent, &None, std::path::Path::new("/"))
            .unwrap()
    });

    assert!(
        specs.is_empty(),
        "must return empty vec (no error) when skills dir is absent; got {specs:?}"
    );
}

#[test]
fn skill_overlays_empty_for_maki_agent_no_error() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let engine = make_engine(tmp.path());
    let agent = AgentName::new("maki").unwrap();

    // Must return Ok(vec![]) — not an error — even though maki has no known skills dir.
    let result = with_amux_config_home(tmp.path(), || {
        engine.skill_overlays(&agent, &None, std::path::Path::new("/"))
    });

    assert!(
        result.is_ok(),
        "maki must not produce an error; got {result:?}"
    );
    assert!(result.unwrap().is_empty(), "maki must produce no mount");
}

#[test]
fn build_overlays_includes_skills_mount_when_include_skills_true() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let skills_canon = std::fs::canonicalize(&skills).unwrap_or(skills.clone());

    let engine = make_engine(tmp.path());
    let session_tmp = tempfile::tempdir().unwrap();
    let session = {
        use amux::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
        let resolver = StaticGitRootResolver::new(session_tmp.path());
        Session::open(
            session_tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    };
    let request = OverlayRequest {
        include_skills: true,
        agent: Some(AgentName::new("claude").unwrap()),
        ..Default::default()
    };

    let overlays = with_amux_config_home(tmp.path(), || {
        engine.build_overlays(&session, &request).unwrap()
    });

    let skills_mount = overlays.iter().find(|o| o.host_path == skills_canon);
    assert!(
        skills_mount.is_some(),
        "build_overlays must include skills mount when include_skills=true; got {overlays:?}"
    );
    assert_eq!(
        skills_mount.unwrap().permission,
        OverlayPermission::ReadOnly,
        "skills mount must be :ro"
    );
}

#[test]
fn build_overlays_skills_and_dir_overlay_both_present() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let skills_canon = std::fs::canonicalize(&skills).unwrap_or(skills.clone());

    let host_dir = tempfile::tempdir().unwrap();
    let host_canon =
        std::fs::canonicalize(host_dir.path()).unwrap_or(host_dir.path().to_path_buf());

    let engine = make_engine(tmp.path());
    let session_tmp = tempfile::tempdir().unwrap();
    let session = {
        use amux::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
        let resolver = StaticGitRootResolver::new(session_tmp.path());
        Session::open(
            session_tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    };
    let request = OverlayRequest {
        include_skills: true,
        agent: Some(AgentName::new("claude").unwrap()),
        directories: vec![DirectorySpec {
            host: host_dir.path().to_string_lossy().into_owned(),
            container: "/mnt/extra".into(),
            permission: OverlayPermission::ReadOnly,
        }],
        ..Default::default()
    };

    let overlays = with_amux_config_home(tmp.path(), || {
        engine.build_overlays(&session, &request).unwrap()
    });

    assert!(
        overlays.iter().any(|o| o.host_path == skills_canon),
        "skills mount must be present; got {overlays:?}"
    );
    assert!(
        overlays.iter().any(|o| o.host_path == host_canon),
        "dir() overlay must also be present; got {overlays:?}"
    );
}
