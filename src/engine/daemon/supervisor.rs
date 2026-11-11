//! `DaemonSupervisor` — the process half of a long-lived awman daemon.
//!
//! Layer 1 (WI 0114 F-29, decision Q1): spawning `systemd-run`, writing a
//! launchd plist, probing `/proc` or `ps` or `tasklist` for liveness, and
//! sending SIGTERM or `taskkill` are process operations, not filesystem
//! ones. Layer 0 keeps what is genuinely on-disk state — the PID file, the
//! `ServerMeta` sidecar, the daemon's paths — as [`DaemonProcess`], which a
//! supervisor owns and reads through.
//!
//! The per-OS blocks are unchanged from `data::fs::daemon_process`: Linux
//! prefers `systemd-run --user`, macOS prefers a launchd agent, and every
//! platform (Windows included) falls back to a direct spawn with the child's
//! output redirected into the daemon log.

use std::path::Path;

// Only the systemd (Linux) and launchd (macOS) paths hand a job its
// environment explicitly; the direct-spawn fallback every other platform
// uses inherits the process environment wholesale. Same idiom as the
// `allow(dead_code)` on the two argv builders below.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(unused_imports)
)]
use crate::data::config::env::ForwardedEnv;
use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::data::fs::daemon_process::{ensure_private_log, read_pid, DaemonProcess};
use crate::engine::error::EngineError;

/// Supervises one daemon: liveness, background spawn, termination.
///
/// Owns the Layer 0 [`DaemonProcess`] so the PID file a supervisor kills by
/// is the same one the rest of the command reads.
pub struct DaemonSupervisor {
    process: DaemonProcess,
    /// Whether a background spawn may go through launchd / `systemd --user`.
    /// Off under test isolation: the label and unit are fixed per daemon
    /// kind, not per storage root, so a test daemon started through either
    /// would boot out or replace the developer's real one.
    service_manager: bool,
}

impl DaemonSupervisor {
    /// Supervise an already-built [`DaemonProcess`].
    pub fn new(process: DaemonProcess) -> Self {
        Self {
            process,
            service_manager: !crate::data::config::env::test_isolation_active(),
        }
    }

    /// Let background spawns use launchd / `systemd --user` even under test
    /// isolation. Only for a test that has put stub `launchctl` /
    /// `systemd-run` executables first on `PATH` and pointed `HOME` at a
    /// temporary directory, so the routing it exercises reaches nothing real.
    pub fn with_stubbed_service_manager(mut self) -> Self {
        self.service_manager = true;
        self
    }

    /// Supervise the daemon at `paths` under the given systemd unit name and
    /// launchd plist label.
    pub fn for_paths(paths: DaemonPaths) -> Self {
        Self::new(DaemonProcess::unified(paths))
    }

    /// The Layer 0 PID / meta / paths object this supervisor drives.
    pub fn process(&self) -> &DaemonProcess {
        &self.process
    }

    /// Whether the OS reports the process is alive.
    pub fn is_process_alive(pid: u32) -> bool {
        is_process_alive(pid)
    }

    /// Whether the process at `pid` looks like an awman daemon.
    pub fn pid_is_awman(pid: u32) -> bool {
        pid_is_awman(pid)
    }

    /// Return the running daemon PID only when the process is alive AND looks
    /// like an awman server. Stale or wrong-process PIDs are cleaned up.
    pub fn running_pid(&self) -> Result<Option<u32>, EngineError> {
        Ok(check_already_running(&self.process.paths().pid_file())?)
    }

    /// Spawn the daemon binary in the background, returning the child PID.
    /// Threads this daemon's unit name / plist label / log path through, so
    /// two daemons never collide on the systemd unit or the launchd plist.
    pub fn spawn_detached(&self, binary: &Path, args: &[String]) -> Result<u32, EngineError> {
        let paths = self.process.paths();
        spawn_background(
            binary,
            args,
            &paths.log_file(),
            self.process.unit_name(),
            self.process.plist_label(),
            self.service_manager,
        )
        .map_err(|error| EngineError::Other(format!("failed to start the awman daemon: {error}")))
    }

    /// Terminate the running daemon: read the PID, and if it is a live awman
    /// process, send SIGTERM, then release the PID file. A stale/absent PID is
    /// a no-op that still clears the file.
    pub fn terminate(&self) -> Result<(), EngineError> {
        self.terminate_running()?;
        Ok(())
    }

    /// Terminate this daemon and report what was actually found, so callers can
    /// render their own "stopped" / "stale pidfile" messages without reaching
    /// for a free OS function. The pidfile is released on every outcome.
    pub fn terminate_running(&self) -> Result<Termination, EngineError> {
        let outcome = match self.process.read_pid()? {
            None => Termination::NotRunning,
            Some(pid) if !is_process_alive(pid) => Termination::StalePidFile { pid },
            Some(pid) if !pid_is_awman(pid) => Termination::NotAwman { pid },
            Some(pid) => {
                kill_process(pid)?;
                Termination::Terminated { pid }
            }
        };
        if let Termination::StalePidFile { pid } | Termination::NotAwman { pid } = outcome {
            self.process.release_pidfile_owned_by(pid)?;
        }
        Ok(outcome)
    }
}

/// Whether the OS reports the process is alive.
///
/// Private: the way to ask is [`DaemonSupervisor::is_process_alive`], which
/// wraps this and is what every caller outside this module already uses.
/// Kept as a free fn because the per-OS `cfg` arms have no state to hang on
/// a type (Tenet 3's stateless-helper exception).
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!(",\"{}\",", pid)))
        .unwrap_or(false)
}

/// Whether the OS reports the process command name contains "awman". Used to
/// disambiguate stale PID files from reused PIDs. Private for the same reason
/// as [`is_process_alive`] above; ask through
/// [`DaemonSupervisor::pid_is_awman`]. On platforms where the command name is unreadable, returns `true`
/// (trust the PID file), matching old-awman.
#[cfg(target_os = "linux")]
fn pid_is_awman(pid: u32) -> bool {
    let path = format!("/proc/{pid}/comm");
    std::fs::read_to_string(&path)
        .map(|s| s.trim().contains("awman"))
        // `check_already_running` has already established that this PID is
        // alive.  A transient procfs read failure must therefore not turn a
        // live daemon into a "stale" pidfile and delete its claim: doing so
        // lets a competing daemon start against the same shared database.
        // When identity cannot be inspected, conservatively retain the
        // pidfile and refuse the competing start. This matches the fallback
        // policy on platforms without a readable process command name.
        .unwrap_or(true)
}

#[cfg(target_os = "macos")]
fn pid_is_awman(pid: u32) -> bool {
    // `-ww`: BSD `ps` truncates its output to 79 columns when none of stdin,
    // stdout or stderr is a terminal — always the case here, since stdout is
    // captured — and `comm` is the executable's full path, with the "awman"
    // at its end. Any install path past that width would make a live daemon
    // look stale, and a second one would be started beside it.
    std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| {
            let command = String::from_utf8_lossy(&o.stdout);
            Path::new(command.trim())
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("awman"))
        })
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn pid_is_awman(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("awman")
        })
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn pid_is_awman(_pid: u32) -> bool {
    true
}

/// Check the PID file. Returns `Some(pid)` only when the process is alive AND
/// looks like an awman server. Stale or wrong-process PIDs are cleaned up.
pub(crate) fn check_already_running(pid_path: &Path) -> Result<Option<u32>, DataError> {
    match read_pid(pid_path)? {
        Some(pid) if is_process_alive(pid) && pid_is_awman(pid) => Ok(Some(pid)),
        Some(pid) => {
            crate::data::fs::daemon_process::clear_pid_owned_by(pid_path, pid)?;
            Ok(None)
        }
        None => Ok(None),
    }
}

/// What [`DaemonProcess::terminate_running`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// No pidfile at all.
    NotRunning,
    /// The pidfile named a process that is no longer alive.
    StalePidFile { pid: u32 },
    /// The pidfile named a live process that is not an awman daemon.
    NotAwman { pid: u32 },
    /// A live awman daemon was signalled.
    Terminated { pid: u32 },
}

#[cfg(unix)]
fn kill_process(pid: u32) -> Result<(), DataError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| DataError::Other(format!("failed to send SIGTERM to PID {pid}: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn kill_process(pid: u32) -> Result<(), DataError> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .map_err(|e| DataError::Other(format!("failed to terminate PID {pid}: {e}")))?;
    if !status.success() {
        return Err(DataError::Other(format!("taskkill /PID {pid} /F failed")));
    }
    Ok(())
}

/// Spawn the daemon in the background. Returns the child PID. `unit_name` /
/// `plist_label` are threaded into the systemd / launchd happy paths so
/// distinct daemons never share a unit or plist.
pub(crate) fn spawn_background(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
    unit_name: &str,
    plist_label: &str,
    service_manager: bool,
) -> Result<u32, DataError> {
    if let Some(parent) = log_path.parent() {
        crate::data::fs::private_files::create_private_dir(parent)?;
    }
    // Create the log ourselves so it exists with owner-only permissions before
    // launchd/systemd starts appending to it at the process umask. A daemon log
    // can capture startup diagnostics that should not be world-readable.
    ensure_private_log(log_path)?;

    // See `DaemonSupervisor::service_manager`: under test isolation the daemon
    // is only ever a plain child process.
    if !service_manager {
        let _ = (unit_name, plist_label);
        return double_fork_spawn(binary_path, args, log_path);
    }

    // Each happy path consumes only its own identity; silence the other on
    // platforms that don't use it.
    #[cfg(target_os = "linux")]
    {
        let _ = plist_label;
        if let Some(pid) = try_systemd_run(binary_path, args, unit_name, log_path)? {
            return Ok(pid);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = unit_name;
        if let Some(pid) = try_launchd(binary_path, args, log_path, plist_label)? {
            return Ok(pid);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (unit_name, plist_label);
    }

    double_fork_spawn(binary_path, args, log_path)
}

/// Append one diagnostic line to the daemon log, best-effort.
///
/// A failed start tells the user to check this file, so the reason the OS
/// process manager was skipped — or the stderr it printed before refusing —
/// has to land *in* it. Failing to write a diagnostic must never fail the
/// spawn, hence the discarded results.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn note_in_log(log_path: &Path, message: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(log_path) {
        let _ = writeln!(f, "awman: {}", message.trim_end());
    }
}

/// Pure argv builder for `systemd-run` (the program name itself is not
/// included; the caller supplies it to `Command::new`).
///
/// Layout: `--user`, `--unit=<unit_name>`, the two `StandardOutput`/
/// `StandardError` append-to-log properties, one `--setenv=NAME=VALUE` per
/// entry of `env` in order, `--`, the binary path, then `args`.
///
/// Each `--setenv=NAME=VALUE` is built as a single `String` and later handed
/// to `Command::args` as one argv element — never assembled into or run
/// through a shell — so a `VALUE` containing `=`, spaces, or other
/// shell-significant characters reaches systemd-run intact with no escaping
/// needed.
///
/// Only `try_systemd_run` (Linux-only) calls this outside of tests; it stays
/// compiled on every platform so the unit tests exercise it everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn systemd_run_argv(
    binary_path: &Path,
    args: &[String],
    unit_name: &str,
    log_path: &Path,
    env: &[(String, String)],
) -> Vec<String> {
    let log = log_path.to_string_lossy();
    let mut argv = vec![
        "--user".to_string(),
        format!("--unit={unit_name}"),
        // Without these the unit's output goes to the journal, and the log
        // file the startup-failure message points the user at stays empty
        // forever. `append:` needs systemd 240+; on anything older
        // systemd-run refuses the property and the caller falls through to
        // the plain spawn, which logs to the same file itself.
        format!("--property=StandardOutput=append:{log}"),
        format!("--property=StandardError=append:{log}"),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        argv.push(format!("--working-directory={}", cwd.display()));
    }
    argv.extend(
        env.iter()
            .map(|(name, value)| format!("--setenv={name}={value}")),
    );
    argv.push("--".to_string());
    argv.push(binary_path.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    argv
}

#[cfg(target_os = "linux")]
fn try_systemd_run(
    binary_path: &Path,
    args: &[String],
    unit_name: &str,
    log_path: &Path,
) -> Result<Option<u32>, DataError> {
    let check = std::process::Command::new("systemd-run")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match check {
        Ok(s) if s.success() => {}
        _ => return Ok(None),
    }

    let argv = systemd_run_argv(
        binary_path,
        args,
        unit_name,
        log_path,
        &daemon_unit_env(ForwardedEnv::from_process()),
    );
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(&argv);

    let output = cmd
        .output()
        .map_err(|e| DataError::Other(format!("systemd-run failed: {e}")))?;
    if !output.status.success() {
        note_in_log(
            log_path,
            &format!(
                "systemd-run declined to start {unit_name}, falling back to a direct spawn: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return Ok(None);
    }
    // systemd-run returns immediately; the actual PID is tracked by the unit.
    // Return 0 as a sentinel — the PID file will be written by the child.
    Ok(Some(0))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the launchd job definition for one daemon.
///
/// Kept free of `#[cfg]` so it stays compiled and unit-tested on every
/// platform: the plist's contents are what decide whether the daemon can find
/// `docker`, resolve the same storage root as the process that started it, and
/// write anywhere the user can read.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn render_launchd_plist(
    plist_label: &str,
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
    env: &[(String, String)],
    working_dir: Option<&Path>,
) -> String {
    let mut program_args = format!(
        "        <string>{}</string>\n",
        xml_escape(&binary_path.to_string_lossy())
    );
    for arg in args {
        program_args.push_str(&format!("        <string>{}</string>\n", xml_escape(arg)));
    }

    let mut environment = String::new();
    if !env.is_empty() {
        environment.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (name, value) in env {
            environment.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(name),
                xml_escape(value)
            ));
        }
        environment.push_str("    </dict>\n");
    }

    // launchd starts a job in `/` unless told otherwise, and the daemon opens
    // its session from the working directory.
    let working_directory = working_dir
        .map(|dir| {
            format!(
                "    <key>WorkingDirectory</key>\n    <string>{}</string>\n",
                xml_escape(&dir.to_string_lossy())
            )
        })
        .unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
{environment}{working_directory}    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(plist_label),
        log = xml_escape(&log_path.to_string_lossy())
    )
}

/// This process's real user id, for addressing the `gui/<uid>` launchd domain.
///
/// Read via `id -u` rather than `getuid(2)` because the crate is
/// `#![forbid(unsafe_code)]` and `nix`'s user feature is not enabled.
#[cfg(target_os = "macos")]
fn current_uid() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit())).then_some(uid)
}

#[cfg(target_os = "macos")]
fn try_launchd(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
    plist_label: &str,
) -> Result<Option<u32>, DataError> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let plist_path = home.join(format!("Library/LaunchAgents/{plist_label}.plist"));
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }

    let plist = render_launchd_plist(
        plist_label,
        binary_path,
        args,
        log_path,
        &daemon_unit_env(ForwardedEnv::from_process()),
        std::env::current_dir().ok().as_deref(),
    );
    std::fs::write(&plist_path, plist).map_err(|e| DataError::io(&plist_path, e))?;

    // Without a uid there is no `gui/<uid>` domain to address, and the only
    // alternative is the legacy `load` whose exit status cannot be trusted.
    // A plain spawn that logs beats a launchd start that silently does nothing.
    let Some(uid) = current_uid() else {
        note_in_log(
            log_path,
            "could not determine the current uid; starting the daemon directly instead of via launchd",
        );
        let _ = std::fs::remove_file(&plist_path);
        return Ok(None);
    };
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{plist_label}");

    // `RunAtLoad` fires when the job is *bootstrapped*, and a job stays
    // bootstrapped for the whole login session (nothing here unloads it, and
    // macOS re-loads `~/Library/LaunchAgents` at every login). Bootstrapping an
    // already-loaded label is a no-op that starts nothing — which is exactly
    // how a start could report success and yet spawn no daemon. Boot it out
    // first so the bootstrap below always launches a fresh process. Failure is
    // expected and ignored: usually the job simply was not loaded.
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &service])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // A label the user has switched off in System Settings › Login Items stays
    // disabled across bootstraps; this is the modern equivalent of `load -w`.
    let _ = std::process::Command::new("launchctl")
        .args(["enable", &service])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // `bootstrap` reports a real exit status, unlike the legacy `load`, which
    // exits 0 even for "service already loaded" and "Load failed".
    let output = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_path.to_string_lossy()])
        .output()
        .map_err(|e| DataError::Other(format!("launchctl bootstrap failed: {e}")))?;

    if !output.status.success() {
        // launchctl's diagnostics are a platform-level implementation detail
        // and must never print into the invoking TUI/CLI — but they are the
        // whole explanation, so they go in the log the failure points at.
        note_in_log(
            log_path,
            &format!(
                "launchctl bootstrap {service} failed ({}), falling back to a direct spawn: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ),
        );
        let _ = std::fs::remove_file(&plist_path);
        return Ok(None);
    }
    Ok(Some(0))
}

/// Spawn the daemon directly, with its output redirected into `log_path`.
///
/// The redirect is the point: this is the path taken whenever the OS process
/// manager is absent or declines, and with `Stdio::null()` here the daemon's
/// every startup diagnostic was discarded — leaving "check <log>" pointing at
/// a file that `ensure_private_log` had just created empty and nothing would
/// ever write to. The daemon logs to stderr (see `init_tracing`), so wiring
/// both streams to the log file is what makes a failed start explain itself.
///
/// No `FORWARDED_ENV` handling needed here: full process-environment inheritance already covers that bootstrap-class allowlist.
fn double_fork_spawn(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
) -> Result<u32, DataError> {
    // Two handles: `Stdio` consumes the file it is built from, and stdout and
    // stderr each need their own. A log that cannot be opened is not worth
    // failing a start over — fall back to discarding output, as before.
    let open_log = || {
        std::fs::OpenOptions::new()
            .append(true)
            .open(log_path)
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null())
    };

    let mut cmd = std::process::Command::new(binary_path);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(open_log())
        .stderr(open_log());

    // On Unix this is a single Command::spawn. True
    // setsid daemonization would require `pre_exec`, which is unsafe — and this crate's
    // `#![forbid(unsafe_code)]` rule disallows. The systemd-run / launchd happy paths
    // above handle real detachment when the OS supports it.

    // On Windows, ensure the child gets its own process group so a Ctrl-C
    // delivered to the parent console does not also kill the daemon.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NEW_PROCESS_GROUP = 0x00000200
        cmd.creation_flags(0x00000200);
    }

    let child = cmd
        .spawn()
        .map_err(|e| DataError::Other(format!("failed to spawn background server: {e}")))?;
    Ok(child.id())
}

/// The environment the `awman-daemon` unit / `io.awman.daemon` agent is started
/// with: the process basics (`PATH`, `HOME`, `RUST_LOG`, XDG dirs) and
/// `AWMAN_HOME`, so the daemon resolves the same home as the process that
/// started it. No other `AWMAN_*` variable rides along: the daemon reads its
/// configuration from that home, and `--set` overrides travel as argv.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) fn daemon_unit_env(forwarded: Vec<(String, String)>) -> Vec<(String, String)> {
    forwarded
        .into_iter()
        .filter(|(name, _)| !name.starts_with("AWMAN_") || name == "AWMAN_HOME")
        .collect()
}
