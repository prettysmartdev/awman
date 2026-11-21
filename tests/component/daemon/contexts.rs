//! Keyed durable repo context (WI 0146) through the production HTTP router:
//! session `context_key`, `/v1/contexts/{key}` and a push command's
//! `context_changes`.
use super::support::Api;
use crate::support;
use awman::command::daemon_api::keys;
use awman_test_support::capability::LocalGit;
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::{wait_for_async, TestSandbox};
use serde_json::{json, Value};
use std::time::Duration;

fn worker_config(workdirs: &[&std::path::Path]) -> Value {
    json!({"daemon":{"features":{"worker":{"enabled":true,
            "push":{"enabled":true,"workDirs":{"allow":workdirs}}}},
        "network":{"listeners":{"component":{"features":["engine","worker"]}}}}})
}

struct Raw {
    status: u16,
    headers: reqwest::header::HeaderMap,
    bytes: Vec<u8>,
}
impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn raw(
    server: &Api,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Vec<u8>>,
    if_match: Option<&str>,
) -> Raw {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .request(method.parse().unwrap(), format!("{}{path}", server.base))
        .bearer_auth(key);
    if let Some(tag) = if_match {
        request = request.header("If-Match", tag);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request.send().await.unwrap();
    Raw {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        bytes: response.bytes().await.unwrap().to_vec(),
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// No response body may name the sandbox (host paths).
fn assert_no_host_path(s: &TestSandbox, body: &[u8]) {
    let root = s.root().to_string_lossy().into_owned();
    let text = String::from_utf8_lossy(body);
    assert!(!text.contains(&root), "host path leaked: {text}");
    assert!(!text.contains("_keyed"), "host layout leaked: {text}");
}

fn keyed_dir(s: &TestSandbox, key: &str) -> std::path::PathBuf {
    let env = support::env_snapshot(s);
    awman::data::fs::context_dirs::ContextDirResolver::from_env(&env)
        .unwrap()
        .keyed_repo_dir(&awman::data::context::ContextKey::parse(key).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_advertises_context_api() {
    let s = support::sandbox("daemon::contexts::status_advertises_context_api");
    let server = Api::with_config(&s, worker_config(&[])).await;
    let key = keys::keys_create(&server.api, "reader").unwrap();
    s.redact(&key.key);
    let (code, status) = server.get("/v1/status", Some(&key.key)).await;
    assert_eq!(code, 200, "{status}");
    let capabilities = status["capabilities"].as_array().unwrap();
    assert_eq!(
        capabilities
            .iter()
            .filter(|c| **c == json!("context_api"))
            .count(),
        1,
        "{status}"
    );
    server.shutdown().await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_context_api_reads_files_and_refuses_mutations() {
    let s =
        support::sandbox("daemon::contexts::windows_context_api_reads_files_and_refuses_mutations");
    let server = Api::with_config(&s, worker_config(&[])).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let k = &key.key;
    let url = "/v1/contexts/memory/files/notes/plan.md";
    let dir = keyed_dir(&s, "memory");
    let empty = raw(&server, "GET", "/v1/contexts/memory", k, None, None).await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.json(), json!({"items": [], "next_cursor": null}));
    let refused = raw(&server, "PUT", url, k, Some(b"new".to_vec()), None).await;
    assert_eq!(refused.status, 500);
    assert_eq!(refused.json()["code"], "internal");
    assert_no_host_path(&s, &refused.bytes);
    assert!(!dir.exists(), "a refused write must not create context");

    // Agent-written files remain available through the read-only API.
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    std::fs::write(dir.join("notes/plan.md"), b"memory").unwrap();
    let digest = sha256(b"memory");
    let fetched = raw(&server, "GET", url, k, None, None).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.bytes, b"memory");
    assert_eq!(
        fetched.header("etag"),
        Some(format!("\"{digest}\"").as_str())
    );
    assert_eq!(fetched.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(fetched.header("content-disposition"), Some("attachment"));
    let listed = raw(&server, "GET", "/v1/contexts/memory", k, None, None).await;
    assert_eq!(listed.status, 200);
    let meta = listed.json();
    assert_eq!(meta["items"].as_array().unwrap().len(), 1);
    assert_eq!(meta["items"][0]["path"], "notes/plan.md");
    assert_eq!(meta["items"][0]["size_bytes"], 6);
    assert_eq!(meta["items"][0]["sha256"], digest);
    assert!(meta["next_cursor"].is_null());
    chrono::DateTime::parse_from_rfc3339(meta["items"][0]["modified_at"].as_str().unwrap())
        .unwrap();
    assert_no_host_path(&s, &listed.bytes);
    for condition in [None, Some(digest.as_str())] {
        for (method, body) in [("PUT", Some(b"changed".to_vec())), ("DELETE", None)] {
            let refused = raw(&server, method, url, k, body, condition).await;
            assert_eq!(refused.status, 500, "{method}");
            assert_eq!(refused.json()["code"], "internal");
            assert_no_host_path(&s, &refused.bytes);
            assert_eq!(std::fs::read(dir.join("notes/plan.md")).unwrap(), b"memory");
        }
    }
    server.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyed_files_round_trip_with_etags_and_preconditions() {
    let s =
        support::sandbox("daemon::contexts::keyed_files_round_trip_with_etags_and_preconditions");
    let server = Api::with_config(&s, worker_config(&[])).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let k = &key.key;

    // A key with no directory yet is an empty 0139 list, not 404.
    let listed = raw(&server, "GET", "/v1/contexts/team.memory", k, None, None).await;
    assert_eq!(listed.status, 200);
    assert_eq!(listed.json(), json!({"items": [], "next_cursor": null}));
    assert!(
        !keyed_dir(&s, "team.memory").exists(),
        "a read creates nothing"
    );

    // Create, then replace.
    let created = raw(
        &server,
        "PUT",
        "/v1/contexts/team.memory/files/notes/plan.md",
        k,
        Some(b"v1".to_vec()),
        None,
    )
    .await;
    assert_eq!(created.status, 201, "{:?}", created.json());
    let v1 = sha256(b"v1");
    assert_eq!(created.header("etag"), Some(format!("\"{v1}\"").as_str()));
    let meta = created.json();
    assert_eq!(meta["path"], "notes/plan.md");
    assert_eq!(meta["size_bytes"], 2);
    assert_eq!(meta["sha256"], v1);
    let replaced = raw(
        &server,
        "PUT",
        "/v1/contexts/team.memory/files/notes/plan.md",
        k,
        Some(b"v2".to_vec()),
        Some(&format!("\"{v1}\"")),
    )
    .await;
    assert_eq!(replaced.status, 200, "{:?}", replaced.json());
    let v2 = sha256(b"v2");

    // A stale If-Match is refused and nothing changes.
    for (method, body) in [("PUT", Some(b"v3".to_vec())), ("DELETE", None)] {
        let stale = raw(
            &server,
            method,
            "/v1/contexts/team.memory/files/notes/plan.md",
            k,
            body,
            Some(&v1),
        )
        .await;
        assert_eq!(stale.status, 412, "{method}");
        assert_eq!(stale.json()["code"], "precondition_failed", "{method}");
        assert_no_host_path(&s, &stale.bytes);
    }
    // If-Match on a file that does not exist yet is a precondition failure too.
    let absent = raw(
        &server,
        "PUT",
        "/v1/contexts/team.memory/files/other.md",
        k,
        Some(b"x".to_vec()),
        Some(&v1),
    )
    .await;
    assert_eq!(absent.status, 412);
    assert_eq!(absent.json()["code"], "precondition_failed");

    // Download: the bytes, the ETag and the inert-content headers.
    let fetched = raw(
        &server,
        "GET",
        "/v1/contexts/team.memory/files/notes/plan.md",
        k,
        None,
        None,
    )
    .await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.bytes, b"v2");
    assert_eq!(fetched.header("etag"), Some(format!("\"{v2}\"").as_str()));
    assert_eq!(fetched.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(fetched.header("content-disposition"), Some("attachment"));

    // The 0139 list shape, with offset pagination.
    for name in ["a.md", "b.md"] {
        let put = raw(
            &server,
            "PUT",
            &format!("/v1/contexts/team.memory/files/{name}"),
            k,
            Some(name.as_bytes().to_vec()),
            None,
        )
        .await;
        assert_eq!(put.status, 201);
    }
    let first = raw(
        &server,
        "GET",
        "/v1/contexts/team.memory?limit=2",
        k,
        None,
        None,
    )
    .await;
    assert_eq!(first.status, 200);
    let first = first.json();
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{first}");
    assert_eq!(items[0]["path"], "a.md");
    assert_eq!(items[0]["size_bytes"], 4);
    assert_eq!(items[0]["sha256"], sha256(b"a.md"));
    chrono::DateTime::parse_from_rfc3339(items[0]["modified_at"].as_str().unwrap())
        .expect("modified_at is RFC 3339");
    let cursor = first["next_cursor"].as_str().expect("a next page");
    let rest = raw(
        &server,
        "GET",
        &format!("/v1/contexts/team.memory?limit=2&cursor={cursor}"),
        k,
        None,
        None,
    )
    .await
    .json();
    assert_eq!(rest["items"][0]["path"], "notes/plan.md", "{rest}");
    assert_eq!(rest["next_cursor"], Value::Null);
    assert_no_host_path(&s, &rest.to_string().into_bytes());

    // Delete with the right ETag, then the file is gone.
    let deleted = raw(
        &server,
        "DELETE",
        "/v1/contexts/team.memory/files/notes/plan.md",
        k,
        None,
        Some(&format!("\"{v2}\"")),
    )
    .await;
    assert_eq!(deleted.status, 204);
    for method in ["GET", "DELETE"] {
        let gone = raw(
            &server,
            method,
            "/v1/contexts/team.memory/files/notes/plan.md",
            k,
            None,
            None,
        )
        .await;
        assert_eq!(gone.status, 404, "{method}");
        assert_eq!(gone.json()["code"], "not_found", "{method}");
    }
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_keys_paths_and_oversized_bodies_are_refused() {
    let s =
        support::sandbox("daemon::contexts::invalid_keys_paths_and_oversized_bodies_are_refused");
    let server = Api::with_config(&s, worker_config(&[])).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let k = &key.key;
    for (method, path) in [
        ("GET", "/v1/contexts/Bad_Key"),
        ("GET", "/v1/contexts/-lead"),
        ("PUT", "/v1/contexts/Bad_Key/files/a.md"),
        // Percent-encoded so the client does not normalise it away.
        ("PUT", "/v1/contexts/k/files/%2E%2E%2F%2E%2E%2Fescape.md"),
        ("GET", "/v1/contexts/k/files/notes%2F..%2F..%2Fescape.md"),
        ("PUT", "/v1/contexts/k/files/a%5Cb.md"),
        ("PUT", "/v1/contexts/k/files/a%3Ab.md"),
        ("DELETE", "/v1/contexts/k/files/.%2Fa.md"),
        ("PUT", "/v1/contexts/k/files/notes//a.md"),
    ] {
        let body = (method == "PUT").then(|| b"x".to_vec());
        let refused = raw(&server, method, path, k, body, None).await;
        assert_eq!(refused.status, 400, "{method} {path}: {:?}", refused.json());
        assert_eq!(refused.json()["code"], "invalid_request", "{method} {path}");
        assert_no_host_path(&s, &refused.bytes);
    }
    assert!(!keyed_dir(&s, "k").exists(), "nothing was written");

    // A weak ETag is refused rather than compared.
    let weak = raw(
        &server,
        "PUT",
        "/v1/contexts/k/files/a.md",
        k,
        Some(b"x".to_vec()),
        Some("W/\"abc\""),
    )
    .await;
    assert_eq!(weak.status, 400);

    // Per-file cap on the received bytes.
    let limit = awman::data::context::ContextLimits::REPO.max_file_bytes as usize;
    let big = raw(
        &server,
        "PUT",
        "/v1/contexts/k/files/big.bin",
        k,
        Some(vec![b'x'; limit + 1]),
        None,
    )
    .await;
    assert_eq!(big.status, 413);
    assert_eq!(big.json()["code"], "context_too_large");
    let exact = raw(
        &server,
        "PUT",
        "/v1/contexts/k/files/big.bin",
        k,
        Some(vec![b'x'; limit]),
        None,
    )
    .await;
    if cfg!(unix) {
        assert_eq!(exact.status, 201);
    } else {
        assert_eq!(exact.status, 500);
        assert_eq!(exact.json()["code"], "internal");
        assert_no_host_path(&s, &exact.bytes);
        assert!(!keyed_dir(&s, "k").exists());
    }

    // A symlink planted in the keyed directory is never followed.
    #[cfg(unix)]
    {
        let outside = s.root().join("outside.txt");
        std::fs::write(&outside, "host secret").unwrap();
        std::os::unix::fs::symlink(&outside, keyed_dir(&s, "k").join("link.txt")).unwrap();
        let link = raw(
            &server,
            "GET",
            "/v1/contexts/k/files/link.txt",
            k,
            None,
            None,
        )
        .await;
        assert_eq!(link.status, 404);
        let listed = raw(&server, "GET", "/v1/contexts/k", k, None, None)
            .await
            .json();
        let paths: Vec<_> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["path"].clone())
            .collect();
        assert_eq!(paths, vec![json!("big.bin")]);
        let overwrite = raw(
            &server,
            "PUT",
            "/v1/contexts/k/files/link.txt",
            k,
            Some(b"pwned".to_vec()),
            None,
        )
        .await;
        assert_ne!(overwrite.status / 100, 5, "{:?}", overwrite.json());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host secret");
    }
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_routes_are_concealed_on_a_listener_without_the_worker_feature() {
    let s = support::sandbox(
        "daemon::contexts::context_routes_are_concealed_on_a_listener_without_the_worker_feature",
    );
    // The default fixture listener exposes only `engine`.
    let server = Api::start(&s).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    for (method, path) in [
        ("GET", "/v1/contexts/k"),
        ("GET", "/v1/contexts/k/files/a.md"),
        ("PUT", "/v1/contexts/k/files/a.md"),
        ("DELETE", "/v1/contexts/k/files/a.md"),
    ] {
        let body = (method == "PUT").then(|| b"x".to_vec());
        let hidden = raw(&server, method, path, &key.key, body, None).await;
        assert_eq!(hidden.status, 404, "{method} {path}");
    }
    assert!(!keyed_dir(&s, "k").exists());
    server.shutdown().await;
}

/// A git repository at `<work>/<name>` whose `origin` is `url`.
fn repo_with_origin(s: &TestSandbox, name: &str, url: &str) -> std::path::PathBuf {
    let repo = RepoBuilder::new(s, name).file("README.md", name).init();
    repo.git(&["remote", "add", "origin", url]);
    repo.path().to_path_buf()
}

async fn create_session(server: &Api, key: &str, body: Value) -> (u16, Value) {
    server
        .request("POST", "/v1/worker/sessions", Some(key), None, Some(body))
        .await
}

/// Setup ends `ready`, or `failed` at the ready checks (this harness has no
/// container runtime). Either way the session is open with its binding.
async fn wait_setup(server: &Api, key: &str, id: &str) -> Value {
    let session = wait_for_async(
        "session setup finished",
        Duration::from_secs(30),
        || async {
            let (code, session) = server
                .get(&format!("/v1/worker/sessions/{id}"), Some(key))
                .await;
            assert_eq!(code, 200, "{session}");
            matches!(session["setup_status"].as_str(), Some("ready" | "failed")).then_some(session)
        },
    )
    .await;
    assert!(
        session["setup_status"] == "ready" || session["setup_error"]["stage"] == "ready",
        "{session}"
    );
    session
}

/// The repo directory the live session's `context(repo)` resolves to.
async fn resolved_repo_dir(server: &Api, id: &str) -> std::path::PathBuf {
    let worker = server.api.worker.as_ref().unwrap();
    let session = worker.sessions.get_by_key(id).expect("live session");
    let session = session.read().await;
    let resolver =
        awman::data::fs::context_dirs::ContextDirResolver::from_env(session.env()).unwrap();
    let remote = worker
        .engines
        .git_engine
        .remote_effective_url(session.git_root(), "origin");
    awman::command::context::scope::RepoContextScope::new(&resolver, &session)
        .resolve(None, remote.as_deref())
        .host_dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_context_key_is_validated_stored_and_restored() {
    let s =
        support::sandbox("daemon::contexts::session_context_key_is_validated_stored_and_restored");
    s.require::<LocalGit>();
    // Two clones of "the same" project over different hosts, plus a third
    // repository without a key.
    let a = repo_with_origin(&s, "a", "https://github.com/acme/app.git");
    let b = repo_with_origin(&s, "b", "https://gitlab.example/mirror/app-fork.git");
    let config = worker_config(&[&a, &b]);
    let server = Api::with_config(&s, config.clone()).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);

    for bad in ["Team", "../x", "", "-x", &"k".repeat(129)] {
        let (code, refused) = create_session(
            &server,
            &key.key,
            json!({"session_type":"local","workdir":a,"context_key":bad}),
        )
        .await;
        assert_eq!(code, 400, "{bad}: {refused}");
        assert_eq!(refused["code"], "invalid_request", "{bad}");
    }
    let (_, sessions) = server.get("/v1/worker/sessions", Some(&key.key)).await;
    assert_eq!(
        sessions["items"],
        json!([]),
        "a refused key persists nothing"
    );

    let mut ids = Vec::new();
    for (dir, context_key) in [(&a, Some("acme.app")), (&b, Some("acme.app")), (&b, None)] {
        let mut body = json!({"session_type":"local","workdir":dir});
        if let Some(k) = context_key {
            body["context_key"] = json!(k);
        }
        let (code, created) = create_session(&server, &key.key, body).await;
        assert!(code == 201 || code == 202, "{created}");
        assert_eq!(created["context_key"], json!(context_key), "{created}");
        let id = created["id"].as_str().unwrap().to_owned();
        let ready = wait_setup(&server, &key.key, &id).await;
        assert_eq!(ready["context_key"], json!(context_key), "{ready}");
        assert_no_host_path(&s, ready["context_key"].to_string().as_bytes());
        ids.push(id);
    }
    let keyed = keyed_dir(&s, "acme.app");
    // Same key over different origins: one directory. Without a key the
    // session keeps the origin-inferred directory.
    assert_eq!(resolved_repo_dir(&server, &ids[0]).await, keyed);
    assert_eq!(resolved_repo_dir(&server, &ids[1]).await, keyed);
    let inferred = resolved_repo_dir(&server, &ids[2]).await;
    assert_ne!(inferred, keyed);
    assert!(
        inferred.ends_with("mirror/app-fork"),
        "{}",
        inferred.display()
    );

    // The binding survives a daemon restart.
    server.shutdown().await;
    let server = Api::restart_with_config(&s, config).await;
    for (id, expected) in [(&ids[0], json!("acme.app")), (&ids[2], Value::Null)] {
        let (code, session) = server
            .get(&format!("/v1/worker/sessions/{id}"), Some(&key.key))
            .await;
        assert_eq!(code, 200, "{session}");
        assert_eq!(session["context_key"], expected, "{session}");
    }
    assert_eq!(resolved_repo_dir(&server, &ids[1]).await, keyed);
    assert_eq!(resolved_repo_dir(&server, &ids[2]).await, inferred);
    server.shutdown().await;
}

/// A running command row in `session` and its tracker, begun the way the
/// queue worker begins it (this harness cannot run an agent container).
async fn start_tracked_command(
    server: &Api,
    session_id: &str,
) -> (
    String,
    awman::command::context::change_tracker::ContextChangeTracker,
) {
    let worker = server.api.worker.as_ref().unwrap();
    let id = awman::data::ids::CommandId::new().to_string();
    worker
        .store
        .insert_command(&id, session_id, "exec prompt", "[]", "log")
        .unwrap();
    worker
        .store
        .update_command_started(&id, &chrono::Utc::now().to_rfc3339())
        .unwrap();
    let attempt = worker.store.get_command(&id).unwrap().unwrap().attempt;
    let session = worker
        .sessions
        .get_by_key(session_id)
        .expect("live session");
    let session = session.read().await;
    let tracker = awman::command::context::change_tracker::ContextChangeTracker::begin(
        &worker.paths,
        &session,
        &worker.engines.git_engine,
        session_id,
        &id,
        attempt,
    );
    (id, tracker)
}

async fn changes(server: &Api, key: &str, command_id: &str) -> Value {
    let (code, body) = server
        .get(
            &format!("/v1/worker/commands/{command_id}/context_changes"),
            Some(key),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    body
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_context_changes_list_added_modified_and_deleted_files() {
    let s = support::sandbox(
        "daemon::contexts::command_context_changes_list_added_modified_and_deleted_files",
    );
    s.require::<LocalGit>();
    let a = repo_with_origin(&s, "a", "https://github.com/acme/app.git");
    let server = Api::with_config(&s, worker_config(&[&a])).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let k = &key.key;
    let (_, created) = create_session(
        &server,
        k,
        json!({"session_type":"local","workdir":a,"context_key":"acme.app"}),
    )
    .await;
    let session_id = created["id"].as_str().unwrap().to_owned();
    wait_setup(&server, k, &session_id).await;
    for (name, body) in [
        ("keep.md", "same"),
        ("edit.md", "before"),
        ("drop.md", "bye"),
    ] {
        let put = raw(
            &server,
            "PUT",
            &format!("/v1/contexts/acme.app/files/{name}"),
            k,
            Some(body.as_bytes().to_vec()),
            None,
        )
        .await;
        if cfg!(unix) {
            assert_eq!(put.status, 201);
        } else {
            assert_eq!(put.status, 500);
            assert_eq!(put.json()["code"], "internal");
            // Seed the files as an agent would through its mounted directory;
            // Windows' HTTP context API itself remains read-only.
            let dir = keyed_dir(&s, "acme.app");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), body).unwrap();
        }
    }

    let unknown = awman::data::ids::CommandId::new().to_string();
    let (code, missing) = server
        .get(
            &format!("/v1/worker/commands/{unknown}/context_changes"),
            Some(k),
        )
        .await;
    assert_eq!(code, 404, "{missing}");
    assert_eq!(missing["code"], "command_not_found");

    let (command_id, tracker) = start_tracked_command(&server, &session_id).await;
    assert_eq!(
        changes(&server, k, &command_id).await,
        json!({"items": [], "next_cursor": null})
    );
    // The agent writes the mounted directory; the API edits it as well.
    let dir = keyed_dir(&s, "acme.app");
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    std::fs::write(dir.join("notes/new.md"), "added").unwrap();
    std::fs::write(dir.join("edit.md"), "after").unwrap();
    let deleted = raw(
        &server,
        "DELETE",
        "/v1/contexts/acme.app/files/drop.md",
        k,
        None,
        None,
    )
    .await;
    if cfg!(unix) {
        assert_eq!(deleted.status, 204);
    } else {
        assert_eq!(deleted.status, 500);
        assert_eq!(deleted.json()["code"], "internal");
        assert_eq!(std::fs::read(dir.join("drop.md")).unwrap(), b"bye");
        std::fs::remove_file(dir.join("drop.md")).unwrap();
    }
    let expected = json!({"items": [
        {"path": "drop.md", "change": "deleted"},
        {"path": "edit.md", "change": "modified", "size_bytes": 5, "sha256": sha256(b"after")},
        {"path": "notes/new.md", "change": "added", "size_bytes": 5, "sha256": sha256(b"added")},
    ], "next_cursor": null});
    // In flight: a live diff against the start snapshot.
    let live = changes(&server, k, &command_id).await;
    assert_eq!(live, expected);
    assert_no_host_path(&s, live.to_string().as_bytes());

    // Finished: the persisted record; later edits no longer show.
    tracker.finish();
    server
        .api
        .worker
        .as_ref()
        .unwrap()
        .store
        .complete_command(
            &command_id,
            awman::data::command_status::CommandStatus::Done,
            Some(0),
            None,
        )
        .unwrap();
    std::fs::write(dir.join("late.md"), "after the command").unwrap();
    assert_eq!(changes(&server, k, &command_id).await, expected);
    let (_, page) = server
        .get(
            &format!("/v1/worker/commands/{command_id}/context_changes?limit=2"),
            Some(k),
        )
        .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 2, "{page}");
    assert!(page["next_cursor"].is_string(), "{page}");

    // The record outlives the daemon process.
    server.shutdown().await;
    let server = Api::restart_with_config(&s, worker_config(&[&a])).await;
    assert_eq!(changes(&server, k, &command_id).await, expected);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_commands_on_one_key_show_the_last_writer() {
    let s =
        support::sandbox("daemon::contexts::concurrent_commands_on_one_key_show_the_last_writer");
    s.require::<LocalGit>();
    let a = repo_with_origin(&s, "a", "https://github.com/acme/app.git");
    let b = repo_with_origin(&s, "b", "https://gitlab.example/acme/app.git");
    let server = Api::with_config(&s, worker_config(&[&a, &b])).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let k = &key.key;
    let mut sessions = Vec::new();
    for dir in [&a, &b] {
        let (_, created) = create_session(
            &server,
            k,
            json!({"session_type":"local","workdir":dir,"context_key":"shared"}),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_owned();
        wait_setup(&server, k, &id).await;
        sessions.push(id);
    }
    let (first, first_tracker) = start_tracked_command(&server, &sessions[0]).await;
    let (second, second_tracker) = start_tracked_command(&server, &sessions[1]).await;
    // Both agents write one file; the second write lands last.
    let dir = keyed_dir(&s, "shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("memory.md"), "from the first job").unwrap();
    std::fs::write(dir.join("memory.md"), "from the second job").unwrap();
    first_tracker.finish();
    second_tracker.finish();
    let last = sha256(b"from the second job");
    for command in [&first, &second] {
        let listed = changes(&server, k, command).await;
        assert_eq!(listed["items"][0]["path"], "memory.md", "{listed}");
        assert_eq!(listed["items"][0]["change"], "added");
        // The first job's client sees content it did not write: the overwrite.
        assert_eq!(listed["items"][0]["sha256"], last, "{listed}");
    }
    let fetched = raw(
        &server,
        "GET",
        "/v1/contexts/shared/files/memory.md",
        k,
        None,
        None,
    )
    .await;
    assert_eq!(fetched.bytes, b"from the second job");
    server.shutdown().await;
}
