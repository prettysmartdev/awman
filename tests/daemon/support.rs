//! Shared fixtures for the daemon suite.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api_process::{ApiHome, ApiServer};

pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed repo the worker may open as a local session.
pub struct Repo {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

impl Repo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(path.join(".awman")).unwrap();
        std::fs::write(path.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        std::fs::write(path.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
        std::fs::write(path.join("shared.txt"), "line one\n").unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        git(&path, &["config", "user.name", "Fixture"]);
        git(&path, &["config", "user.email", "fixture@awman.test"]);
        git(&path, &["add", "-A"]);
        git(&path, &["commit", "-q", "-m", "base"]);
        Self { _dir: dir, path }
    }

    /// Allow-list this repo for the worker's local sessions.
    pub fn allow_in(&self, home: &ApiHome, extra: Value) {
        let mut config = json!({"daemon":{"features":{"worker":{"push":{"workDirs":{"allow":[self.path.display().to_string()]}}}}}});
        merge(&mut config, extra);
        home.write_config(config);
    }

    pub async fn session(&self, server: &ApiServer) -> String {
        server
            .ready_session(json!({"session_type": "local", "workdir": self.path}))
            .await
    }
}

/// Recursively merge `extra` into `base`.
pub fn merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(base), Value::Object(extra)) => {
            for (k, v) in extra {
                merge(base.entry(k).or_insert(Value::Null), v);
            }
        }
        (base, extra) => *base = extra,
    }
}

pub async fn http(
    server: &ApiServer,
    method: reqwest::Method,
    path: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new().request(method, format!("{}{path}", server.url));
    if let Some(session) = session {
        request = request.header("x-awman-session", session);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

pub async fn submit(server: &ApiServer, session: &str, args: &[&str]) -> String {
    let (status, body) = http(
        server,
        reqwest::Method::POST,
        "/v1/worker/commands",
        Some(session),
        Some(json!({"subcommand": "exec prompt", "args": args})),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    body["id"].as_str().unwrap().to_string()
}

pub async fn wait_status(server: &ApiServer, id: &str, wanted: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let record = server.get(&format!("/v1/worker/commands/{id}")).await;
        if wanted.contains(&record["status"].as_str().unwrap_or("")) {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "command {id} never reached {wanted:?}: {record}\n{}",
            server.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the fake slow agent has started.
pub async fn wait_slow_agent(home: &ApiHome) {
    let pid = home.control().join("slow.pid");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !pid.exists() {
        assert!(Instant::now() < deadline, "slow agent never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// End the slow fake agent the way `docker stop` would, letting it finish.
pub fn release_slow_agent(home: &ApiHome) {
    for file in ["slow.child", "slow.pid"] {
        if let Ok(pid) = std::fs::read_to_string(home.control().join(file)) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }
}

/// Poll `check` against `GET /v1/status` until it returns true.
pub async fn wait_for_status(
    server: &ApiServer,
    what: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let status = server.get("/v1/status").await;
        if check(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "status never showed {what}: {status}\n{}",
            server.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A seeded squad task, created through the daemon's own API.
pub async fn create_squad_task(server: &ApiServer, name: &str, repo: &Path) -> String {
    let (status, body) = http(
        server,
        reqwest::Method::POST,
        "/v1/squad/tasks",
        None,
        Some(json!({
            "name": name,
            "description": "daemon e2e",
            "workspace": {"kind": "custom", "path": repo},
            "mount_scope": "gitroot",
            "interval_ms": 3_600_000,
            "agent": "claude",
        })),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    body["id"].as_str().unwrap().to_string()
}
