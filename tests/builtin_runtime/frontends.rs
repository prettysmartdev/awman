//! Actual-awman frontend scenarios on the builtin runtime: workflows (setup,
//! dependent steps, teardown, failure), the API server, owner crash and
//! orphan cleanup. Hardware tests: real guests booted by the real `awman`
//! binary, with a synthetic agent mounted by overlay so no paid agent or host
//! credential is used, and `docker`/`container`/`sbx` stubs on PATH that
//! record any host container-helper call.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::lifecycle::{awman, gate, install_ready_ping_stub, run_bounded};

/// One isolated awman project: a git repo, a private HOME/config, a private
/// state dir, the synthetic agent and the helper traps.
pub(crate) struct Project {
    pub temp: tempfile::TempDir,
    /// Kept alive for the project's lifetime: the builtin state directory.
    _state: tempfile::TempDir,
    pub repo: PathBuf,
    pub home: PathBuf,
    pub bin: PathBuf,
    pub binary: PathBuf,
    pub marker: PathBuf,
    /// The overlay that mounts the synthetic agent, for commands that take
    /// their own overlays (squad tasks).
    pub agent_overlay: String,
}

/// Synthetic `claude`: records its argv and prompt, then behaves as the
/// prompt asks. `exit=N` exits N; `sleep=N` sleeps first; `write=PATH` appends
/// the prompt to PATH (inside the guest workspace).
const SYNTHETIC_AGENT: &str = r#"#!/bin/sh
prompt=""
if ! IFS= read -r -t 20 prompt; then prompt="$*"; fi
case "$prompt" in *"sleep="*) n=${prompt##*sleep=}; sleep "${n%% *}";; esac
printf 'SYNTHETIC_UID=%s\n' "$(id -u)"
printf 'SYNTHETIC_PROMPT=%s\n' "$prompt"
if [ -e /compat/check-image ]; then echo SYNTHETIC_IMAGE=fixture; else echo SYNTHETIC_IMAGE=other; fi
case "$prompt" in *"write="*) f=${prompt##*write=}; printf '%s\n' "$prompt" >> "${f%% *}";; esac
case "$prompt" in *"exit="*) n=${prompt##*exit=}; exit "${n%% *}";; esac
# As a squad task-evaluation leader: report a verdict where the daemon reads it.
if [ -d /awman/squad/run ]; then
  printf '{"triggered":false,"reason":"synthetic-builtin-leader"}' > /awman/squad/run/verdict.json
fi
exit 0
"#;

impl Project {
    pub(crate) fn new(binary: &Path, archive: &Path, prefix: &str) -> Self {
        Self::with_runtime(binary, archive, prefix, "builtin-experimental")
    }

    /// As [`Project::new`], with an explicit `runtime` value (an older awman
    /// spells the builtin runtime `builtin`).
    pub(crate) fn with_runtime(binary: &Path, archive: &Path, prefix: &str, runtime: &str) -> Self {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/tmp")
            .unwrap();
        let state = tempfile::Builder::new()
            .prefix("awb.")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        let bin = temp.path().join("bin");
        for dir in [repo.join(".awman"), home.join(".awman"), bin.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        std::fs::write(
            repo.join(".awman/Dockerfile.claude"),
            "FROM awman-repo:latest\n",
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), "fixture repo\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "init",
        ]);
        let agent = temp.path().join("claude");
        std::fs::write(&agent, SYNTHETIC_AGENT).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            repo.join(".awman/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "agent": "claude", "auth": "none", "autoAgentAuthAccepted": true,
                "overlays": [format!("dir({}:/usr/local/bin/claude:ro)", agent.display())]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.join(".awman/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "runtime": runtime, "default_agent": "claude",
                "auth": "none", "autoAgentAuthAccepted": true,
                "builtin": {
                    "stateDir": state.path().join("s"),
                    "imageSource": {"type": "archive", "path": archive},
                    "network": {"mode": "none"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let marker = temp.path().join("helper-called");
        for helper in ["docker", "container", "sbx"] {
            let path = bin.join(helper);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' {helper} >> '{}'\nexit 91\n",
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        install_ready_ping_stub(&bin, "claude", &temp.path().join("ready-pings"));
        let agent_overlay = format!("dir({}:/usr/local/bin/claude:ro)", agent.display());
        let project = Self {
            temp,
            _state: state,
            repo,
            home,
            bin,
            binary: binary.to_path_buf(),
            marker,
            agent_overlay,
        };
        let ready = run_bounded(project.cmd(&["ready", "--json"]), Duration::from_secs(300));
        assert!(
            ready.status.success(),
            "ready failed: {}",
            String::from_utf8_lossy(&ready.stderr)
        );
        project
    }

    /// Switch the global runtime value and the binary driving this project,
    /// keeping its repo, HOME and builtin state.
    pub(crate) fn switch(&mut self, binary: &Path, runtime: &str) {
        let path = self.home.join(".awman/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["runtime"] = runtime.into();
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        self.binary = binary.to_path_buf();
    }

    /// Replace the global builtin image source (then run `ready` again).
    pub(crate) fn set_image_source(&self, source: serde_json::Value) {
        let path = self.home.join(".awman/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["builtin"]["imageSource"] = source;
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    }

    pub(crate) fn cmd(&self, args: &[&str]) -> Command {
        let mut command = awman(&self.binary, &self.repo, &self.home, &self.bin);
        command.args(args);
        command
    }

    pub(crate) fn assert_no_helper(&self) {
        assert!(
            !self.marker.exists(),
            "awman invoked a host container helper"
        );
    }

    /// Running VM workers for this binary: each VM lives in the same
    /// executable re-launched with the private `machine` route.
    pub(crate) fn live_vms(&self) -> usize {
        let out = Command::new("ps")
            .args(["-axo", "command="])
            .output()
            .expect("ps");
        let exe = self.binary.to_string_lossy().into_owned();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| {
                line.starts_with(&exe) && line[exe.len()..].trim_start().starts_with("machine")
            })
            .count()
    }
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Setup, two dependent steps and teardown all run in guests; guest writes
/// to the workspace reach the host repo; a failing step fails the workflow.
/// Agent steps see the repo at `/workspace`; setup/teardown commands run in
/// the repo's own path (as on Docker), so they use relative paths.
#[test]
fn builtin_hw_actual_awman_workflow_setup_steps_teardown_and_failure() {
    const TEST: &str = "builtin_hw_actual_awman_workflow_setup_steps_teardown_and_failure";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let p = Project::new(&binary, &archive, "awman-wf.");
    std::fs::write(
        p.repo.join("wf.toml"),
        r#"title = "builtin workflow"

[[setup]]
type = "run_shell"
command = "echo setup-ran > setup.out"

[[step]]
name = "first"
prompt = "first step write=/workspace/steps.out"

[[step]]
name = "second"
depends_on = ["first"]
prompt = "second step write=/workspace/steps.out"

[[teardown]]
type = "run_shell"
command = "test -f setup.out && wc -l < steps.out | tr -d ' ' > teardown.out"
"#,
    )
    .unwrap();
    let run = run_bounded(
        p.cmd(&["exec", "workflow", "wf.toml", "--non-interactive"]),
        Duration::from_secs(600),
    );
    assert!(run.status.success(), "workflow failed: {}", text(&run));
    let read = |name: &str| std::fs::read_to_string(p.repo.join(name)).unwrap_or_default();
    assert_eq!(read("setup.out").trim(), "setup-ran");
    let steps = read("steps.out");
    let first = steps
        .find("first step")
        .expect("first step ran in the guest");
    let second = steps
        .find("second step")
        .expect("second step ran in the guest");
    assert!(first < second, "depends_on order: {steps}");
    assert_eq!(read("teardown.out").trim(), "2", "teardown saw both steps");

    std::fs::write(
        p.repo.join("fail.toml"),
        r#"[[step]]
name = "breaks"
abort_on_failure = true
prompt = "fail exit=3"

[[step]]
name = "never"
depends_on = ["breaks"]
prompt = "must not run write=/workspace/never.out"
"#,
    )
    .unwrap();
    let failed = run_bounded(
        p.cmd(&["exec", "workflow", "fail.toml", "--non-interactive"]),
        Duration::from_secs(600),
    );
    assert!(
        !failed.status.success(),
        "a failed step must fail the workflow"
    );
    assert!(
        !p.repo.join("never.out").exists(),
        "a dependent of a failed step ran"
    );
    p.assert_no_helper();
    eprintln!("PASS: {TEST}");
}

/// The owning `awman` is killed while its agent runs. The guest does not
/// outlive the cleanup that follows (`awman clean --yes`), and a fresh run
/// afterwards works.
#[test]
fn builtin_hw_actual_awman_owner_crash_then_clean_recovers() {
    const TEST: &str = "builtin_hw_actual_awman_owner_crash_then_clean_recovers";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let p = Project::new(&binary, &archive, "awman-crash.");
    let mut child = p
        .cmd(&[
            "exec",
            "prompt",
            "--non-interactive",
            "--agent",
            "claude",
            "long sleep=60",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Wait until the guest is really running.
    let deadline = Instant::now() + Duration::from_secs(120);
    while p.live_vms() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(p.live_vms() > 0, "the agent VM never started");
    child.kill().unwrap(); // SIGKILL: no chance to clean up
    let _ = child.wait();

    let clean = run_bounded(p.cmd(&["clean", "--yes"]), Duration::from_secs(180));
    assert!(clean.status.success(), "clean failed: {}", text(&clean));
    let deadline = Instant::now() + Duration::from_secs(60);
    while p.live_vms() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(p.live_vms(), 0, "an orphaned VM survived clean");

    let again = run_bounded(
        p.cmd(&[
            "exec",
            "prompt",
            "--non-interactive",
            "--agent",
            "claude",
            "after exit=0",
        ]),
        Duration::from_secs(180),
    );
    assert!(
        again.status.success(),
        "a run after recovery failed: {}",
        text(&again)
    );
    assert!(
        text(&again).contains("SYNTHETIC_PROMPT=after exit=0"),
        "{}",
        text(&again)
    );
    p.assert_no_helper();
    eprintln!("PASS: {TEST}");
}

/// A command dispatched through the HTTP API runs the agent in a builtin
/// guest bound to the session's workdir.
#[test]
fn builtin_hw_actual_awman_api_dispatches_into_a_guest() {
    const TEST: &str = "builtin_hw_actual_awman_api_dispatches_into_a_guest";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let p = Project::new(&binary, &archive, "awman-api.");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let repo = p.repo.canonicalize().unwrap();
    // Loopback-only, plain HTTP and no key: API authentication has its own
    // suites; this scenario is about the runtime behind the API.
    let mut server = p
        .cmd(&[
            "api",
            "start",
            "--port",
            &port.to_string(),
            "--workdirs",
            &repo.to_string_lossy(),
            "--dangerously-skip-tls",
            "--dangerously-skip-auth",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _server = Kill(&mut server);
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::blocking::Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let session = loop {
        let response = http
            .post(format!("{base}/v1/sessions"))
            .json(&serde_json::json!({"type": "local", "workdir": repo}))
            .send();
        if let Ok(r) = response {
            let body: serde_json::Value = r.json().unwrap_or_default();
            if let Some(id) = body["session_id"].as_str() {
                break id.to_string();
            }
            panic!("session creation failed: {body}");
        }
        assert!(Instant::now() < deadline, "API server never came up");
        std::thread::sleep(Duration::from_millis(250));
    };
    // Session setup is asynchronous; wait for it to become ready.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let body: serde_json::Value = http
            .get(format!("{base}/v1/sessions/{session}/status"))
            .send()
            .unwrap()
            .json()
            .unwrap_or_default();
        let setup = body["status"].as_str().unwrap_or("").to_string();
        if setup == "ready" {
            break;
        }
        assert!(
            !matches!(setup.as_str(), "failed" | "error"),
            "session setup failed: {body}"
        );
        assert!(
            Instant::now() < deadline,
            "session never became ready: {body}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let command: serde_json::Value = http
        .post(format!("{base}/v1/commands"))
        .header("x-awman-session", &session)
        .json(&serde_json::json!({
            "subcommand": "exec prompt",
            "args": ["api prompt exit=0", "--non-interactive", "--agent", "claude"]
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let id = command["command_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no command id: {command}"))
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        let body: serde_json::Value = http
            .get(format!("{base}/v1/commands/{id}/status"))
            .send()
            .unwrap()
            .json()
            .unwrap();
        let state = body["status"].as_str().unwrap_or("").to_string();
        if !matches!(state.as_str(), "" | "pending" | "queued" | "running") {
            break body;
        }
        assert!(Instant::now() < deadline, "command never finished: {body}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let logs = http
        .get(format!("{base}/v1/commands/{id}/logs"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        logs.contains("SYNTHETIC_PROMPT=api prompt exit=0"),
        "status {status}; logs: {logs}"
    );
    assert!(logs.contains("SYNTHETIC_UID=1234"), "{logs}");
    p.assert_no_helper();
    eprintln!("PASS: {TEST}: {status}");
}

/// The squad daemon dispatches a task evaluation into a builtin guest: the
/// leader runs with the run directory mounted, writes its verdict, and the
/// daemon records it.
#[test]
fn builtin_hw_actual_awman_squad_dispatches_a_leader_into_a_guest() {
    const TEST: &str = "builtin_hw_actual_awman_squad_dispatches_a_leader_into_a_guest";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let p = Project::new(&binary, &archive, "awman-squad.");
    // Foreground and loopback-only without a key: the daemon stays a child
    // of this test (no launchd job); squad authentication has its own suites.
    let mut daemon = p
        .cmd(&["squad", "start", "--dangerously-skip-auth"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _daemon = Kill(&mut daemon);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !run_bounded(p.cmd(&["squad", "status"]), Duration::from_secs(30))
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "the squad daemon never came up");
        std::thread::sleep(Duration::from_millis(500));
    }
    let add = run_bounded(
        p.cmd(&[
            "squad",
            "add",
            "--name",
            "builtin-probe",
            "--description",
            "Probe the builtin runtime",
            "--interval",
            "6h",
            "--overlay",
            &p.agent_overlay,
            "--non-interactive",
        ]),
        Duration::from_secs(120),
    );
    assert!(add.status.success(), "squad add failed: {}", text(&add));
    let trigger = run_bounded(
        p.cmd(&["squad", "trigger", "builtin-probe"]),
        Duration::from_secs(60),
    );
    assert!(
        trigger.status.success(),
        "trigger failed: {}",
        text(&trigger)
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    let shown = loop {
        let show = run_bounded(
            p.cmd(&["squad", "show", "builtin-probe", "--json"]),
            Duration::from_secs(60),
        );
        let body = String::from_utf8_lossy(&show.stdout).into_owned();
        if body.contains("synthetic-builtin-leader") {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "no verdict recorded: {}",
            text(&show)
        );
        std::thread::sleep(Duration::from_secs(2));
    };
    assert!(
        shown.contains("not_triggered") || shown.contains("not triggered"),
        "{shown}"
    );
    p.assert_no_helper();
    eprintln!("PASS: {TEST}");
}

/// Two genuine awman builds share one project and builtin state: the older
/// build (runtime spelled `builtin`) imports and runs, the newer build
/// (`builtin-experimental`) reuses that state, and the older build
/// explicitly refuses the upgraded catalog without damaging it. Binary
/// replacement is exercised independently below.
///
/// `AWMAN_TEST_BUILTIN_OLD_ARTIFACT` names the older, entitlement-signed
/// build; without it the test is BLOCKED (REQUIRE_HW fails it).
#[test]
fn builtin_hw_actual_awman_old_new_forward_upgrade_and_old_reader_refusal() {
    const TEST: &str = "builtin_hw_actual_awman_old_new_forward_upgrade_and_old_reader_refusal";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let Some(old) = std::env::var_os("AWMAN_TEST_BUILTIN_OLD_ARTIFACT").map(PathBuf::from) else {
        eprintln!("BLOCKED: {TEST}: AWMAN_TEST_BUILTIN_OLD_ARTIFACT is not set");
        assert!(
            std::env::var("AWMAN_TEST_BUILTIN_REQUIRE_HW").is_err(),
            "{TEST}: an older awman build is required"
        );
        return;
    };
    let run = |p: &Project, prompt: &str| {
        let out = run_bounded(
            p.cmd(&[
                "exec",
                "prompt",
                "--non-interactive",
                "--agent",
                "claude",
                prompt,
            ]),
            Duration::from_secs(180),
        );
        (out.status.code(), text(&out))
    };
    // 1. Old build: import and run.
    let mut p = Project::with_runtime(&old, &archive, "awman-upg.", "builtin");
    let (code, out) = run(&p, "old one exit=0");
    assert_eq!(code, Some(0), "old build run: {out}");
    // 2. New build on the old state.
    p.switch(&binary, "builtin-experimental");
    let ready = run_bounded(p.cmd(&["ready", "--json"]), Duration::from_secs(300));
    assert!(
        ready.status.success(),
        "new ready on old state: {}",
        text(&ready)
    );
    let (code, out) = run(&p, "new one exit=0");
    assert_eq!(code, Some(0), "new build on old state: {out}");
    assert!(out.contains("SYNTHETIC_PROMPT=new one exit=0"), "{out}");
    // 3. Old build again, after the new build wrote the state.
    p.switch(&old, "builtin");
    let (code, out) = run(&p, "old again exit=0");
    assert_ne!(
        code,
        Some(0),
        "0.7.2 must refuse the upgraded 0.7.8 catalog: {out}"
    );
    assert!(out.contains("database schema is newer"), "{out}");
    p.switch(&binary, "builtin-experimental");
    let (code, out) = run(&p, "new after refusal exit=0");
    assert_eq!(code, Some(0), "catalog survives old-reader refusal: {out}");
    p.assert_no_helper();
    eprintln!("PASS: {TEST}");
}

/// Executable replacement has its own project and gate; downgrade refusal
/// cannot prevent this independent hardware scenario from running.
#[test]
fn builtin_hw_actual_awman_binary_replacement() {
    const TEST: &str = "builtin_hw_actual_awman_binary_replacement";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let mut p = Project::new(&binary, &archive, "awman-repl.");
    let ready = run_bounded(p.cmd(&["ready", "--json"]), Duration::from_secs(300));
    assert!(ready.status.success(), "{}", text(&ready));
    // 4. Replace the executable file while an agent runs from it: copy the new
    //    build to a private path, start a slow agent from that path, then
    //    atomically replace the file (as an installer or package manager does).
    let installed = p.temp.path().join("installed-awman");
    std::fs::copy(&binary, &installed).unwrap();
    p.switch(&installed, "builtin-experimental");
    let child = p
        .cmd(&[
            "exec",
            "prompt",
            "--non-interactive",
            "--agent",
            "claude",
            "slow sleep=15 exit=4",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while p.live_vms() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(p.live_vms() > 0, "the slow agent never started");
    let staged = p.temp.path().join("installed-awman.new");
    std::fs::copy(&binary, &staged).unwrap();
    std::fs::rename(&staged, &installed).unwrap();
    let finished = child.wait_with_output().unwrap();
    assert_eq!(
        finished.status.code(),
        Some(4),
        "the running agent's exit survives replacement: {}",
        text(&finished)
    );
    p.assert_no_helper();
    eprintln!("PASS: {TEST}");
}

/// An agent image pulled from a real public registry by `awman ready` runs
/// in a builtin guest with networking off. Opt in with
/// `AWMAN_TEST_REGISTRY_PUBLIC=1` (it contacts Docker Hub).
#[test]
fn builtin_hw_actual_awman_registry_pulled_image_runs_offline() {
    const TEST: &str = "builtin_hw_actual_awman_registry_pulled_image_runs_offline";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    if std::env::var("AWMAN_TEST_REGISTRY_PUBLIC").as_deref() != Ok("1") {
        eprintln!("SKIP: {TEST}: AWMAN_TEST_REGISTRY_PUBLIC=1 is not set");
        return;
    }
    let p = Project::new(&binary, &archive, "awman-reg.");
    p.set_image_source(serde_json::json!({
        "type": "registry", "registry": "docker.io", "reference": "library/alpine:3.22"
    }));
    let ready = run_bounded(p.cmd(&["ready", "--json"]), Duration::from_secs(600));
    let summary = String::from_utf8_lossy(&ready.stdout).into_owned();
    assert!(
        ready.status.success(),
        "ready from the registry: {}",
        text(&ready)
    );
    assert!(summary.contains("\"ready\": true"), "{summary}");
    let out = run_bounded(
        p.cmd(&[
            "exec",
            "prompt",
            "--non-interactive",
            "--agent",
            "claude",
            "from registry exit=0",
        ]),
        Duration::from_secs(180),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("SYNTHETIC_PROMPT=from registry exit=0"),
        "{}",
        text(&out)
    );
    assert!(
        text(&out).contains("SYNTHETIC_IMAGE=other"),
        "the guest ran the fixture, not the registry image: {}",
        text(&out)
    );
    p.assert_no_helper();
    eprintln!("PASS: {TEST}");
}
