//! A real `awman api start` process with a fake `docker` on its `PATH`
//! (WI 0133).
//!
//! The remote-session flows — clone, branch, merge, run a command, push — span
//! the HTTP layer, session setup, the queue worker and the push stage, so these
//! tests drive the shipped binary end to end. Agent execution is faked at the
//! container CLI: [`FAKE_DOCKER`] answers awman's `docker info` / `image
//! inspect` probes and, for `docker run`, plays the agent:
//!
//! * a dynamic-workflow leader (a mount that holds `workflow-usage.md`) writes
//!   a one-step `workflow.toml` into that mount and touches nothing else;
//! * any other agent writes `agent-edit.txt` into its `/workspace` mount,
//!   unless the fixture's [`AgentMode`] says otherwise.
//!
//! The fake lives under the test's temp directory, which is the only place
//! `host_cli::program` accepts a `docker` from under `AWMAN_TEST_ISOLATION`,
//! so no real container CLI or daemon is ever reached. The server runs with a
//! throwaway `HOME`, no global or system git config (so the clone has no git
//! identity) and `AWMAN_TEST_ISOLATION=1`, whatever the calling shell has.
//!
//! Included with `#[path = "../helpers/api_process.rs"] mod api_process;`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The fake container CLI. `$FAKE_DOCKER_CONTROL/mode` selects the agent's
/// behaviour; every invocation is appended to `$FAKE_DOCKER_CONTROL/docker.log`.
pub const FAKE_DOCKER: &str = r#"#!/bin/sh
ctl="$FAKE_DOCKER_CONTROL"
echo "docker $*" >> "$ctl/docker.log"
case "$1" in
  info) echo "27.0.0"; exit 0 ;;
  image|images|inspect) echo '[{"Id":"sha256:fake"}]'; exit 0 ;;
  exec)
    shift
    workdir=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        -w) workdir="$2"; shift 2 ;;
        -e) export "$2"; shift 2 ;;
        sh) shift; cd "$workdir" || exit 1; exec sh "$@" ;;
        *) shift ;;
      esac
    done
    exit 2
    ;;
  run)
    mode="$(cat "$ctl/mode" 2>/dev/null)"
    workspace=""; leader=""; background=""
    for arg in "$@"; do
      case "$arg" in
        -d) background=yes ;;
        /*:/*)
          host="${arg%%:/*}"; target="${arg#"$host":}"
          case "$target" in /workspace|/workspace:*) workspace="$host" ;; esac
          [ -f "$host/workflow-usage.md" ] && leader="$host"
          ;;
      esac
    done
    [ -n "$background" ] && { echo fake-background; exit 0; }
    if [ -n "$leader" ]; then
      printf '[[steps]]\nname = "implement"\nagent = "claude"\nprompt = "implement the work item"\n' > "$leader/workflow.toml"
      [ -f "$ctl/teardown.toml" ] && cat "$ctl/teardown.toml" >> "$leader/workflow.toml"
      [ "$mode" = "leader-rewrite" ] && echo rewritten > "$workspace/shared.txt"
      [ "$mode" = "leader-commit" ] && git -C "$workspace" -c user.name=Test -c user.email=test@localhost commit --allow-empty -m forbidden
      exit 0
    fi
    [ "$mode" = "noop" ] && exit 0
    [ -n "$workspace" ] && echo "edited by the fake agent" > "$workspace/agent-edit.txt"
    if [ "$mode" = "poison" ]; then
      mkdir -p "$workspace/.git/hooks"
      printf '#!/bin/sh\necho executed >> "%s/host-executed"\n' "$ctl" > "$workspace/.git/hooks/pre-commit"
      chmod +x "$workspace/.git/hooks/pre-commit"
      cp "$workspace/.git/hooks/pre-commit" "$workspace/.git/hooks/pre-push"
      cp "$workspace/.git/hooks/pre-commit" "$workspace/.git/hooks/reference-transaction"
      cat >> "$workspace/.git/config" <<CONFIG
[core]
    fsmonitor = $workspace/.git/hooks/pre-commit
[filter "planted"]
    clean = $workspace/.git/hooks/pre-commit
[credential]
    helper = !$workspace/.git/hooks/pre-commit
[remote "origin"]
    pushurl = ext::$workspace/.git/hooks/pre-commit
CONFIG
      echo '*.txt filter=planted' > "$workspace/.gitattributes"
    fi
    [ "$mode" = "fail" ] && exit 3
    exit 0
    ;;
  *) exit 0 ;;
esac
"#;

/// What the fake agent does in its `/workspace` on `docker run`.
#[derive(Debug, Clone, Copy)]
pub enum AgentMode {
    /// Write `agent-edit.txt` and exit 0 (the default).
    Edit,
    /// Change nothing and exit 0.
    NoOp,
    /// Write `agent-edit.txt` and exit 3.
    Fail,
    Poison,
    LeaderRewrite,
    LeaderCommit,
}

/// A throwaway awman home plus the fake container CLI. Outlives any number
/// of [`ApiServer`] processes, which is what a restart test needs.
pub struct ApiHome {
    dir: tempfile::TempDir,
}

impl ApiHome {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["home", "bin", "control"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let docker = dir.path().join("bin/docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = Self { dir };
        home.set_agent(AgentMode::Edit);
        home
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn set_agent(&self, mode: AgentMode) {
        let mode = match mode {
            AgentMode::Edit => "edit",
            AgentMode::NoOp => "noop",
            AgentMode::Fail => "fail",
            AgentMode::Poison => "poison",
            AgentMode::LeaderRewrite => "leader-rewrite",
            AgentMode::LeaderCommit => "leader-commit",
        };
        std::fs::write(self.dir.path().join("control/mode"), mode).unwrap();
    }

    pub fn control(&self) -> PathBuf {
        self.dir.path().join("control")
    }

    /// A real phase-shell PR stub records the published tip and upstream at the
    /// moment PR creation runs. No external service is contacted.
    pub fn set_teardown(&self, toml: &str) {
        std::fs::write(self.control().join("teardown.toml"), toml).unwrap();
        let gh = self.dir.path().join("bin/gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
git rev-parse HEAD > "$FAKE_DOCKER_CONTROL/pr-head"
git rev-parse --abbrev-ref '@{u}' > "$FAKE_DOCKER_CONTROL/pr-upstream"
git ls-remote origin "refs/heads/$(git branch --show-current)" > "$FAKE_DOCKER_CONTROL/pr-origin"
exit 0
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Start `awman api start` on a free loopback port and wait until it
    /// answers `/v1/status`.
    pub async fn start(&self) -> ApiServer {
        let mut last_log = String::new();
        for attempt in 0..3 {
            let port = free_port();
            let log_path = self.dir.path().join(format!("server-{port}-{attempt}.log"));
            let log = std::fs::File::create(&log_path).unwrap();
            let child = self
                .server_command(port)
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .expect("spawn awman api start");
            let mut server = ApiServer {
                child,
                url: format!("http://127.0.0.1:{port}"),
                log_path,
                client: reqwest::Client::new(),
            };
            if server.wait_until_up().await {
                return server;
            }
            last_log = server.log();
            server.kill();
        }
        panic!("awman api start never came up; last log:\n{last_log}");
    }

    fn server_command(&self, port: u16) -> Command {
        let home = self.home();
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_awman"));
        command
            .args([
                "api",
                "start",
                "--port",
                &port.to_string(),
                "--dangerously-skip-auth",
                "--dangerously-skip-tls",
            ])
            .current_dir(&home)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            // The fake docker must stay inside the server's temp dir.
            .env("TMPDIR", std::env::temp_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("AWMAN_TEST_ISOLATION", "1")
            .env("FAKE_DOCKER_CONTROL", self.dir.path().join("control"))
            .stdin(Stdio::null());
        command
    }
}

/// One running `awman api start` process; killed on drop.
pub struct ApiServer {
    child: Child,
    pub url: String,
    log_path: PathBuf,
    client: reqwest::Client,
}

impl ApiServer {
    async fn wait_until_up(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false;
            }
            let probe = self
                .client
                .get(format!("{}/v1/status", self.url))
                .send()
                .await;
            if matches!(probe, Ok(ref r) if r.status().is_success()) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Everything the server has written to stdout and stderr, including the
    /// session-setup log lines.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Stop the process the way a crash or `kill -9` would.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// `POST /v1/sessions`; returns the HTTP status and the JSON body.
    pub async fn post_session(&self, body: Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}/v1/sessions", self.url))
            .json(&body)
            .send()
            .await
            .expect("POST /v1/sessions");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Create a session that must be accepted; returns its id.
    pub async fn create_session(&self, body: Value) -> String {
        let (status, response) = self.post_session(body).await;
        assert_eq!(status, 202, "session not accepted: {response}");
        response["session_id"].as_str().unwrap().to_string()
    }

    pub async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("GET {path} body: {e}"))
    }

    /// `GET /v1/sessions/{id}`.
    pub async fn session(&self, id: &str) -> Value {
        self.get(&format!("/v1/sessions/{id}")).await
    }

    /// Poll `GET /v1/sessions/{id}/status` until setup is `ready` or `failed`.
    pub async fn wait_for_setup(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let status = self.get(&format!("/v1/sessions/{id}/status")).await;
            if matches!(status["status"].as_str(), Some("ready" | "failed")) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "setup of {id} never finished: {status}\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Create a session and wait for it to be ready; returns its id.
    pub async fn ready_session(&self, body: Value) -> String {
        let id = self.create_session(body).await;
        let status = self.wait_for_setup(&id).await;
        assert_eq!(status["status"], "ready", "setup failed: {status}");
        id
    }

    /// Queue `subcommand args` on `session` and wait for its terminal record.
    pub async fn run_command(&self, session: &str, subcommand: &str, args: &[&str]) -> Value {
        let response = self
            .client
            .post(format!("{}/v1/commands", self.url))
            .header("x-awman-session", session)
            .json(&serde_json::json!({ "subcommand": subcommand, "args": args }))
            .send()
            .await
            .expect("POST /v1/commands");
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap();
        assert!(status < 300, "command not queued ({status}): {body}");
        let command_id = body["command_id"].as_str().unwrap().to_string();

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let record = self.get(&format!("/v1/commands/{command_id}/status")).await;
            if matches!(
                record["status"].as_str(),
                Some("done" | "error" | "aborted" | "cancelled")
            ) {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "command {command_id} never finished: {record}\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.kill();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("bind an ephemeral loopback port")
}

/// The persisted event stream of a finished command (`events.log`, beside the
/// record's `log_path`). A terminal row guarantees the tail is already flushed.
pub async fn events_log(command: &Value) -> String {
    let output_log = Path::new(command["log_path"].as_str().expect("command log_path"));
    let path = output_log.with_file_name("events.log");
    let log = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        log.matches(r#""type":"Done""#).count(),
        1,
        "{}: {log}",
        path.display()
    );
    log
}

/// The clone a remote session works in, as `GET /v1/sessions/{id}` reports it.
pub fn clone_path(session: &Value) -> PathBuf {
    Path::new(session["workdir"].as_str().expect("session workdir")).to_path_buf()
}
