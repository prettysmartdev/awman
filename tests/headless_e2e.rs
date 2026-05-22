/// End-to-end tests for `amux headless start` (work item 0057).
///
/// These tests spawn the compiled `amux` binary as a subprocess, make real
/// HTTP requests via `reqwest`, and verify both the HTTP response shape and
/// on-disk artifacts (DB file, PID file).
///
/// The `--background` tests are marked `#[ignore]` because they require a
/// functional OS process manager (systemd on Linux, launchd on macOS) or
/// the double-fork daemonisation path, which may not be available in all CI
/// environments.
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find a free TCP port by binding to port 0 and immediately releasing it.
///
/// There is a small TOCTOU window between releasing the socket and the
/// subprocess claiming the port, but in practice this is negligible in tests.
fn find_free_port() -> u16 {
    let socket = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("failed to bind to ephemeral port");
    socket.local_addr().unwrap().port()
}

/// Return the path to the compiled `amux` binary.
fn amux_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_awman"))
}

/// Poll `GET {base}/v1/status` until the server responds with 200 or the
/// deadline is exceeded.  Returns `true` if the server came up in time.
async fn wait_for_server(client: &reqwest::Client, base: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.get(format!("{base}/v1/status")).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// End-to-end: foreground server via subprocess
// ---------------------------------------------------------------------------

/// Spawns `amux headless start` in a subprocess, exercises the HTTP API, and
/// verifies that on-disk artifacts are created.
#[tokio::test]
async fn e2e_headless_start_subprocess_responds_to_http() {
    let root_dir = tempfile::TempDir::new().unwrap();
    let workdir = tempfile::TempDir::new().unwrap();

    let port = find_free_port();
    let base = format!("http://127.0.0.1:{port}");

    // Spawn the server process, redirecting its output to /dev/null to keep
    // test output clean.  AWMAN_API_ROOT forces it to use our temp dir.
    let mut child = std::process::Command::new(amux_bin())
        .args([
            "api",
            "start",
            "--port",
            &port.to_string(),
            "--workdirs",
            workdir.path().to_str().unwrap(),
            "--dangerously-skip-auth",
        ])
        .env("AWMAN_API_ROOT", root_dir.path())
        // Suppress tracing output so it doesn't pollute the test runner.
        .env("RUST_LOG", "off")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn amux headless start");

    let client = reqwest::Client::new();

    // Wait for the server to accept connections (up to 5 seconds).
    let started = wait_for_server(&client, &base, Duration::from_secs(5)).await;

    if !started {
        let _ = child.kill();
        let _ = child.wait();
        panic!("amux headless start did not respond within 5 seconds");
    }

    // ── PID file must exist while the server is running ───────────────────
    let pid_file = root_dir.path().join("amux.pid");
    assert!(
        pid_file.exists(),
        "amux.pid must be written by the foreground server"
    );

    // ── DB file must exist ────────────────────────────────────────────────
    let db_file = root_dir.path().join("amux.db");
    assert!(db_file.exists(), "amux.db must be created at startup");

    // ── /v1/status shape ─────────────────────────────────────────────────
    let status: serde_json::Value = client
        .get(format!("{base}/v1/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"], "ok", "status field must be 'ok'");
    assert!(status["pid"].is_number(), "pid must be a number");
    assert!(
        status["uptime_seconds"].is_number(),
        "uptime_seconds must be a number"
    );

    // ── /v1/workdirs shape ────────────────────────────────────────────────
    let workdirs_resp: serde_json::Value = client
        .get(format!("{base}/v1/workdirs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dirs = workdirs_resp["workdirs"]
        .as_array()
        .expect("workdirs must be an array");
    assert_eq!(dirs.len(), 1, "exactly one workdir was configured");

    // ── Create a session and verify it appears in the DB ──────────────────
    let canonical_workdir = std::fs::canonicalize(workdir.path()).unwrap();
    let create_resp: serde_json::Value = client
        .post(format!("{base}/v1/sessions"))
        .json(&serde_json::json!({
            "workdir": canonical_workdir.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = create_resp["session_id"]
        .as_str()
        .expect("session_id must be a string");
    assert!(!session_id.is_empty(), "session_id must not be empty");

    // ── Gracefully shut down the server ──────────────────────────────────
    // Send SIGTERM to trigger the graceful shutdown handler.
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let pid = child.id() as i32;
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    // Wait for the process to exit (give it up to 5 seconds for graceful shutdown).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // ── DB must persist after server exits ───────────────────────────────
    assert!(
        db_file.exists(),
        "amux.db must still exist after the server exits"
    );

    // ── Verify the session is in the DB ──────────────────────────────────
    let conn = awman::commands::headless::db::open_db(root_dir.path()).unwrap();
    let row = awman::commands::headless::db::get_session(&conn, session_id)
        .unwrap()
        .expect("session created via HTTP must be present in the DB");
    assert_eq!(row.workdir, canonical_workdir.to_str().unwrap());
}

// ---------------------------------------------------------------------------
// --background flag  (requires OS process manager; skipped in CI by default)
// ---------------------------------------------------------------------------

/// Verifies that `--background` daemonises the server, writes a PID file,
/// and that `amux headless kill` terminates the process and removes the file.
///
/// Marked `#[ignore]` because it requires either systemd-run (Linux) or
/// launchctl (macOS) to be available, or relies on the double-fork fallback
/// which may not write the PID file synchronously in all environments.
/// Run manually with: `cargo test background -- --ignored`
#[tokio::test]
#[ignore]
async fn background_flag_daemonises_server_and_kill_terminates_it() {
    let root_dir = tempfile::TempDir::new().unwrap();
    let workdir = tempfile::TempDir::new().unwrap();
    let port = find_free_port();
    let base = format!("http://127.0.0.1:{port}");

    // Start in background mode.
    let status = std::process::Command::new(amux_bin())
        .args([
            "api",
            "start",
            "--port",
            &port.to_string(),
            "--workdirs",
            workdir.path().to_str().unwrap(),
            "--background",
        ])
        .env("AWMAN_API_ROOT", root_dir.path())
        .env("RUST_LOG", "off")
        .status()
        .expect("failed to run amux headless start --background");

    assert!(
        status.success(),
        "amux headless start --background must exit 0"
    );

    // The background process writes its PID file asynchronously; wait for it.
    let pid_file = root_dir.path().join("amux.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if pid_file.exists() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("amux.pid was not written within 5 seconds of --background start");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Wait for the HTTP server to accept connections.
    let client = reqwest::Client::new();
    let started = wait_for_server(&client, &base, Duration::from_secs(5)).await;
    assert!(started, "background server did not respond within 5 seconds");

    // Read the PID and verify the process is alive.
    let pid_str = std::fs::read_to_string(&pid_file).unwrap();
    let pid: u32 = pid_str.trim().parse().expect("amux.pid must contain a valid PID");
    assert!(
        awman::commands::headless::process::is_process_alive(pid),
        "background server process (PID {pid}) must be alive"
    );

    // Kill the server via `amux headless kill`.
    let kill_status = std::process::Command::new(amux_bin())
        .args(["api", "kill"])
        .env("AWMAN_API_ROOT", root_dir.path())
        .status()
        .expect("failed to run amux headless kill");
    assert!(kill_status.success(), "amux headless kill must exit 0");

    // Give the process a moment to exit and for the PID file to be removed.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if !pid_file.exists() {
            break;
        }
        if Instant::now() >= deadline {
            break; // assertion below will fail and report the problem
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        !pid_file.exists(),
        "amux.pid must be removed after headless kill"
    );
    assert!(
        !awman::commands::headless::process::is_process_alive(pid),
        "process (PID {pid}) must be dead after headless kill"
    );
}
