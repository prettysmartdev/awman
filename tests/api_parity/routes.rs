//! Route table and API-path tests.
//!
//! Does NOT start a real server — just verifies the data-layer types used
//! by the API server are correct.

use awman::data::config::env::EnvSnapshot;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::data_paths::AWMAN_HOME;

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
fn api_paths_from_env_honours_awman_home() {
    let env = EnvSnapshot::with_overrides([(AWMAN_HOME, "/custom/root")]);
    let paths = ApiPaths::from_env(&env).unwrap();
    assert_eq!(paths.root(), std::path::Path::new("/custom/root/api"));
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
/// streaming from `/v1/worker/sessions/{id}/jobs/{job_id}/logs` to
/// `/v1/worker/commands/{id}/events`, and added `GET /v1/worker/sessions/:id/queue` and
/// `GET /v1/worker/commands/:id/status`. WI 0138 added `GET /v1/worker/commands/:id`.
const EXPECTED_ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/engine/instances"),
    ("POST", "/v1/engine/instances"),
    ("GET", "/v1/engine/instances/{id}"),
    ("DELETE", "/v1/engine/instances/{id}"),
    ("PATCH", "/v1/engine/instances/{id}"),
    ("GET", "/v1/engine/instances/{id}/frontend"),
    ("GET", "/v1/engine/instances/{id}/tabs"),
    ("GET", "/v1/engine/instances/{id}/tabs/{tab}"),
    ("PATCH", "/v1/engine/instances/{id}/tabs/{tab}"),
    ("GET", "/v1/engine/instances/{id}/dialogs"),
    (
        "POST",
        "/v1/engine/instances/{id}/dialogs/{dialog_id}/answer",
    ),
    ("POST", "/v1/engine/instances/{id}/ui_requests"),
    ("GET", "/v1/health"),
    ("GET", "/v1/keys"),
    ("POST", "/v1/keys"),
    ("DELETE", "/v1/keys/{id}"),
    ("GET", "/v1/status"),
    ("GET", "/v1/config"),
    ("POST", "/v1/config/reload"),
    ("GET", "/v1/features"),
    ("GET", "/v1/features/{feature}"),
    ("PATCH", "/v1/features/{feature}"),
    ("GET", "/v1/grants"),
    ("DELETE", "/v1/grants/{id}"),
    // WI 0141: one core terminal resource; no per-feature terminal routes.
    ("GET", "/v1/terminals"),
    ("GET", "/v1/terminals/{id}"),
    ("GET", "/v1/worker/workdirs"),
    ("GET", "/v1/worker/sessions"),
    ("POST", "/v1/worker/sessions"),
    ("GET", "/v1/worker/sessions/{id}"),
    ("DELETE", "/v1/worker/sessions/{id}"),
    ("GET", "/v1/worker/sessions/{id}/queue"),
    ("GET", "/v1/worker/commands"),
    ("POST", "/v1/worker/commands"),
    ("GET", "/v1/worker/commands/{id}"),
    ("POST", "/v1/worker/commands/{id}/cancel"),
    ("GET", "/v1/worker/commands/{id}/events"),
    ("GET", "/v1/worker/commands/{id}/workflow"),
    ("GET", "/v1/worker/commands/{id}/outputs"),
    ("GET", "/v1/worker/commands/{id}/outputs/{*path}"),
    ("POST", "/v1/worker/runs"),
    // WI 0143 pull routes (local listener only).
    ("GET", "/v1/worker/servers"),
    ("GET", "/v1/worker/servers/{name}"),
    ("GET", "/v1/worker/queue"),
    ("POST", "/v1/worker/queue/{item_id}/claim"),
    ("GET", "/v1/worker/leases"),
    ("GET", "/v1/worker/leases/{id}"),
    ("POST", "/v1/worker/leases/{id}/release"),
    ("POST", "/v1/worker/leases/{id}/events"),
    ("POST", "/v1/worker/leases/{id}/terminal"),
    ("POST", "/v1/worker/leases/{id}/result"),
    ("POST", "/v1/worker/leases/{id}/checkout"),
    ("GET", "/v1/squad/tasks"),
    ("POST", "/v1/squad/tasks"),
    ("GET", "/v1/squad/tasks/{id}"),
    ("PATCH", "/v1/squad/tasks/{id}"),
    ("DELETE", "/v1/squad/tasks/{id}"),
    ("GET", "/v1/squad/tasks/{id}/workflow"),
    ("POST", "/v1/squad/tasks/{id}/runs"),
    ("GET", "/v1/squad/tasks/{id}/runs"),
    ("POST", "/v1/squad/runs/{id}/cancel"),
    ("GET", "/v1/squad/env"),
    ("PUT", "/v1/squad/env"),
    ("DELETE", "/v1/squad/env"),
];
#[test]
fn route_table_has_exactly_the_contract_and_one_owner_per_operation() {
    use awman::command::daemon_api::{capabilities::CAPABILITIES, Feature};
    use awman::frontend::api::routes::ROUTES;
    use std::collections::HashSet;
    let actual = ROUTES
        .iter()
        .map(|r| (r.method, r.path))
        .collect::<HashSet<_>>();
    assert_eq!(actual.len(), ROUTES.len());
    assert_eq!(actual, EXPECTED_ROUTES.iter().copied().collect());
    let operations = ROUTES.iter().map(|r| r.op).collect::<HashSet<_>>();
    assert_eq!(operations.len(), ROUTES.len());
    for route in ROUTES {
        assert!(matches!(
            route.op.owner(),
            Feature::Core | Feature::Worker | Feature::Squad | Feature::Engine
        ));
        if let Some(c) = route.capability {
            assert!(CAPABILITIES.contains(&c));
        }
    }
}
#[test]
fn every_client_action_is_a_registered_route() {
    use awman::command::commands::daemon::actions::ACTIONS;
    use awman::frontend::api::routes::ROUTES;
    for a in ACTIONS {
        assert!(
            ROUTES
                .iter()
                .any(|r| r.method == a.method && r.path == a.path),
            "action {} targets {} {} which is not a route",
            a.name,
            a.method,
            a.path
        );
    }
}

#[test]
fn key_routes_are_local_socket_only() {
    use awman::frontend::api::routes::ROUTES;
    let keys: Vec<_> = ROUTES
        .iter()
        .filter(|r| r.path.starts_with("/v1/keys"))
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|r| r.local_only));
}

/// WI 0140 §3: `PATCH /v1/features/{feature}` is the ephemeral enable/disable
/// route. The router does not register it yet (0140-test-findings.md F1).
#[test]
fn patch_features_route_is_registered() {
    use awman::frontend::api::routes::ROUTES;
    assert!(ROUTES
        .iter()
        .any(|r| r.method == "PATCH" && r.path == "/v1/features/{feature}"));
}

// ─── SqliteSessionStore as API persistence layer ────────────────────────

#[test]
fn api_store_open_from_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = ApiPaths::from_root(tmp.path());
    let store = SqliteSessionStore::open_from_paths(&paths).expect("open from paths");
    // Round-trip session to confirm the store works.
    store
        .insert_session(
            "ses_8ddf878039b70767c4a5bcf4f0c4f65e",
            "/wd",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    let rec = store
        .get_session("ses_8ddf878039b70767c4a5bcf4f0c4f65e")
        .unwrap()
        .unwrap();
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
