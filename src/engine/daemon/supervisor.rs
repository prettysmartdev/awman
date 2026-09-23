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
use crate::data::fs::daemon_process::{
    clear_pid, ensure_private_log, read_pid, DaemonProcess, API_UNIT_NAME, SQUAD_UNIT_NAME,
};
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
    pub fn for_paths(
        paths: DaemonPaths,
        unit_name: &'static str,
        plist_label: &'static str,
    ) -> Self {
        Self::new(DaemonProcess::new(paths, unit_name, plist_label))
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
        .map_err(|error| {
            let daemon = match self.process.unit_name() {
                SQUAD_UNIT_NAME => "squad",
                API_UNIT_NAME => "API",
                _ => "background",
            };
            EngineError::Other(format!("failed to start the {daemon} daemon: {error}"))
        })
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
        self.process.release_pidfile()?;
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
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().contains("awman"))
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
        Some(_) => {
            clear_pid(pid_path)?;
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
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
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
        &ForwardedEnv::from_process(),
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
        &ForwardedEnv::from_process(),
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

    // On Unix this matches old-amux exactly: a single Command::spawn. True
    // setsid daemonization would require `pre_exec`, which is unsafe — and this
    // crate is `#![forbid(unsafe_code)]`. The systemd-run / launchd happy paths
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::daemon_process::write_pid;

    #[test]
    #[cfg(unix)]
    fn a_directly_spawned_daemon_writes_its_output_to_the_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("awman.log");
        ensure_private_log(&log).unwrap();

        // `sh -c` stands in for the daemon: one line on stdout, one on stderr.
        double_fork_spawn(
            Path::new("/bin/sh"),
            &[
                "-c".to_string(),
                "echo to-stdout; echo to-stderr >&2".to_string(),
            ],
            &log,
        )
        .unwrap();

        // The child is detached, so poll rather than assuming it has run.
        let contents = (0..100)
            .find_map(|_| {
                let body = std::fs::read_to_string(&log).unwrap_or_default();
                if body.contains("to-stdout") && body.contains("to-stderr") {
                    return Some(body);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            })
            .unwrap_or_else(|| {
                panic!(
                    "daemon output never reached the log: {:?}",
                    std::fs::read_to_string(&log)
                )
            });
        assert!(contents.contains("to-stdout"), "{contents:?}");
        assert!(contents.contains("to-stderr"), "{contents:?}");
    }

    /// An unopenable log must degrade to discarded output, never to a failed
    /// start: the daemon matters more than its diagnostics.
    #[test]
    fn a_daemon_still_spawns_when_its_log_cannot_be_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let unopenable = tmp.path().join("no-such-dir").join("awman.log");
        assert!(double_fork_spawn(
            Path::new(if cfg!(windows) { "cmd" } else { "/bin/sh" }),
            &[
                if cfg!(windows) { "/c" } else { "-c" }.to_string(),
                "exit 0".to_string()
            ],
            &unopenable,
        )
        .is_ok());
    }

    /// The launchd job inherits nothing from the shell that started awman, so
    /// everything it needs has to be written into the plist: the environment
    /// (or it resolves a different storage root, and publishes its endpoint
    /// where nobody is looking) and a working directory (or it starts in `/`).
    #[test]
    fn the_launchd_plist_carries_the_environment_and_working_directory() {
        let plist = render_launchd_plist(
            "io.awman.squad",
            Path::new("/usr/local/bin/awman"),
            &["squad".to_string(), "start".to_string()],
            Path::new("/home/u/.awman/squad/awman.log"),
            &[
                ("PATH".to_string(), "/usr/local/bin:/usr/bin".to_string()),
                ("AWMAN_SQUAD_ROOT".to_string(), "/custom/squad".to_string()),
            ],
            Some(Path::new("/home/u/project")),
        );

        assert!(plist.contains("<key>EnvironmentVariables</key>"), "{plist}");
        assert!(plist.contains("<key>PATH</key>"), "{plist}");
        assert!(
            plist.contains("<string>/usr/local/bin:/usr/bin</string>"),
            "{plist}"
        );
        assert!(plist.contains("<key>AWMAN_SQUAD_ROOT</key>"), "{plist}");
        assert!(
            plist.contains("<key>WorkingDirectory</key>\n    <string>/home/u/project</string>"),
            "{plist}"
        );
        // The pieces that were already load-bearing must survive the rewrite.
        assert!(plist.contains("<string>io.awman.squad</string>"), "{plist}");
        assert!(
            plist.contains("<string>/usr/local/bin/awman</string>"),
            "{plist}"
        );
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        assert_eq!(
            plist.matches("/home/u/.awman/squad/awman.log").count(),
            2,
            "stdout and stderr both belong in the log: {plist}"
        );
    }

    /// A daemon with no environment to forward and no resolvable working
    /// directory must still produce a plist launchd will accept — an empty
    /// `<dict/>` or a stray key would make it unparseable.
    #[test]
    fn the_launchd_plist_omits_empty_optional_sections() {
        let plist = render_launchd_plist(
            "io.awman.api",
            Path::new("/usr/local/bin/awman"),
            &["api".to_string(), "start".to_string()],
            Path::new("/tmp/awman.log"),
            &[],
            None,
        );
        assert!(!plist.contains("EnvironmentVariables"), "{plist}");
        assert!(!plist.contains("WorkingDirectory"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
    }

    /// Paths and values reach the plist as XML text, so a character that ends
    /// an element early would produce a plist launchd refuses to parse — and a
    /// refusal that, before `bootstrap`, was reported as success.
    #[test]
    fn the_launchd_plist_escapes_xml_metacharacters() {
        let plist = render_launchd_plist(
            "io.awman.squad",
            Path::new("/opt/a&b/awman"),
            &["squad".to_string(), "<start>".to_string()],
            Path::new("/tmp/awman.log"),
            &[("PATH".to_string(), "/x\"y/bin".to_string())],
            None,
        );
        assert!(plist.contains("/opt/a&amp;b/awman"), "{plist}");
        assert!(plist.contains("&lt;start&gt;"), "{plist}");
        assert!(plist.contains("/x&quot;y/bin"), "{plist}");
    }

    /// `systemd_run_argv` is the pure builder `try_systemd_run` now delegates
    /// to on Linux. One `--setenv=NAME=VALUE` element per forwarded name, in
    /// the order given, and — because it is one `String` handed straight to
    /// `Command::args` with no shell involved — a value containing `=`,
    /// spaces and a colon reaches it as a single unmodified argv element.
    #[test]
    fn systemd_run_argv_emits_one_setenv_per_forwarded_name_in_order_with_tricky_value_intact() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/u".to_string()),
            (
                crate::data::config::env::AWMAN_OVERLAYS.to_string(),
                "env(A),dir(/x y:/z)".to_string(),
            ),
        ];
        let argv = systemd_run_argv(
            Path::new("/usr/local/bin/awman"),
            &["squad".to_string(), "start".to_string()],
            "io.awman.squad",
            Path::new("/home/u/.awman/squad/awman.log"),
            &env,
        );

        let setenv: Vec<&String> = argv.iter().filter(|a| a.starts_with("--setenv=")).collect();
        assert_eq!(
            setenv,
            vec![
                &"--setenv=PATH=/usr/bin".to_string(),
                &"--setenv=HOME=/home/u".to_string(),
                &"--setenv=AWMAN_OVERLAYS=env(A),dir(/x y:/z)".to_string(),
            ],
            "one element per name, in order, with '=', spaces and ':' intact: {argv:?}"
        );

        let dd = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &argv[dd + 1..],
            &["/usr/local/bin/awman", "squad", "start"],
            "the trailing tail must survive regardless of env contents: {argv:?}"
        );
    }

    /// A daemon with nothing to forward still produces a well-formed argv:
    /// no stray `--setenv` elements, and the `-- binary args` tail intact.
    #[test]
    fn systemd_run_argv_with_no_env_emits_no_setenv_elements() {
        let argv = systemd_run_argv(
            Path::new("/usr/local/bin/awman"),
            &[],
            "io.awman.api",
            Path::new("/tmp/awman.log"),
            &[],
        );
        assert!(!argv.iter().any(|a| a.starts_with("--setenv=")), "{argv:?}");
        let dd = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[dd + 1..], &["/usr/local/bin/awman"]);
    }

    /// `render_launchd_plist` is fed `ForwardedEnv::from_process()`'s output verbatim, so
    /// the names WI 0116 added to `FORWARDED_ENV` must reach the plist the
    /// same way the pre-existing ones already do.
    #[test]
    fn the_launchd_plist_carries_the_wi_0116_forwarded_names() {
        let plist = render_launchd_plist(
            "io.awman.squad",
            Path::new("/usr/local/bin/awman"),
            &["squad".to_string(), "start".to_string()],
            Path::new("/tmp/awman.log"),
            &[
                (
                    crate::data::config::env::AWMAN_OVERLAYS.to_string(),
                    "env(DEPLOY_TOKEN)".to_string(),
                ),
                (
                    crate::data::config::env::AWMAN_MAX_CONCURRENT_AGENTS.to_string(),
                    "4".to_string(),
                ),
                (
                    crate::data::config::env::AWMAN_LAUNCH_MODE.to_string(),
                    "stdio".to_string(),
                ),
            ],
            None,
        );
        assert!(plist.contains("<key>AWMAN_OVERLAYS</key>"), "{plist}");
        assert!(
            plist.contains("<string>env(DEPLOY_TOKEN)</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<key>AWMAN_MAX_CONCURRENT_AGENTS</key>"),
            "{plist}"
        );
        assert!(plist.contains("<string>4</string>"), "{plist}");
        assert!(plist.contains("<key>AWMAN_LAUNCH_MODE</key>"), "{plist}");
        assert!(plist.contains("<string>stdio</string>"), "{plist}");
    }

    /// The diagnostic explaining why the OS process manager was skipped is the
    /// only trace of it the user ever sees, so it has to reach the log.
    #[test]
    fn a_skipped_process_manager_leaves_its_reason_in_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("awman.log");
        ensure_private_log(&log).unwrap();
        note_in_log(&log, "launchctl bootstrap failed: Service is disabled\n");
        let body = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            body,
            "awman: launchctl bootstrap failed: Service is disabled\n"
        );

        // A log that cannot be opened is silently tolerated.
        note_in_log(&tmp.path().join("gone").join("awman.log"), "ignored");
    }

    #[test]
    fn is_process_alive_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn pid_is_awman_returns_false_for_a_clearly_non_awman_pid() {
        assert!(!pid_is_awman(1), "PID 1 is not awman");
    }

    #[test]
    fn check_already_running_for_unrelated_alive_pid_treats_as_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("foreign.pid");
        write_pid(&pid_path, 1).unwrap();
        let result = check_already_running(&pid_path).unwrap();
        assert!(
            result.is_none(),
            "unrelated alive PID must be treated as stale"
        );
        assert!(!pid_path.exists(), "stale PID file must be removed");
    }

    #[test]
    fn check_already_running_stale_pid_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("stale.pid");
        write_pid(&pid_path, u32::MAX - 1).unwrap();
        let result = check_already_running(&pid_path).unwrap();
        assert!(result.is_none());
        assert!(!pid_path.exists());
    }

    // ─── DaemonProcess method surface ────────────────────────────────────────
}
