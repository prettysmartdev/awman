//! Actual-awman lifecycle smoke. This is a hardware test, never fake-driver
//! evidence. The archive is the promoted fixture; a private file mount supplies
//! a synthetic agent executable so no paid agent or host credential is used.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub(crate) fn gate(test: &str) -> Option<(PathBuf, PathBuf)> {
    let required = truthy("AWMAN_TEST_BUILTIN_REQUIRE_HW");
    let blocked = |reason: &str| {
        eprintln!("BLOCKED: {test}: {reason}");
        assert!(!required, "{test}: {reason}");
        None
    };
    if !truthy("AWMAN_TEST_BUILTIN") {
        eprintln!("SKIP: {test}: set AWMAN_TEST_BUILTIN=1");
        assert!(!required, "{test}: hardware gate is required");
        return None;
    }
    let binary = std::env::var_os("AWMAN_TEST_BUILTIN_ARTIFACT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_awman")));
    if !binary.is_file() {
        return blocked("AWMAN_TEST_BUILTIN_ARTIFACT does not name an awman executable");
    }
    if let Err(reason) = crate::gate::hypervisor(&binary) {
        return blocked(&reason);
    }
    let Some(archive) = crate::hardware::fixture_archive() else {
        return blocked("promoted fixture archive is unavailable");
    };
    let info = Command::new(&binary)
        .arg("__awman-builtin-info")
        .output()
        .expect("query the exact awman artifact");
    if !info.status.success() {
        return blocked("the selected awman artifact lacks the builtin provider");
    }
    let provider: serde_json::Value =
        serde_json::from_slice(&info.stdout).expect("builtin info is JSON");
    assert_eq!(provider["host_helpers"], false, "{provider}");
    Some((binary, archive))
}

pub(crate) fn run_bounded(mut command: Command, timeout: Duration) -> Output {
    let start = Instant::now();
    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn actual awman");
    let pid = child.id();
    let waiter = std::thread::spawn(move || child.wait_with_output());
    while !waiter.is_finished() {
        if start.elapsed() > timeout {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("actual awman did not finish within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    waiter.join().unwrap().expect("wait for awman")
}

pub(crate) fn awman(binary: &Path, repo: &Path, home: &Path, bin_dir: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(repo)
        .env_clear()
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
        .env("AWMAN_TEST_ISOLATION", "0")
        .env("AWMAN_CONFIG_HOME", home.join(".awman"));
    command
}

/// `ready` necessarily calls HostAgentPinger. The test substitute accepts
/// only its fixed descriptor argv, greeting allowlist and empty scratch cwd;
/// a user task or repo working directory cannot run through this exception.
pub(crate) fn install_ready_ping_stub(bin_dir: &Path, agent: &str, log: &Path) {
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    let matrix = awman::engine::agent::agent_matrix::matrix_for(agent).unwrap();
    let mut script = format!(
        "#!/bin/sh\nset -eu\ncase \"$PWD\" in */awman-ready-ping-*) ;; *) exit 91;; esac\ntest -z \"$(ls -A .)\"\ntest \"$#\" = {}\n",
        matrix.ping_argv.len()
    );
    for arg in &matrix.ping_argv[1..] {
        script.push_str(&format!("test \"$1\" = {}\nshift\n", quote(arg)));
    }
    let greetings = awman::engine::ready::GREETINGS
        .iter()
        .map(|g| quote(g))
        .collect::<Vec<_>>()
        .join("|");
    script.push_str(&format!("case \"$1\" in {greetings}) ;; *) exit 92;; esac\nprintf 'ping\\n' >> {}\nprintf 'synthetic-ready-ping\\n'\n", quote(&log.to_string_lossy())));
    let path = bin_dir.join(matrix.ping_argv[0]);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn builtin_hw_actual_awman_cli_synthetic_agent_exit_37() {
    const TEST: &str = "builtin_hw_actual_awman_cli_synthetic_agent_exit_37";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("awman-entry.")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let private_bin = temp.path().join("bin");
    let config_dir = home.join(".awman");
    let state = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::create_dir_all(repo.join(".awman")).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&private_bin).unwrap();
    let git = Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .expect("git is a required test tool");
    assert!(git.success());
    std::fs::write(repo.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    std::fs::write(
        repo.join(".awman/Dockerfile.claude"),
        "FROM awman-repo:latest\n",
    )
    .unwrap();
    let agent = temp.path().join("claude");
    std::fs::write(
        &agent,
        b"#!/bin/sh\nIFS= read -r prompt\nprintf 'SYNTHETIC_UID=%s\\n' \"$(id -u)\"\nprintf 'SYNTHETIC_PROMPT=%s\\n' \"$prompt\"\nprintf 'SYNTHETIC_STDERR\\n' >&2\nexit 37\n",
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let overlay = format!("dir({}:/usr/local/bin/claude:ro)", agent.display());
    std::fs::write(
        repo.join(".awman/config.json"),
        serde_json::to_vec(&serde_json::json!({
            "agent": "claude", "auth": "none", "autoAgentAuthAccepted": true,
            "overlays": [overlay]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "runtime": "builtin", "default_agent": "claude",
            "builtin": {
                "stateDir": state.path().join("s"),
                "imageSource": {"type": "archive", "path": archive},
                "network": {"mode": "none"}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    // Any accidental host container-helper use is recorded and fails the test.
    let marker = temp.path().join("helper-called");
    for helper in ["docker", "container", "sbx"] {
        let path = private_bin.join(helper);
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
    let ping_log = temp.path().join("ready-pings");
    install_ready_ping_stub(&private_bin, "claude", &ping_log);
    let mut ready = awman(&binary, &repo, &home, &private_bin);
    ready.args(["ready", "--json"]);
    let ready = run_bounded(ready, Duration::from_secs(180));
    assert!(
        ready.status.success(),
        "actual awman ready failed: {}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let ready_json: serde_json::Value =
        serde_json::from_slice(&ready.stdout).expect("ready --json result");
    assert_eq!(ready_json["ready"], true, "{ready_json}");
    assert_eq!(std::fs::read_to_string(&ping_log).unwrap(), "ping\n");
    assert_eq!(
        ready_json["steps"]["dev_image"]["status"], "ok",
        "{ready_json}"
    );
    assert_eq!(
        ready_json["steps"]["image_source"]["status"], "ok",
        "{ready_json}"
    );
    let mut exec = awman(&binary, &repo, &home, &private_bin);
    exec.args([
        "exec",
        "prompt",
        "--non-interactive",
        "--agent",
        "claude",
        "lifecycle prompt",
    ]);
    let exec = run_bounded(exec, Duration::from_secs(180));
    assert_eq!(
        exec.status.code(),
        Some(37),
        "actual awman exec failed: {}",
        String::from_utf8_lossy(&exec.stderr)
    );
    assert!(
        exec.stdout
            .windows(b"SYNTHETIC_UID=1234".len())
            .any(|w| w == b"SYNTHETIC_UID=1234"),
        "guest identity was not observed: {}",
        String::from_utf8_lossy(&exec.stdout)
    );
    assert!(exec
        .stdout
        .windows(b"SYNTHETIC_PROMPT=lifecycle prompt".len())
        .any(|w| w == b"SYNTHETIC_PROMPT=lifecycle prompt"));
    assert!(exec
        .stderr
        .windows(b"SYNTHETIC_STDERR".len())
        .any(|w| w == b"SYNTHETIC_STDERR"));
    assert!(!marker.exists(), "awman invoked a host container helper");
    assert_eq!(
        std::fs::read_to_string(&ping_log).unwrap(),
        "ping\n",
        "guest task must not invoke the host ping stand-in"
    );
}
