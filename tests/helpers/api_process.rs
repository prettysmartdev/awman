//! A real `awman daemon start` process (the worker feature) with a fake `docker` on its `PATH`
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
//! The unified daemon (WI 0140) serves its API on an owner-only local socket, not TCP
//! (the network listener needs an API key and TLS). So that the many tests which
//! speak HTTP to `ApiServer::url` stay unchanged, each [`ApiServer`] runs a
//! loopback TCP proxy that forwards raw bytes to the daemon's local socket.
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
  stop)
    # The slow fixtures model docker stop ending the running container and
    # closing its pipes, so a cancelled agent ends at once rather than when
    # its 30s sleep runs out.
    case "$(cat "$ctl/mode" 2>/dev/null)" in
      slow|chatty-slow)
        [ -f "$ctl/slow.child" ] && kill "$(cat "$ctl/slow.child")" 2>/dev/null
        [ -f "$ctl/slow.pid" ] && kill "$(cat "$ctl/slow.pid")" 2>/dev/null
        ;;
    esac
    exit 0
    ;;
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
    if [ "$mode" = "structured" ]; then cat "$ctl/agent.ndjson"; exit 0; fi
    workspace=""; leader=""; background=""; output=""
    for arg in "$@"; do
      case "$arg" in
        -d) background=yes ;;
        /*:/*)
          host="${arg%%:/*}"; target="${arg#"$host":}"
          case "$target" in /workspace|/workspace:*) workspace="$host" ;; esac
          case "$target" in /awman/output|/awman/output:*) output="$host" ;; esac
          [ -f "$host/workflow-usage.md" ] && leader="$host"
          ;;
      esac
    done
    [ -n "$background" ] && { echo fake-background; exit 0; }
    if [ -n "$leader" ]; then
      # Status regressions can hold an actual evaluation long enough to poll
      # its shared executor permit, without requiring a fabricated verdict.
      [ -f "$ctl/hold-leader" ] && sleep 5
      printf '[[steps]]\nname = "implement"\nagent = "claude"\nprompt = "implement the work item"\n' > "$leader/workflow.toml"
      [ -f "$ctl/teardown.toml" ] && cat "$ctl/teardown.toml" >> "$leader/workflow.toml"
      [ "$mode" = "leader-rewrite" ] && echo rewritten > "$workspace/shared.txt"
      [ "$mode" = "leader-commit" ] && git -C "$workspace" -c user.name=Test -c user.email=test@localhost commit --allow-empty -m forbidden
      exit 0
    fi
    [ "$mode" = "noop" ] && exit 0
    if [ "$mode" = "outputs-resume" ]; then
      # The first agent run writes an output and succeeds; every later run
      # is a slow agent, so a test can restart the daemon mid-workflow.
      if [ ! -f "$ctl/outputs-first.done" ]; then
        mkdir -p "$output/first" && echo '{"step":"first"}' > "$output/first/result.json"
        touch "$ctl/outputs-first.done"
        exit 0
      fi
      mode=slow
    fi
    if [ "$mode" = "chatty" ] || [ "$mode" = "chatty-slow" ]; then
      [ "$mode" = "chatty-slow" ] && echo "$$" > "$ctl/slow.pid"
      i=0
      while [ "$i" -lt 2000 ]; do
        printf 'burst-%s\n' "$i"
        printf 'error-%s\n' "$i" >&2
        i=$((i + 1))
      done
      printf 'LAST-STDOUT'
      printf 'LAST-STDERR' >&2
      [ "$mode" = "chatty-slow" ] && exec sleep 30
      exit 0
    fi
    if [ "$mode" = "slow" ]; then
      # slow.pid is the readiness signal, so write it once both pids exist.
      sleep 30 &
      echo "$!" > "$ctl/slow.child"
      echo "$$" > "$ctl/slow.pid"
      wait "$!"
    fi
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
    /// Sleep 30 seconds (a long-running agent a test can cancel), then edit.
    Slow,
    /// Burst on both pipes, with unterminated tails, then exit immediately.
    Chatty,
    /// Emit the same burst, then wait for cancellation.
    ChattySlow,
    LeaderRewrite,
    LeaderCommit,
    /// The first run writes `first/result.json` into `/awman/output` and exits
    /// 0; every later run behaves like [`AgentMode::Slow`].
    OutputsResume,
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
        let config = home.home().join(".config/awman");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"daemon":{"verboseSetup":true,"features":{"worker":{"enabled":true,"push":{"enabled":true}}}}}"#,
        )
        .unwrap();
        home.set_agent(AgentMode::Edit);
        home
    }

    /// Replace the global config with `config`, keeping the worker feature (and
    /// its push side) enabled: the daemon refuses to start with no feature on.
    pub fn write_config(&self, mut config: Value) {
        let worker = &mut config["daemon"]["features"]["worker"];
        if !worker.is_object() {
            *worker = serde_json::json!({});
        }
        worker["enabled"] = true.into();
        if !worker["push"].is_object() {
            worker["push"] = serde_json::json!({});
        }
        worker["push"]["enabled"] = true.into();
        let dir = self.home().join(".config/awman");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
    }

    /// Replace the global config with exactly `config` (no features forced on).
    pub fn write_config_exact(&self, config: Value) {
        let dir = self.home().join(".config/awman");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
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
            AgentMode::Slow => "slow",
            AgentMode::Chatty => "chatty",
            AgentMode::ChattySlow => "chatty-slow",
            AgentMode::LeaderRewrite => "leader-rewrite",
            AgentMode::LeaderCommit => "leader-commit",
            AgentMode::OutputsResume => "outputs-resume",
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

    /// Start `awman daemon start` (worker feature on) and wait until its local
    /// socket answers `/v1/status`; `ApiServer::url` is a loopback proxy to it.
    pub async fn start(&self) -> ApiServer {
        self.start_with_env(&[]).await
    }

    /// [`Self::start`] with extra environment variables for the server, such
    /// as a test-isolation-only timing override.
    pub async fn start_with_env(&self, env: &[(&str, &str)]) -> ApiServer {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let attempt = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log_path = self.dir.path().join(format!("daemon-{attempt}.log"));
        let log = std::fs::File::create(&log_path).unwrap();
        let child = self
            .server_command(env)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn awman daemon start");
        let mut server = ApiServer {
            child,
            url: String::new(),
            log_path,
            client: reqwest::Client::new(),
            proxy: None,
        };
        let Some(socket) = self.wait_for_socket(&mut server.child, env).await else {
            let log = server.log();
            server.kill();
            panic!("awman daemon start never came up; last log:\n{log}");
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the loopback proxy");
        server.url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        server.proxy = Some(tokio::spawn(proxy(listener, socket)));
        assert!(
            server.wait_until_up().await,
            "daemon never answered /v1/status:\n{}",
            server.log()
        );
        server
    }

    /// The daemon's local socket once this very child owns it.
    async fn wait_for_socket(&self, child: &mut Child, extra: &[(&str, &str)]) -> Option<PathBuf> {
        let home = self.home();
        let mut vars = vec![
            ("HOME", home.to_str().unwrap().to_string()),
            (
                "XDG_CONFIG_HOME",
                home.join(".config").to_str().unwrap().to_string(),
            ),
            (
                "XDG_DATA_HOME",
                home.join(".local/share").to_str().unwrap().to_string(),
            ),
        ];
        for (k, v) in extra {
            vars.retain(|(name, _)| name != k);
            vars.push((k, v.to_string()));
        }
        let env = awman::data::config::env::EnvSnapshot::with_overrides(vars);
        let paths = awman::data::fs::DataPaths::from_env(&env).ok()?.daemon();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return None;
            }
            if let Ok(Some(endpoint)) = awman::engine::remote::local::discover(&paths) {
                // A crash/restart test must kill a fully started daemon, not
                // the tiny sidecar-publication window while it still owns
                // startup.lock (whose abandoned-lock grace is 30 seconds).
                if endpoint.pid == child.id() && !paths.startup_lock_file().exists() {
                    return Some(endpoint.address);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    fn server_command(&self, env: &[(&str, &str)]) -> Command {
        self.command(env, &["daemon", "start"])
    }

    /// An `awman` invocation in this home's isolated environment (the same one
    /// its daemons run in), so it finds the daemon's local socket.
    pub fn command(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let home = self.home();
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_awman"));
        command
            .args(args)
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
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        if let Ok(backend) = std::env::var("AWMAN_TEST_BACKEND") {
            assert!(matches!(backend.as_str(), "builtin" | "daemon"));
            command.env("AWMAN_TUI_BACKEND", backend);
        }
        command
    }
}

/// One running `awman daemon start` process; killed on drop. `url` is the
/// loopback proxy to its local socket.
pub struct ApiServer {
    child: Child,
    pub url: String,
    log_path: PathBuf,
    client: reqwest::Client,
    proxy: Option<tokio::task::JoinHandle<()>>,
}

impl ApiServer {
    async fn wait_until_up(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false;
            }
            // Only our own pid counts as up.
            let probe = self
                .client
                .get(format!("{}/v1/status", self.url))
                .send()
                .await;
            if let Ok(response) = probe {
                if response.status().is_success() {
                    let body: Value = response.json().await.unwrap_or(Value::Null);
                    if body["pid"].as_u64() == Some(u64::from(self.child.id()))
                        && !body["listeners"]["local"].is_null()
                    {
                        return true;
                    }
                }
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
        if let Some(proxy) = self.proxy.take() {
            proxy.abort();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// `POST /v1/worker/sessions`; returns the HTTP status and the JSON body.
    pub async fn post_session(&self, body: Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}/v1/worker/sessions", self.url))
            .json(&body)
            .send()
            .await
            .expect("POST /v1/worker/sessions");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Create a session that must be accepted; returns its id.
    pub async fn create_session(&self, body: Value) -> String {
        let (status, response) = self.post_session(body).await;
        assert_eq!(status, 201, "session not accepted: {response}");
        response["id"].as_str().unwrap().to_string()
    }

    pub async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}\n{}", self.log()))
            .json()
            .await
            .unwrap_or_else(|e| panic!("GET {path} body: {e}"))
    }

    /// `GET /v1/worker/sessions/{id}`.
    pub async fn session(&self, id: &str) -> Value {
        self.get(&format!("/v1/worker/sessions/{id}")).await
    }

    /// Poll `GET /v1/worker/sessions/{id}` until setup is `ready` or `failed`.
    pub async fn wait_for_setup(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let status = self.get(&format!("/v1/worker/sessions/{id}")).await;
            if matches!(status["setup_status"].as_str(), Some("ready" | "failed")) {
                return status["setup"].clone();
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
            .post(format!("{}/v1/worker/commands", self.url))
            .header("x-awman-session", session)
            .json(&serde_json::json!({ "subcommand": subcommand, "args": args }))
            .send()
            .await
            .expect("POST /v1/worker/commands");
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap();
        assert!(status < 300, "command not queued ({status}): {body}");
        let command_id = body["id"].as_str().unwrap().to_string();

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let record = self.get(&format!("/v1/worker/commands/{command_id}")).await;
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

/// Forward every accepted loopback connection to the daemon's local socket,
/// byte for byte (HTTP, SSE and all).
async fn proxy(listener: tokio::net::TcpListener, socket: PathBuf) {
    while let Ok((mut tcp, _)) = listener.accept().await {
        let socket = socket.clone();
        tokio::spawn(async move {
            if let Ok(mut unix) = tokio::net::UnixStream::connect(socket).await {
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut unix).await;
            }
        });
    }
}

/// The persisted event stream of a finished command (`events.log`, beside the
/// record's `log_path`). WI 0137 §2 writes the terminal row before publishing
/// the final status and Done, so wait for the log writer to finish that tail.
pub async fn events_log(command: &Value) -> String {
    let output_log = Path::new(command["log_path"].as_str().expect("command log_path"));
    let path = output_log.with_file_name("events.log");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = match std::fs::read_to_string(&path) {
            Ok(log) => log,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("{}: {error}", path.display()),
        };
        let done = log.matches(r#""type":"Done""#).count();
        // A reader can observe a partially written JSON record. The final
        // newline, as well as Done, must be present before returning the log.
        if done > 0 && log.ends_with('\n') {
            assert_eq!(done, 1, "{}: {log}", path.display());
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "no complete Done event in {}: {log}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The clone a remote session works in, as `GET /v1/worker/sessions/{id}` reports it.
pub fn clone_path(session: &Value) -> PathBuf {
    Path::new(session["workdir"].as_str().expect("session workdir")).to_path_buf()
}

/// A daemon whose data and config live under an explicit `AWMAN_HOME` root,
/// together with the fixtures (fake `docker`, throwaway HOME) keeping it
/// alive. Dropping it stops the daemon.
pub struct RootDaemon {
    _home: ApiHome,
    pub server: ApiServer,
}

impl RootDaemon {
    /// Start `awman daemon start` with `AWMAN_HOME=root`; the caller writes
    /// `root/config.json` first (some feature must be enabled).
    pub async fn start(root: &Path) -> Self {
        let home = ApiHome::new();
        let server = home
            .start_with_env(&[("AWMAN_HOME", root.to_str().unwrap())])
            .await;
        Self {
            _home: home,
            server,
        }
    }

    /// The loopback URL that reaches the daemon's local socket.
    pub fn base(&self) -> String {
        self.server.url.clone()
    }

    pub fn log(&self) -> String {
        self.server.log()
    }

    pub fn abort(self) {}
}

impl ApiHome {
    /// Run `awman args…` to completion in this home's environment.
    pub fn cli(&self, args: &[&str]) -> std::process::Output {
        self.command(&[], args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run awman")
    }

    /// [`Self::cli`] that must exit 0; returns stdout.
    pub fn cli_ok(&self, args: &[&str]) -> String {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "awman {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// [`Self::cli`] that must exit nonzero; returns stderr.
    pub fn cli_err(&self, args: &[&str]) -> String {
        let output = self.cli(args);
        assert!(
            !output.status.success(),
            "awman {args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}
