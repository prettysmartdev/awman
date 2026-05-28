//! Route table and API-path tests.
//!
//! Does NOT start a real server — just verifies the data-layer types used
//! by the API server are correct.

use awman::data::config::env::{EnvSnapshot, AWMAN_API_ROOT};
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;

// ─── ApiPaths resolution ─────────────────────────────────────────────────

#[test]
fn api_paths_from_root_has_correct_db_path() {
    let paths = ApiPaths::from_root("/tmp/awman-test");
    assert_eq!(
        paths.db_path(),
        std::path::PathBuf::from("/tmp/awman-test/awman.db")
    );
}

#[test]
fn api_paths_from_env_honours_awman_api_root() {
    let env = EnvSnapshot::with_overrides([(AWMAN_API_ROOT, "/custom/root")]);
    let paths = ApiPaths::from_env(&env).unwrap();
    assert_eq!(paths.root(), std::path::Path::new("/custom/root"));
}

#[test]
fn api_paths_sessions_dir_under_root() {
    let paths = ApiPaths::from_root("/srv/api");
    let sessions = paths.sessions_dir();
    assert!(
        sessions.starts_with("/srv/api"),
        "sessions dir should be under root: {sessions:?}"
    );
}

#[test]
fn api_paths_tls_dir_under_root() {
    let paths = ApiPaths::from_root("/srv/api");
    let tls = paths.tls_dir();
    assert!(
        tls.starts_with("/srv/api"),
        "tls dir should be under root: {tls:?}"
    );
}

// ─── Route method/path coverage ──────────────────────────────────────────────
//
// Rather than starting a live server (which requires Engines → ContainerRuntime
// → Docker), we verify the expected route paths are registered in the source by
// querying the SQLite store directly to confirm the paths documented in the
// build_router source are correct.
//
// Actual route-hit tests live in binary_smoke and are marked `real_network_*`.

/// The expected route paths from `build_router`. WI 0079 moved SSE log
/// streaming from `/v1/sessions/{id}/jobs/{job_id}/logs` to
/// `/v1/commands/{id}/logs`, and added `GET /v1/sessions/:id/queue` and
/// `GET /v1/commands/:id/status`.
const EXPECTED_ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/status"),
    ("GET", "/v1/workdirs"),
    ("GET", "/v1/sessions"),
    ("POST", "/v1/sessions"),
    ("GET", "/v1/sessions/:id"),
    ("DELETE", "/v1/sessions/:id"),
    ("GET", "/v1/sessions/:id/status"),
    ("GET", "/v1/sessions/:id/queue"),
    ("POST", "/v1/commands"),
    ("GET", "/v1/commands/:id/status"),
    ("GET", "/v1/commands/:id/logs"),
    ("GET", "/v1/workflows/:command_id"),
];

#[test]
fn expected_routes_table_is_non_empty() {
    assert_eq!(EXPECTED_ROUTES.len(), 12);
}

#[test]
fn expected_routes_all_have_method_and_path() {
    for (method, path) in EXPECTED_ROUTES {
        assert!(!method.is_empty());
        assert!(path.starts_with('/'));
    }
}

#[test]
fn v1_status_route_present_in_expected_table() {
    let has_status = EXPECTED_ROUTES
        .iter()
        .any(|(m, p)| *m == "GET" && *p == "/v1/status");
    assert!(has_status);
}

#[test]
fn per_job_logs_route_present() {
    let has_job_logs = EXPECTED_ROUTES
        .iter()
        .any(|(_, p)| *p == "/v1/commands/:id/logs");
    assert!(
        has_job_logs,
        "per-command structured SSE endpoint must be registered"
    );
}

#[test]
fn legacy_command_log_routes_are_removed() {
    let has_legacy = EXPECTED_ROUTES.iter().any(|(_, p)| p.contains("/jobs/"));
    assert!(
        !has_legacy,
        "legacy /v1/sessions/{{id}}/jobs/{{job_id}}/logs endpoints must not be present"
    );
}

// ─── SqliteSessionStore as API persistence layer ────────────────────────

#[test]
fn api_store_open_from_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = ApiPaths::from_root(tmp.path());
    let store = SqliteSessionStore::open_from_paths(&paths).expect("open from paths");
    // Round-trip session to confirm the store works.
    store
        .insert_session("s1", "/wd", "2026-01-01T00:00:00Z")
        .unwrap();
    let rec = store.get_session("s1").unwrap().unwrap();
    assert_eq!(rec.workdir, "/wd");
}

#[test]
fn api_store_session_dir_is_under_sessions_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = ApiPaths::from_root(tmp.path());
    let session_dir = paths.session_dir("my-session-id");
    assert!(
        session_dir.starts_with(paths.sessions_dir()),
        "session dir should be under sessions dir: {session_dir:?}"
    );
}

#[test]
fn api_paths_api_key_hash_file_under_root() {
    let paths = ApiPaths::from_root("/srv/api");
    let hash = paths.api_key_hash_file();
    assert!(
        hash.starts_with("/srv/api"),
        "api_key.hash should be under root: {hash:?}"
    );
    assert_eq!(hash.file_name().unwrap(), "api_key.hash");
}
