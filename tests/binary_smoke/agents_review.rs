//! Real adapter and binary regressions, using isolated local runtime shims.
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ID: &str = "123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123";

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let f = Self(tempfile::tempdir().unwrap());
        std::fs::create_dir(f.path().join("bin")).unwrap();
        std::fs::create_dir(f.path().join(".awman")).unwrap();
        std::fs::write(
            f.path().join(".awman/config.json"),
            r#"{"runtime":"docker"}"#,
        )
        .unwrap();
        let script = r##"#!/bin/sh
root=$(dirname "$(dirname "$0")")
mode=$(cat "$root/mode" 2>/dev/null)
printf '%s\n' "$*" >> "$root/calls"
case "$1" in
info|system) [ "$mode" != down ] || { echo 'daemon unavailable' >&2; exit 1; } ;;
ps)
  [ "$mode" != partial-list-fail ] || case "$*" in *name=*) echo 'partial query failed' >&2; exit 1;; esac
  [ "$mode" != list-fail ] || { echo 'daemon unavailable' >&2; exit 1; }
  case " $* " in *' --no-trunc '*) ;; *) echo 'full IDs required' >&2; exit 1;; esac
  case "$*" in *status=exited*) file=stopped ;; *) file=running ;; esac
  [ ! -f "$root/$file" ] || cat "$root/$file"
  if [ "$mode" = inspect-vanish ]; then rm -f "$root/running"; fi
  ;;
ls)
  [ "$mode" != slow-sbx ] || { touch "$root/stats-entered"; sleep 30; }
  [ "$mode" != list-fail ] || { echo 'sandbox daemon down' >&2; exit 1; }
  echo '[]' ;;
list) cat "$root/apple.json" ;;
stats)
  [ "$mode" != slow ] || { touch "$root/stats-entered"; sleep 30; }
  [ "$mode" != stats-fail ] || { echo 'stats temporarily unavailable' >&2; exit 1; }
  case "$0" in *container) echo '{"cpuUsageUsec":10,"memoryUsageBytes":1048576}' ;;
    *) echo 'awman-example|1.0%|2MiB / 4GiB' ;; esac
  ;;
stop)
  [ "$mode" != stop-old-missing ] || { echo 'No such container' >&2; exit 1; }
  [ "$mode" != stop-fail ] || { echo 'permission denied' >&2; exit 1; }
  [ "$mode" != stop-gone ] || { rm -f "$root/running"; echo 'No such container' >&2; exit 1; }
  [ ! -f "$root/running" ] || mv "$root/running" "$root/stopped"
  ;;
rm)
  [ "$mode" != remove-fail ] || { echo 'busy' >&2; exit 1; }
  rm -f "$root/stopped"
  ;;
attach)
  if [ "$mode" = attach-hold ]; then touch "$root/attach-entered"; exec cat > "$root/received"; fi
  if [ "$mode" = attach-gone ]; then
    sleep 0.05; rm -f "$root/running"; echo 'Error response from daemon: No such container' >&2; exit 1
  fi
  if [ "$mode" = attach-normal-exit ]; then rm -f "$root/running"; exit 7; fi
  exit 0
  ;;
esac
"##;
        for binary in ["docker", "container", "sbx"] {
            let path = f.path().join("bin").join(binary);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        f.mode("");
        f.row("running", ID, "awman-example");
        f
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn mode(&self, mode: &str) {
        std::fs::write(self.path().join("mode"), mode).unwrap();
    }
    fn row(&self, file: &str, id: &str, name: &str) {
        std::fs::write(self.path().join(file), format!("{id}\t{name}\tregistry.example.invalid/team/a-very-long-image-name:latest\t2026-10-21 10:00:00 +0000 UTC\n")).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(crate::awman_bin());
        self.env(&mut command);
        command
    }
    fn env(&self, command: &mut Command) {
        command
            .current_dir(self.path())
            .env("HOME", self.path())
            .env("AWMAN_CONFIG_HOME", self.path().join(".awman"))
            .env("AWMAN_TEST_ISOLATION", "1")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.path().join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.path().join("calls")).unwrap_or_default()
    }
}

fn exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn docker_adapter_full_ids_json_kill_and_immutable_action_identity() {
    let f = Fixture::new();
    let other = format!("{}different", &ID[..12]);
    f.row("running", ID, "awman-example");
    // Both names share a displayed ID prefix. Exact long references still resolve.
    let mut rows = std::fs::read_to_string(f.path().join("running")).unwrap();
    rows.push_str(&format!(
        "{other}\tawman-other\timage\t2026-10-21 10:00:00 +0000 UTC\n"
    ));
    std::fs::write(f.path().join("running"), rows).unwrap();
    let output = f.run(&["agents", "detail", ID, "--json"]);
    exit(&output, 0);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["id"],
        ID
    );
    exit(&f.run(&["agents", "detail", &ID[..12]]), 2);
    let output = f.run(&["agents", "ls", "--no-trunc"]);
    exit(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains(ID));
    exit(&f.run(&["agents", "kill", ID, "--json"]), 2);
    assert!(!f.calls().lines().any(|l| l.starts_with("stop ")));
    let output = f.run(&["agents", "kill", ID, "--yes", "--json"]);
    exit(&output, 0);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["id"],
        ID
    );
    let calls = f.calls();
    assert!(calls.lines().any(|l| l == format!("stop {ID}")));
    assert!(calls.lines().any(|l| l == format!("rm {ID}")));
    assert!(!calls.contains("stop awman-example"));
    assert!(!calls.contains("rm awman-example"));
}

#[test]
fn docker_adapter_stop_and_listing_failures_keep_their_error_classes() {
    let f = Fixture::new();
    f.mode("stop-fail");
    exit(&f.run(&["agents", "kill", ID, "-y"]), 3);
    assert!(!f.calls().lines().any(|l| l.starts_with("rm ")));
    f.mode("stop-gone");
    let output = f.run(&["agents", "kill", ID, "-y"]);
    exit(&output, 4);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no longer running"));
    for mode in ["down", "list-fail", "partial-list-fail"] {
        f.mode(mode);
        for leaf in ["status", "list", "detail", "kill", "attach"] {
            let args = if matches!(leaf, "status" | "list") {
                vec!["agents", leaf]
            } else {
                vec!["agents", leaf, ID]
            };
            let output = f.run(&args);
            exit(&output, 3);
            assert!(
                output.stdout.is_empty(),
                "failed collection must not print a partial dashboard"
            );
        }
    }
}

#[test]
fn docker_adapter_inspection_checks_lifecycle_independently_from_stats() {
    let f = Fixture::new();
    std::fs::rename(f.path().join("running"), f.path().join("stopped")).unwrap();
    let output = f.run(&["agents", "detail", ID, "--json"]);
    exit(&output, 0);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["running"],
        false
    );
    let output = f.run(&["agents", "detail", ID]);
    exit(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains("stopped"));
    std::fs::rename(f.path().join("stopped"), f.path().join("running")).unwrap();
    f.mode("stats-fail");
    let output = f.run(&["agents", "detail", ID, "--json"]);
    exit(&output, 0);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["running"], true);
    assert!(json["cpu_percent"].is_null());
    f.mode("inspect-vanish");
    exit(&f.run(&["agents", "detail", ID]), 4);
}

#[test]
fn docker_adapter_delayed_attach_failure_differs_from_normal_agent_exit() {
    let f = Fixture::new();
    f.mode("attach-gone");
    let output = f.run(&["agents", "attach", ID]);
    exit(&output, 4);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no longer running"));
    f.row("running", ID, "awman-example");
    f.mode("attach-normal-exit");
    exit(&f.run(&["agents", "attach", ID]), 7);
    assert!(!f
        .calls()
        .lines()
        .any(|l| l.starts_with("stop ") || l.starts_with("rm ")));
}

#[test]
fn watch_sigint_during_pending_stats_reaps_cli_and_exits_130() {
    let f = Fixture::new();
    f.mode("slow");
    let mut child = f
        .command()
        .args(["agents", "status", "--watch"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.path().join("stats-entered").exists() {
        assert!(
            Instant::now() < deadline,
            "watch must enter the stats operation"
        );
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let start = Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            panic!("watch cancellation blocked on stats");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    exit(&output, 130);
    assert!(
        output.stdout.is_empty(),
        "cancelled collection must not render a frame"
    );
}

/// Run the actual Apple adapter in a child with its own PATH and metadata.
#[test]
fn apple_adapter_child() {
    if std::env::var_os("AWMAN_REVIEW_APPLE_CHILD").is_none() {
        return;
    }
    use awman::engine::agent_runtime::AgentRuntimeEngine;
    let runtime = awman::engine::container::ContainerRuntime::apple();
    let running = runtime.list_running_all().unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].name, "awman-example");
    let detail = AgentRuntimeEngine::inspect(&runtime, &running[0]).unwrap();
    assert!(!detail.owner.unwrap().alive);
    // Dead owner is rejected by the real command before a socket connect.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bus = awman::command::commands::api_server::event_bus::EventBus::new(16);
    let frontend = awman::frontend::api::command_frontend::ApiDispatchFrontend::new(
        "agents attach",
        &[],
        bus.sender(),
    );
    use awman::command::commands::{agents::AgentsAttachCommand, Command as _};
    let err = rt
        .block_on(
            AgentsAttachCommand::new("awman-example".into(), std::sync::Arc::new(runtime))
                .run_with_frontend(Box::new(frontend)),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("launching awman process has exited"),
        "{err}"
    );
    let attach_dir = std::env::var_os("AWMAN_ATTACH_DIR").unwrap();
    for entry in std::fs::read_dir(attach_dir).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let runtime = awman::engine::container::ContainerRuntime::apple();
    let error =
        match awman::engine::agent_runtime::AgentRuntimeEngine::attach(&runtime, &running[0]) {
            Ok(_) => panic!("legacy metadata must not bypass the Apple attach guard"),
            Err(error) => error,
        };
    assert!(error
        .to_string()
        .contains("launcher identity is unavailable"));
}

#[test]
fn apple_adapter_rejects_unowned_nanoclaw_names_and_reads_dead_launcher_identity() {
    let f = Fixture::new();
    std::fs::write(f.path().join("apple.json"), r#"[{"configuration":{"id":"my-nanoclaw-production"},"status":"running"},{"configuration":{"id":"awman-example"},"status":"running"}]"#).unwrap();
    let attach_dir = f.path().join("attach");
    std::fs::create_dir(&attach_dir).unwrap();
    let hash = awman::data::fs::hash::sha256_hex("awman-example");
    std::fs::write(
        attach_dir.join(format!("{}.owner", &hash[..16])),
        "2147483647",
    )
    .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    f.env(&mut command);
    let output = command
        .env("AWMAN_REVIEW_APPLE_CHILD", "1")
        .env("AWMAN_ATTACH_DIR", attach_dir)
        .args([
            "--exact",
            "agents_review::apple_adapter_child",
            "--nocapture",
        ])
        .output()
        .unwrap();
    exit(&output, 0);
}

struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn std::io::Write + Send>,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
    seen: Vec<u8>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl PtySession {
    fn new(fixture: &Fixture, args: &[&str]) -> Self {
        use std::io::Read;
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(crate::awman_bin());
        command.args(args);
        command.cwd(fixture.path());
        command.env("HOME", fixture.path());
        command.env("AWMAN_CONFIG_HOME", fixture.path().join(".awman"));
        command.env("AWMAN_TEST_ISOLATION", "1");
        command.env("TERM", "xterm");
        command.env(
            "PATH",
            format!(
                "{}:{}",
                fixture.path().join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (tx, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            writer,
            output,
            seen: Vec::new(),
            _master: pair.master,
        }
    }
    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = String::new();
        while !output.contains(needle) {
            let chunk = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("PTY did not show {needle:?}: {e}; output={output}"));
            output.push_str(&String::from_utf8_lossy(&chunk));
            self.seen.extend_from_slice(&chunk);
        }
    }
    fn finish(&mut self) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("PTY child did not exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn docker_replacement_during_kill_confirmation_is_never_stopped_by_name() {
    use std::io::Write;
    let f = Fixture::new();
    let mut session = PtySession::new(&f, &["agents", "kill", "awman-example"]);
    session.wait_for("choice [");
    let replacement = "fedcba9876543210";
    f.row("running", replacement, "awman-example");
    f.mode("stop-old-missing");
    session.writer.write_all(b"y\n").unwrap();
    session.writer.flush().unwrap();
    assert_eq!(session.finish(), 4);
    assert!(f.calls().lines().any(|call| call == format!("stop {ID}")));
    assert!(!f
        .calls()
        .lines()
        .any(|call| call == "stop awman-example" || call.starts_with("rm ")));
    assert!(std::fs::read_to_string(f.path().join("running"))
        .unwrap()
        .contains(replacement));
}

#[test]
fn cli_attach_ctrl_c_detaches_live_pty_without_sending_interrupt() {
    use std::io::Write;
    let f = Fixture::new();
    f.mode("attach-hold");
    let mut session = PtySession::new(&f, &["agents", "attach", ID]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.path().join("received").exists() {
        assert!(Instant::now() < deadline, "attach stub must start");
        std::thread::sleep(Duration::from_millis(10));
    }
    session.writer.write_all(b"hello\n").unwrap();
    session.writer.flush().unwrap();
    while std::fs::read(f.path().join("received")).unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "ordinary input must reach the live stub"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    session.writer.write_all(&[3]).unwrap();
    session.writer.flush().unwrap();
    assert_eq!(session.finish(), 0);
    assert_eq!(
        std::fs::read(f.path().join("received")).unwrap(),
        b"hello\n"
    );
    assert!(f.path().join("running").exists());
    assert!(!f
        .calls()
        .lines()
        .any(|call| call.starts_with("stop ") || call.starts_with("rm ")));
}

#[test]
fn watch_termination_signals_restore_the_tty_and_exit_130() {
    use nix::sys::signal::{kill, Signal};
    for signal in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        let f = Fixture::new();
        let mut session = PtySession::new(&f, &["agents", "status", "--watch"]);
        session.wait_for("Tip:");
        kill(
            nix::unistd::Pid::from_raw(session.child.process_id().unwrap() as i32),
            signal,
        )
        .unwrap();
        assert_eq!(session.finish(), 130);
        while let Ok(bytes) = session.output.recv_timeout(Duration::from_millis(100)) {
            session.seen.extend_from_slice(&bytes);
        }
        let output = String::from_utf8_lossy(&session.seen);
        assert_eq!(output.matches("\x1b[?1049h").count(), 1);
        assert_eq!(output.matches("\x1b[?1049l").count(), 1);
        assert_eq!(output.matches("\x1b[?25l").count(), 1);
        assert_eq!(output.matches("\x1b[?25h").count(), 1);
    }
}
