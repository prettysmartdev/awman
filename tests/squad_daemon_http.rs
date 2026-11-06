//! The squad feature's HTTP surface (`/v1/squad/*`) on the unified daemon.
//!
//! Each test boots a real `awman daemon start` process with only the squad
//! feature enabled (fake `docker` on its PATH, throwaway `AWMAN_HOME`) and
//! drives it over HTTP through the loopback proxy `ApiServer` provides to the
//! owner-only local socket. The daemon's own log is read back from the
//! process output for the "no value ever reaches a log line" assertions.

use std::sync::Arc;

use awman::command::dispatch::Engines;
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use tokio::sync::Mutex;

#[path = "helpers/api_process.rs"]
mod api_process;
use api_process::{ApiHome, ApiServer};

/// Serialises the tests that touch process-global state (the in-process
/// gateway tests below); daemon tests run in their own process.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn engines_with(container_runtime: Arc<ContainerRuntime>, root: &std::path::Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: container_runtime.clone(),
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(api_paths.root())),
        credential_monitor: None,
        global_config: std::sync::Arc::new(
            awman::data::config::file::ConfigFile::load(
                &root.join("config.json"),
                awman::data::config::FileKind::Global,
            )
            .unwrap(),
        ),
    }
}

/// A running daemon plus the fixtures that keep it alive.
struct Daemon {
    _home: ApiHome,
    server: ApiServer,
}

impl Daemon {
    fn log(&self) -> String {
        self.server.log()
    }

    fn abort(self) {}
}

/// Boot a squad-only daemon whose data and config live under `root`
/// (`AWMAN_HOME`); a test may write `root/config.json` first. Returns the
/// daemon and its base URL.
async fn start_daemon(root: &std::path::Path) -> (Daemon, String) {
    // A test daemon must never reach a real keychain: default to the explicit
    // opt-out unless the test wrote its own config.
    if !root.join("config.json").exists() {
        write_squad_config(root, "none");
    }
    let home = ApiHome::new();
    let server = home
        .start_with_env(&[("AWMAN_HOME", root.to_str().unwrap())])
        .await;
    let base = server.url.clone();
    (
        Daemon {
            _home: home,
            server,
        },
        base,
    )
}

// ─── Startup ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn squad_only_daemon_reports_the_squad_feature_running() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path()).await;

    let resp = reqwest::get(format!("{base}/v1/status")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["features"]["squad"]["state"], "on", "{body}");
    assert_eq!(body["features"]["worker"]["state"], "off", "{body}");

    handle.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
// WI 0116 §4 — the daemon-env protocol over HTTP.
//
// Everything below drives the *shipped wire format* (`API-protocol.md` §1)
// against a real daemon booted through the same seam the tests above use. The
// governing property of the whole surface is negative — **no response body and
// no log line ever carries a payload value** — so the assertions are made on
// the serialised JSON text, never on the typed struct, and every value used
// here is a distinctive sentinel that a scan can look for.
// ═══════════════════════════════════════════════════════════════════════════

/// Sentinels chosen so a substring scan over a whole response body (or the
/// whole captured daemon log) cannot match by accident.
const SECRET: &str = "wi0116-secret-must-never-appear";
const ROTATED_SECRET: &str = "wi0116-rotated-must-never-appear";
const STRAY_SECRET: &str = "wi0116-stray-must-never-appear";
/// response body and on the daemon log.
fn assert_carries_no_value(text: &str, what: &str) {
    for secret in [SECRET, ROTATED_SECRET, STRAY_SECRET] {
        assert!(
            !text.contains(secret),
            "{what} must never carry a payload value, but it contains {secret:?}: {text}"
        );
    }
}

/// A squad task seeded straight into the daemon's database before it boots, so
/// its `env()` names are in `required_env` from the daemon's first tick.
fn seed_task(root: &std::path::Path, name: &str, overlays: &[&str]) {
    seed_task_for_agent(root, name, overlays, None)
}

/// [`seed_task`] for a task that names its agent.
fn seed_task_for_agent(root: &std::path::Path, name: &str, overlays: &[&str], agent: Option<&str>) {
    let db = root.join("data").join("awman.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = awman::data::fs::TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    store
        .create(&awman::data::fs::Task {
            prerequisites: None,
            id: awman::data::ids::TaskId::new(),
            name: name.into(),
            description: "seeded for the env protocol tests".into(),
            repo_scope: root.to_path_buf(),
            mount_scope: awman::data::fs::MountScope::GitRoot,
            overlays: overlays.iter().map(|s| s.to_string()).collect(),
            interval_secs: 3600,
            status: awman::data::fs::TaskStatus::Active,
            agent: agent.map(String::from),
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        })
        .unwrap();
}

/// One request, returning the status and the **raw body text** — the shape
/// every "no value in the body" assertion needs.
async fn request(
    method: reqwest::Method,
    url: &str,
    key: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, String) {
    let mut builder = reqwest::Client::new().request(method, url);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    if let Some(body) = body {
        builder = builder.json(&body);
    }
    let response = builder.send().await.expect("request must reach the daemon");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

async fn env_get(base: &str, key: Option<&str>) -> serde_json::Value {
    let (status, text) = request(
        reqwest::Method::GET,
        &format!("{base}/v1/squad/env"),
        key,
        None,
    )
    .await;
    assert_eq!(status, 200, "GET /v1/squad/env: {text}");
    assert_carries_no_value(&text, "GET /v1/squad/env");
    serde_json::from_str(&text).expect("coverage must be JSON")
}

async fn env_push(
    base: &str,
    key: Option<&str>,
    body: serde_json::Value,
) -> (u16, String, serde_json::Value) {
    let (status, text) = request(
        reqwest::Method::PUT,
        &format!("{base}/v1/squad/env"),
        key,
        Some(body),
    )
    .await;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, text, json)
}

/// The coverage entry for `name`, or `None` when the daemon no longer requires
/// it at all.
fn entry<'a>(coverage: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    coverage["required"]
        .as_array()
        .expect("`required` is an array")
        .iter()
        .find(|entry| entry["name"] == name)
}

#[tokio::test]
async fn daemon_env_push_reports_accepted_names_only_and_its_body_carries_no_value() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(ACCEPTED_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ACCEPTED_TOKEN": SECRET, "STRAY": STRAY_SECRET}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(body["accepted"], serde_json::json!(["ACCEPTED_TOKEN"]));
    assert_eq!(
        body["ignored"],
        serde_json::json!(["STRAY"]),
        "a name outside required_env is reported, never stored"
    );
    // The whole point of the route: assert on the serialised JSON, because a
    // typed struct with no value field proves nothing about what the
    // *serializer* emitted.
    assert_carries_no_value(&text, "the POST /v1/squad/env body");

    // The embedded coverage reports the name and a digest, and the digest is
    // not the value: 16 hex chars, whatever the value's length.
    let digest = body["coverage"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "ACCEPTED_TOKEN")
        .and_then(|e| e["digest"].as_str())
        .expect("an accepted name reports a digest");
    assert_eq!(digest.len(), 16, "the digest is 16 hex chars: {digest}");
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

    handle.abort();
}

/// A body that will not deserialise is answered with a **fixed** string. axum's
/// own `JsonDataError` text quotes the offending input, which for this one
/// route is the payload — so neither the body nor a serde message may reach the
/// response *or the log*.
#[tokio::test]
async fn a_malformed_daemon_env_push_echoes_neither_the_body_nor_a_serde_message() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(MALFORMED_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    // A non-string value: serde rejects it, and the default rejection text
    // would quote `{"vars":{"MALFORMED_TOKEN":"<the value>"}}`.
    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"MALFORMED_TOKEN": {"nested": SECRET}}}),
    )
    .await;
    assert_eq!(status, 400, "{text}");
    assert_eq!(body["error"], "malformed daemon env request");
    assert_carries_no_value(&text, "the 400 body");
    assert!(
        !text.contains("MALFORMED_TOKEN"),
        "not even the name may be echoed back from a rejected body: {text}"
    );

    // …and the daemon's own log — which is what `awman squad logs` prints —
    // carries no part of it either.
    let log = handle.log();
    assert_carries_no_value(&log, "the daemon log");
    assert!(
        log.contains("rejected a malformed daemon env push"),
        "the rejection is still recorded, as a fixed line with no fields: {log}"
    );

    handle.abort();
}

/// **The regression this work item's merge semantics exist to prevent.**
///
/// Running any squad command from a shell that does not export the tokens
/// reports every one of them `absent`. If that push were a whole-map replace —
/// or if `absent` meant "delete" — every scheduled task would be silently
/// disarmed by someone opening a terminal. It must be a strict no-op: the
/// overlay, the digest, `last_provided_at` and `unmet_since` all survive
/// untouched.
///
/// A whole-map-replace implementation fails this test on the very first
/// assertion after the all-absent push.
#[tokio::test]
async fn an_all_absent_push_leaves_every_existing_value_in_place() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(
        tmp.path(),
        "nightly",
        &["env(ABSENT_ONE)", "env(ABSENT_TWO)"],
    );
    let (handle, base) = start_daemon(tmp.path()).await;

    // An armed daemon: two names, both provided.
    let (status, _, _) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ABSENT_ONE": SECRET, "ABSENT_TWO": SECRET}}),
    )
    .await;
    assert_eq!(status, 200);
    let armed = env_get(&base, None).await;
    let deploy_before = entry(&armed, "ABSENT_ONE").unwrap().clone();
    let npm_before = entry(&armed, "ABSENT_TWO").unwrap().clone();
    assert!(deploy_before["digest"].is_string(), "{armed}");

    // The under-equipped shell: it has neither, and says so.
    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {}, "absent": ["ABSENT_ONE", "ABSENT_TWO"]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        body["accepted"],
        serde_json::json!([]),
        "an all-absent push accepts nothing"
    );
    assert_eq!(body["ignored"], serde_json::json!([]));

    let after = env_get(&base, None).await;
    assert_eq!(
        entry(&after, "ABSENT_ONE"),
        Some(&deploy_before),
        "an `absent` report must not disturb a held value — digest, source, \
         last_provided_at and unmet_since all unchanged"
    );
    assert_eq!(entry(&after, "ABSENT_TWO"), Some(&npm_before));
    assert_eq!(
        after["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source"] == "pushed")
            .count(),
        2,
        "both names are still held by the daemon: {after}"
    );

    handle.abort();
}

/// Rotation works, an `absent` never removes, and the daemon's own answer says
/// which of the two happened.
#[tokio::test]
async fn a_provided_value_replaces_an_existing_one_and_an_absent_never_removes_it() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(ROTATE_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ROTATE_TOKEN": SECRET}}),
    )
    .await;
    let first = env_get(&base, None).await;
    let first_digest = entry(&first, "ROTATE_TOKEN").unwrap()["digest"]
        .as_str()
        .unwrap()
        .to_string();

    // Rotation: a different value for a name the daemon already holds.
    let (_, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ROTATE_TOKEN": ROTATED_SECRET}}),
    )
    .await;
    assert_eq!(body["accepted"], serde_json::json!(["ROTATE_TOKEN"]));
    assert_carries_no_value(&text, "the rotation response");
    let rotated = env_get(&base, None).await;
    let rotated_digest = entry(&rotated, "ROTATE_TOKEN").unwrap()["digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        rotated_digest, first_digest,
        "a rotated value must replace what the daemon held"
    );

    // An `absent` for a name the daemon holds is not a deletion.
    env_push(
        &base,
        None,
        serde_json::json!({"vars": {}, "absent": ["ROTATE_TOKEN"]}),
    )
    .await;
    let after_absent = env_get(&base, None).await;
    assert_eq!(
        entry(&after_absent, "ROTATE_TOKEN").unwrap()["digest"]
            .as_str()
            .unwrap(),
        rotated_digest,
        "an `absent` report never wins over a provision"
    );

    handle.abort();
}

/// The **only** removal: the last task naming a variable goes away, so nothing
/// declares it any more. It leaves `required_env`, and its value leaves with
/// it — re-declaring the name reports it uncovered rather than silently
/// re-attaching the old value.
#[tokio::test]
async fn a_name_leaving_required_env_takes_its_value_with_it() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_task(tmp.path(), "nightly", &["env(GC_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"GC_TOKEN": SECRET}}),
    )
    .await;
    assert!(entry(&env_get(&base, None).await, "GC_TOKEN").is_some());

    // Drop the only task that declares it.
    let (status, _) = request(
        reqwest::Method::DELETE,
        &format!("{base}/v1/squad/tasks/{}", task_id(&base, "nightly").await),
        None,
        None,
    )
    .await;
    assert_eq!(status, 204);

    let after = env_get(&base, None).await;
    assert!(
        entry(&after, "GC_TOKEN").is_none(),
        "a name nothing declares any more leaves required_env: {after}"
    );

    // Re-declaring it must find nothing held: the value was garbage-collected
    // with the name, not merely hidden from the report.
    let (status, text) = request(
        reqwest::Method::POST,
        &format!("{base}/v1/squad/tasks"),
        None,
        Some(serde_json::json!({"name":"nightly-again","description":"re-declares the same variable","workspace":{"kind":"custom","path":repo},"mount_scope":"cwd","interval_ms":3600000,"overlays":["env(GC_TOKEN)"]})),
    )
    .await;
    assert_eq!(status, 201, "{text}");
    let readded = env_get(&base, None).await;
    let entry = entry(&readded, "GC_TOKEN").expect("the name is required again");
    assert!(
        entry["digest"].is_null() && entry["source"].is_null(),
        "the value left with the name and did not come back: {entry}"
    );

    handle.abort();
}

/// An exported-but-empty variable is not a provision. The rule matches the
/// set-and-non-empty test `src/engine/container/options.rs` already applies to
/// a declared `env()`, so `FOO=` behaves identically on both sides of the
/// socket — and it may not overwrite a value the daemon already holds.
#[tokio::test]
async fn an_empty_string_value_is_treated_exactly_as_absent() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(EMPTY_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    // On an empty daemon: neither accepted nor ignored, and nothing is held.
    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"EMPTY_TOKEN": ""}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(body["accepted"], serde_json::json!([]));
    assert_eq!(
        body["ignored"],
        serde_json::json!([]),
        "an empty value is absent, not an unrequired name"
    );
    let empty = env_get(&base, None).await;
    assert!(entry(&empty, "EMPTY_TOKEN").unwrap()["digest"].is_null());

    // On an armed daemon: the held value survives.
    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"EMPTY_TOKEN": SECRET}}),
    )
    .await;
    let armed = entry(&env_get(&base, None).await, "EMPTY_TOKEN")
        .unwrap()
        .clone();
    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"EMPTY_TOKEN": ""}}),
    )
    .await;
    assert_eq!(
        entry(&env_get(&base, None).await, "EMPTY_TOKEN"),
        Some(&armed),
        "an empty value must not disarm a name the daemon holds"
    );

    handle.abort();
}

/// The negative property across the *whole* surface a push touches, asserted on
/// raw response text: coverage, push, clear, status, and the command envelope
/// that returns tasks.
#[tokio::test]
async fn no_endpoint_the_env_protocol_touches_returns_a_payload_value() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(SCAN_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"SCAN_TOKEN": SECRET, "STRAY": STRAY_SECRET}}),
    )
    .await;

    let mut bodies: Vec<(String, String)> = Vec::new();
    for (method, path) in [
        (reqwest::Method::GET, "/v1/squad/env"),
        (reqwest::Method::GET, "/v1/status"),
        (reqwest::Method::DELETE, "/v1/squad/env"),
    ] {
        let (status, text) = request(method.clone(), &format!("{base}{path}"), None, None).await;
        assert_eq!(status, 200, "{method} {path}: {text}");
        bodies.push((format!("{method} {path}"), text));
    }
    for path in [
        "/v1/squad/tasks".to_string(),
        format!("/v1/squad/tasks/{}", task_id(&base, "nightly").await),
    ] {
        let (status, text) =
            request(reqwest::Method::GET, &format!("{base}{path}"), None, None).await;
        assert_eq!(status, 200, "{text}");
        bodies.push((path, text));
    }

    for (what, text) in &bodies {
        assert_carries_no_value(text, what);
    }
    // And the daemon's log, which `awman squad logs` prints verbatim.
    assert_carries_no_value(&handle.log(), "the daemon log");

    handle.abort();
}

/// `DELETE` removes what is *stored*, and says so honestly: `false` when there
/// is no keychain behind this daemon. It must never disarm a daemon that is
/// running fine, so the in-memory overlay is untouched.
#[tokio::test]
async fn clearing_the_store_reports_honestly_and_leaves_the_running_overlay_intact() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    write_squad_config(tmp.path(), "none");
    seed_task(tmp.path(), "nightly", &["env(CLEAR_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"CLEAR_TOKEN": SECRET}}),
    )
    .await;
    let armed = entry(&env_get(&base, None).await, "CLEAR_TOKEN")
        .unwrap()
        .clone();

    let (status, text) = request(
        reqwest::Method::DELETE,
        &format!("{base}/v1/squad/env"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["cleared"], false,
        "there is no keychain item to remove on a daemon that persists nothing"
    );

    assert_eq!(
        entry(&env_get(&base, None).await, "CLEAR_TOKEN"),
        Some(&armed),
        "clearing the store must not disarm the running daemon"
    );

    handle.abort();
}

/// Write a global config carrying `squad.envPersistence`.
fn write_squad_config(root: &std::path::Path, persistence: &str) {
    let path = root.join("config.json");
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "daemon":{"features":{"squad": { "enabled": true, "envPersistence": persistence }}}
        }))
        .unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn disabled_squad_feature_returns_feature_disabled() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.json"),
        serde_json::json!({"daemon":{"features":{"worker":{"enabled":true},"squad":{"enabled":false,"envPersistence":"none"}}}}).to_string(),
    )
    .unwrap();
    let (handle, base) = start_daemon(root.path()).await;
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/squad/tasks"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["code"],
        "feature_disabled"
    );
    handle.abort();
}

/// §5a on the wire: `GET /v1/status` reports the daemon's persistence state as
/// a plain string. `"none"` is the explicit opt-out, and it is what a daemon
/// configured that way must say — never a guess and never an empty string.
#[tokio::test]
async fn status_reports_env_persistence_none_for_the_explicit_opt_out() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    write_squad_config(tmp.path(), "none");
    seed_task(tmp.path(), "nightly", &["env(STATUS_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;

    let (status, text) = request(
        reqwest::Method::GET,
        &format!("{base}/v1/status"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let _body: serde_json::Value = serde_json::from_str(&text).unwrap();

    // The same daemon's coverage carries the same string.
    let coverage = env_get(&base, None).await;
    assert_eq!(coverage["persistence"], "none");
    // …and the same name carries the `unmet_since` clock `awman squad env`'s
    // SINCE column renders: the count in the status line and the "since when"
    // in the table are two views of one fact, stamped when the name first
    // became required while uncovered.
    let unmet = entry(&coverage, "STATUS_TOKEN").expect("the name is required");
    assert!(
        unmet["unmet_since"].is_string(),
        "an uncovered required name carries an unmet clock: {unmet}"
    );
    assert_eq!(unmet["source"], serde_json::Value::Null);
    assert_eq!(unmet["required_by"], serde_json::json!(["nightly"]));

    handle.abort();
}

/// The reported bug, end to end: a task declaring `env(GITHUB_TOKEN)` against a
/// daemon that has no value for it must report unmet at every surface, exactly
/// like any other name.
///
/// `GITHUB_TOKEN` used to be seeded into `required_env` on every daemon and
/// exempted from unmet reporting, so all of these were silent — the TUI card,
/// the detail modal, the `squad ●` indicator, `squad status`, the create/edit
/// warning and the run row alike — for the one variable WI 0116's own examples
/// are written around.
#[tokio::test]
async fn a_task_declared_github_token_is_reported_unmet_like_any_other_name() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(GITHUB_TOKEN)"]);
    // The daemon must not inherit one from whatever shell runs the suite: what
    // is under test is the no-value case.
    std::env::remove_var("GITHUB_TOKEN");
    let (handle, base) = start_daemon(tmp.path()).await;

    let (status, text) = request(
        reqwest::Method::GET,
        &format!("{base}/v1/status"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let _body: serde_json::Value = serde_json::from_str(&text).unwrap();

    // …and so must the per-task marker every TUI surface reads.
    let (status, text) = request(
        reqwest::Method::GET,
        &format!("{base}/v1/squad/tasks"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let tasks: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        tasks["items"][0]["unmet_env"],
        serde_json::json!(["GITHUB_TOKEN"]),
        "the card, the detail modal and the indicator all read this: {tasks}"
    );

    // The coverage view agrees, with the same unmet clock any other name gets.
    let coverage = env_get(&base, None).await;
    let entry = entry(&coverage, "GITHUB_TOKEN").expect("the task declares it");
    assert!(
        entry["unmet_since"].is_string(),
        "an uncovered declared name carries an unmet clock: {entry}"
    );
    assert_eq!(entry["required_by"], serde_json::json!(["nightly"]));

    handle.abort();
}

/// The three `env_persistence` strings, at the seam `GET /v1/status`
/// serialises. Driven through `LocalTaskGateway::status` — the function the
/// route calls — because the `keychain` and `unavailable(...)` states depend on
/// what the host machine actually has, and a wire test could only pin whichever
/// one this CI box happens to be in.
#[tokio::test]
async fn the_status_route_reports_all_three_env_persistence_forms() {
    use awman::command::commands::squad::gateway::{LocalTaskGateway, TaskGateway};
    use awman::data::fs::daemon_env::{EnvPersistence, NoStore};
    use awman::engine::squad::env_state::{DaemonEnvState, Salt};

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(awman::data::fs::TaskStore::open(&tmp.path().join("squad.db")).unwrap());
    store.migrate().unwrap();

    for (persistence, expected) in [
        (EnvPersistence::Keychain, "keychain"),
        (EnvPersistence::None, "none"),
        (
            EnvPersistence::Unavailable("secret-tool not found".into()),
            "unavailable(secret-tool not found)",
        ),
    ] {
        let env_state = Arc::new(DaemonEnvState::new(
            Box::new(NoStore),
            persistence,
            Salt::random(),
        ));
        let gateway = LocalTaskGateway::new(
            store.clone(),
            engines_with(Arc::new(ContainerRuntime::docker()), tmp.path()),
            Arc::new(std::sync::Mutex::new(
                awman::engine::squad::SchedulerStatus::default(),
            )),
            SquadPaths::from_root(tmp.path().join("squad")),
            env_state,
        );
        let status = gateway.status().await.expect("status must succeed");
        assert_eq!(
            status.env_persistence.as_ref().map(|p| p.to_string()),
            Some(expected.to_string()),
            "the string `GET /v1/status` serialises is the daemon's own Display"
        );
        // And it survives serialisation verbatim — the frontend renders it as
        // written, `<reason>` included.
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["env_persistence"], expected);
    }
}

/// Remediation of review-security F3 / review-adversarial F4.
///
/// `DaemonEnvState`'s store calls are synchronous and capped at five seconds.
/// Issuing one directly from an `async fn` parks a runtime worker for the whole
/// cap whenever the keychain is slow, locked, or has no D-Bus behind it — the
/// exact states the design anticipates — and an authenticated client can drive
/// it once per push. The gateway therefore hands them to `spawn_blocking`, as
/// the daemon bootstrap already does.
///
/// A single-threaded runtime makes the difference observable: if the keychain
/// write ran inline, the one worker would be parked for the whole write and the
/// concurrently spawned task below could not tick at all.
#[tokio::test(flavor = "current_thread")]
async fn a_slow_keychain_write_does_not_park_the_runtime_during_a_push() {
    use awman::command::commands::squad::gateway::{EnvPush, LocalTaskGateway, TaskGateway};
    use awman::data::config::env::DaemonEnvMap;
    use awman::data::error::DataError;
    use awman::data::fs::daemon_env::{DaemonEnvStore, EnvPersistence};
    use awman::engine::squad::env_state::{DaemonEnvState, Salt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stands in for a keychain that answers slowly — well short of the five
    /// second cap, so the test stays fast, but long enough that a parked worker
    /// is unambiguous.
    struct SlowStore;
    impl DaemonEnvStore for SlowStore {
        fn backend_name(&self) -> &'static str {
            "keychain"
        }
        fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Ok(())
        }
        fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
            Ok(None)
        }
        fn clear(&self) -> Result<(), DataError> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Ok(())
        }
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(awman::data::fs::TaskStore::open(&tmp.path().join("squad.db")).unwrap());
    store.migrate().unwrap();

    let env_state = Arc::new(DaemonEnvState::new(
        Box::new(SlowStore),
        EnvPersistence::Keychain,
        Salt::random(),
    ));
    let gateway = LocalTaskGateway::new(
        store.clone(),
        engines_with(Arc::new(ContainerRuntime::docker()), tmp.path()),
        Arc::new(std::sync::Mutex::new(
            awman::engine::squad::SchedulerStatus::default(),
        )),
        SquadPaths::from_root(tmp.path().join("squad")),
        Arc::clone(&env_state),
    );

    // The daemon stores a pushed value only for a name something declared, and
    // `push_env` recomputes `required_env` from the store on the way in — so
    // the declaring task has to be on disk. No name is seeded on the daemon's
    // own behalf.
    let now = chrono::Utc::now();
    store
        .create(&awman::data::fs::Task {
            prerequisites: None,
            id: awman::data::ids::TaskId::new(),
            name: "nightly".into(),
            description: "declares the pushed name".into(),
            repo_scope: tmp.path().to_path_buf(),
            mount_scope: awman::data::fs::MountScope::GitRoot,
            overlays: vec!["env(GITHUB_TOKEN)".to_string()],
            interval_secs: 3600,
            status: awman::data::fs::TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        })
        .unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = tokio::spawn({
        let ticks = Arc::clone(&ticks);
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let response = gateway
        .push_env(EnvPush {
            vars: DaemonEnvMap::from_pairs([("GITHUB_TOKEN", "ghp-not-a-real-token")]),
            absent: Vec::new(),
        })
        .await
        .expect("a push must succeed even when the store is slow");
    assert_eq!(response.accepted, vec!["GITHUB_TOKEN".to_string()]);

    let during_push = ticks.load(Ordering::SeqCst);
    assert!(
        during_push > 5,
        "the runtime was parked for the keychain write: only {during_push} ticks \
         elapsed during a 400ms store call on a single-threaded runtime"
    );

    // `DELETE /v1/squad/env` never degrades the daemon, so it is the one call a
    // client can re-drive indefinitely. It must be off the runtime too.
    let before_clear = ticks.load(Ordering::SeqCst);
    gateway
        .clear_env_store()
        .await
        .expect("a clear must succeed even when the store is slow");
    assert!(
        ticks.load(Ordering::SeqCst) - before_clear > 5,
        "the runtime was parked for the keychain clear"
    );

    ticker.abort();
    // Leave the process-global overlay as this test found it.
    awman::data::config::env::set_daemon_overlay(DaemonEnvMap::new());
}

/// A custom agent's `auth.env` is required like an `env(NAME)` overlay, but
/// only for a definition that resolves (here: a global one without `[ping]`,
/// which needs no approval). An unapproved definition solicits nothing. The
/// value is held while declared, never echoed, and leaves with its last task.
#[tokio::test]
async fn approved_custom_agent_auth_env_is_required_and_collected_with_its_task() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let agents = tmp.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let def = |auth: &str, ping: &str| {
        format!(
            "schema=1\ndisplay_name='X'\n[run]\ncommand=['x']\n[auth]\nenv=['{auth}']\n{ping}[image]\ninstall=['true']\n"
        )
    };
    std::fs::write(agents.join("pi.toml"), def("PI_API_KEY", "")).unwrap();
    // A global definition with [ping] needs an approval it does not have.
    std::fs::write(
        agents.join("ghost.toml"),
        def("GHOST_KEY", "[ping]\ncommand=['/bin/echo']\n"),
    )
    .unwrap();
    seed_task_for_agent(tmp.path(), "nightly", &[], Some("pi"));
    seed_task_for_agent(tmp.path(), "haunted", &[], Some("ghost"));
    let (handle, base) = start_daemon(tmp.path()).await;

    let coverage = env_get(&base, None).await;
    let pi = entry(&coverage, "PI_API_KEY").expect("auth.env of an approved agent is required");
    assert_eq!(pi["required_by"], serde_json::json!(["nightly"]), "{pi}");
    assert!(
        entry(&coverage, "GHOST_KEY").is_none(),
        "an unapproved definition solicits no secret: {coverage}"
    );

    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"PI_API_KEY": SECRET, "GHOST_KEY": STRAY_SECRET}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(body["accepted"], serde_json::json!(["PI_API_KEY"]));
    assert_eq!(body["ignored"], serde_json::json!(["GHOST_KEY"]));
    assert_carries_no_value(&text, "the POST /v1/squad/env body");
    let held = env_get(&base, None).await;
    assert!(
        entry(&held, "PI_API_KEY").unwrap()["digest"].is_string(),
        "{held}"
    );

    let (status, _) = request(
        reqwest::Method::DELETE,
        &format!("{base}/v1/squad/tasks/{}", task_id(&base, "nightly").await),
        None,
        None,
    )
    .await;
    assert_eq!(status, 204);
    let after = env_get(&base, None).await;
    assert!(
        entry(&after, "PI_API_KEY").is_none(),
        "the name leaves with the last task whose agent declares it: {after}"
    );
    assert!(!handle.log().contains(SECRET));

    handle.abort();
}

#[tokio::test]
async fn prerequisite_widening_is_refused_by_the_real_squad_route_unless_operator_opts_in() {
    let _env_guard = ENV_LOCK.lock().await;
    for (use_policy, allow) in [("ask", false), ("deny", false), ("ask", true)] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.json"), serde_json::json!({
            "daemon":{"features":{"squad": {"enabled":true,"envPersistence":"none"}}},
            "workflows": {"prerequisites": {"allowApiClientGrants":allow, "ssh":{"use":use_policy}}}
        }).to_string()).unwrap();
        let (handle, base) = start_daemon(root.path()).await;
        for (name, flag, value, accepted) in [
            ("ssh-task", "--prerequisite-ssh", "all", allow),
            ("token-task", "--prerequisite-token", "env:GH_TOKEN", allow),
            ("narrow-task", "--prerequisite-ssh", "deny", true),
        ] {
            let flags = awman::data::config::flags::PrerequisiteFlags {
                ssh: if flag == "--prerequisite-ssh" {
                    Some(if value == "all" {
                        awman::data::config::flags::SshFlag::All
                    } else {
                        awman::data::config::flags::SshFlag::Deny
                    })
                } else {
                    None
                },
                token: if flag == "--prerequisite-token" {
                    Some(awman::data::config::flags::TokenFlag::Env(
                        "GH_TOKEN".into(),
                    ))
                } else {
                    None
                },
                ..Default::default()
            };
            let policy = awman::data::config::workflows::PrerequisitePolicy::from_flags(&flags);
            let response=reqwest::Client::new().post(format!("{base}/v1/squad/tasks")).json(&serde_json::json!({"name":name,"description":"security fixture","workspace":{"kind":"default"},"mount_scope":"directory","interval_ms":3600000,"prerequisites":policy})).send().await.unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(status.as_u16(), if accepted { 201 } else { 403 }, "{text}");
            if !accepted {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&text).unwrap()["code"],
                    "prerequisite_widening_refused"
                );
            }
        }
        handle.abort();
    }
}

#[tokio::test]
async fn malformed_command_json_uses_the_shared_error_envelope() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path()).await;
    for (input, status, code) in [
        ("{bad", 400, "invalid_request"),
        ("{}", 400, "invalid_request"),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{base}/v1/squad/tasks"))
            .header("content-type", "application/json")
            .body(input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["content-type"], "application/json");
        let json: serde_json::Value = response.json().await.unwrap();
        assert_eq!(json["code"], code);
    }
    handle.abort();
}

async fn task_id(base: &str, name: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("{base}/v1/squad/tasks")).unwrap();
    url.query_pairs_mut().append_pair("name", name);
    let response = reqwest::Client::new().get(url).send().await.unwrap();
    let page: serde_json::Value = response.json().await.unwrap();
    page["items"][0]["id"].as_str().expect("task ID").into()
}

#[tokio::test]
async fn task_resources_page_patch_nullable_fields_and_cancel_the_exact_reserved_run() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(root.path()).await;
    let client = reqwest::Client::new();
    for name in ["page-a", "page-b"] {
        let response=client.post(format!("{base}/v1/squad/tasks")).json(&serde_json::json!({"name":name,"description":"resource fixture","workspace":{"kind":"default"},"mount_scope":"directory","interval_ms":60000,"agent":"claude","model":"fixture-model"})).send().await.unwrap();
        assert_eq!(response.status(), 201);
        let task: serde_json::Value = response.json().await.unwrap();
        assert!(task["id"].as_str().unwrap().starts_with("task_"));
        assert_eq!(task["interval_ms"], 60000);
        assert!(task.get("interval_secs").is_none());
    }
    let page: serde_json::Value = client
        .get(format!("{base}/v1/squad/tasks?limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap();
    let mut next = reqwest::Url::parse(&format!("{base}/v1/squad/tasks")).unwrap();
    next.query_pairs_mut()
        .append_pair("limit", "1")
        .append_pair("cursor", cursor);
    let page2: serde_json::Value = client.get(next).send().await.unwrap().json().await.unwrap();
    assert_ne!(page["items"][0]["id"], page2["items"][0]["id"]);
    assert!(page2["next_cursor"].is_null());
    let id = task_id(&base, "page-a").await;
    let url = format!("{base}/v1/squad/tasks/{id}");
    let malformed = client
        .patch(&url)
        .json(&serde_json::json!({"pasued":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    assert!(
        malformed.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("pasued")
    );
    let invalid_status = client
        .get(format!("{url}/runs?status=bogus"))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_status.status(), 400);
    assert_eq!(
        invalid_status.json::<serde_json::Value>().await.unwrap()["code"],
        "invalid_request"
    );
    let response = client
        .patch(&url)
        .json(&serde_json::json!({"paused":true,"agent":null,"model":null}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let task: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{task}");
    assert_eq!(task["status"], "paused");
    assert!(task["agent"].is_null());
    assert!(task["model"].is_null());
    let refused = client.post(format!("{url}/runs")).send().await.unwrap();
    assert_eq!(refused.status(), 400);
    assert_eq!(
        refused.json::<serde_json::Value>().await.unwrap()["code"],
        "invalid_request"
    );
    assert_eq!(
        client
            .patch(&url)
            .json(&serde_json::json!({"paused":false}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = client
        .post(format!("{url}/runs"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let created: serde_json::Value = response.json().await.unwrap();
    let run = created["id"].as_str().unwrap();
    assert!(run.starts_with("run_"));
    let response = client
        .post(format!("{base}/v1/squad/runs/{run}/cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let runs: serde_json::Value = client
        .get(format!("{url}/runs?status=canceled&limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runs["items"][0]["id"], run);
    handle.abort();
}

/// 0140 finding F2 (see 0140-test-findings.md): `awman squad --headless`
/// status deserialises the squad feature's `detail` from `GET /v1/status` as a
/// `DaemonStatus`, but `SquadFeature::status` only reports tick data, so the
/// required fields (`running`, `task_count`, `in_flight`, …) and the
/// `env_persistence` / `unmet_env` fields are missing.
#[tokio::test]
async fn squad_status_detail_carries_the_daemon_status_contract() {
    let tmp = tempfile::tempdir().unwrap();
    write_squad_config(tmp.path(), "none");
    seed_task(tmp.path(), "nightly", &["env(STATUS_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path()).await;
    let body: serde_json::Value = reqwest::get(format!("{base}/v1/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let detail = body["features"]["squad"]["detail"].clone();
    assert_eq!(detail["env_persistence"], "none");
    assert_eq!(detail["unmet_env"], serde_json::json!(["STATUS_TOKEN"]));
    assert!(detail.get("scheduler_last_tick_at").is_some());
    assert!(detail["task_count"].as_u64().unwrap() >= 1);
    handle.abort();
}
