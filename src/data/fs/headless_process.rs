//! PID file lifecycle and background process spawning for the headless server.
//!
//! Ported from `oldsrc/commands/headless/process.rs`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Sidecar metadata for the running headless server. Written next to the PID
/// file when the server boots so other commands (status, kill) can locate
/// the bound endpoint without re-parsing flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerMeta {
    pub port: u16,
    pub bind_ip: String,
    pub scheme: String,
}

/// Truncating PID write — overwrites whatever is already on disk. Used after
/// `check_already_running` has cleaned up a stale file. Prefer
/// [`write_pid_exclusive`] for the start-of-server race-safe path.
pub fn write_pid(pid_path: &Path, pid: u32) -> Result<(), DataError> {
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    std::fs::write(pid_path, pid.to_string()).map_err(|e| DataError::io(pid_path, e))
}

/// Race-safe PID write: opens the PID file with `O_CREAT|O_EXCL` so two
/// concurrent `headless start` invocations cannot both pass the
/// `check_already_running` check and end up overwriting each other's PID.
/// Returns `Ok(false)` when the file already exists (caller should re-run
/// `check_already_running`).
pub fn write_pid_exclusive(pid_path: &Path, pid: u32) -> Result<bool, DataError> {
    use std::io::Write as _;
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pid_path)
    {
        Ok(mut f) => {
            f.write_all(pid.to_string().as_bytes())
                .map_err(|e| DataError::io(pid_path, e))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

pub fn read_pid(pid_path: &Path) -> Result<Option<u32>, DataError> {
    match std::fs::read_to_string(pid_path) {
        Ok(content) => {
            let pid: u32 = content
                .trim()
                .parse()
                .map_err(|_| DataError::Other(format!("invalid PID in {}", pid_path.display())))?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

pub fn clear_pid(pid_path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(pid_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

/// Persist server bind metadata (port, scheme, bind IP) so `status`/`kill`
/// can probe the right endpoint without flag parsing.
pub fn write_server_meta(meta_path: &Path, meta: &ServerMeta) -> Result<(), DataError> {
    if let Some(parent) = meta_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    let json = serde_json::to_string(meta)
        .map_err(|e| DataError::Other(format!("serialize ServerMeta: {e}")))?;
    std::fs::write(meta_path, json).map_err(|e| DataError::io(meta_path, e))
}

pub fn read_server_meta(meta_path: &Path) -> Result<Option<ServerMeta>, DataError> {
    match std::fs::read_to_string(meta_path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| DataError::Other(format!("parse ServerMeta: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(meta_path, e)),
    }
}

pub fn clear_server_meta(meta_path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(meta_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(meta_path, e)),
    }
}

#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!(",\"{}\",", pid)))
        .unwrap_or(false)
}

/// Returns `true` when the OS reports the process command name contains "amux".
/// Used to disambiguate stale PID files from PIDs reused by unrelated processes
/// after a reboot. On platforms where the command name is not readable, returns
/// `true` so we err on the side of "trust the PID file" — matches old-amux.
#[cfg(target_os = "linux")]
pub fn pid_is_amux(pid: u32) -> bool {
    let path = format!("/proc/{pid}/comm");
    std::fs::read_to_string(&path)
        .map(|s| s.trim().contains("amux"))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn pid_is_amux(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().contains("amux"))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
pub fn pid_is_amux(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("amux")
        })
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn pid_is_amux(_pid: u32) -> bool {
    true
}

/// Check the PID file. Returns `Some(pid)` only when the process is alive AND
/// looks like an amux server. Stale or wrong-process PIDs are cleaned up.
pub fn check_already_running(pid_path: &Path) -> Result<Option<u32>, DataError> {
    match read_pid(pid_path)? {
        Some(pid) if is_process_alive(pid) && pid_is_amux(pid) => Ok(Some(pid)),
        Some(_) => {
            clear_pid(pid_path)?;
            Ok(None)
        }
        None => Ok(None),
    }
}

#[cfg(unix)]
pub fn kill_process(pid: u32) -> Result<(), DataError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| DataError::Other(format!("failed to send SIGTERM to PID {pid}: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn kill_process(pid: u32) -> Result<(), DataError> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .map_err(|e| DataError::Other(format!("failed to terminate PID {pid}: {e}")))?;
    if !status.success() {
        return Err(DataError::Other(format!("taskkill /PID {pid} /F failed")));
    }
    Ok(())
}

/// Spawn the headless server in the background. Returns the child PID.
pub fn spawn_background(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
) -> Result<u32, DataError> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(pid) = try_systemd_run(binary_path, args)? {
            return Ok(pid);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(pid) = try_launchd(binary_path, args, log_path)? {
            return Ok(pid);
        }
    }

    double_fork_spawn(binary_path, args)
}

#[cfg(target_os = "linux")]
fn try_systemd_run(binary_path: &Path, args: &[String]) -> Result<Option<u32>, DataError> {
    let check = std::process::Command::new("systemd-run")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match check {
        Ok(s) if s.success() => {}
        _ => return Ok(None),
    }

    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", "--unit=amux-headless", "--"])
        .arg(binary_path)
        .args(args);

    let status = cmd
        .status()
        .map_err(|e| DataError::Other(format!("systemd-run failed: {e}")))?;
    if !status.success() {
        return Ok(None);
    }
    // systemd-run returns immediately; the actual PID is tracked by the unit.
    // Return 0 as a sentinel — the PID file will be written by the child.
    Ok(Some(0))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn try_launchd(
    binary_path: &Path,
    args: &[String],
    log_path: &Path,
) -> Result<Option<u32>, DataError> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let plist_path = home.join("Library/LaunchAgents/io.amux.headless.plist");
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }

    let mut program_args = format!(
        "    <string>{}</string>\n",
        xml_escape(&binary_path.to_string_lossy())
    );
    for arg in args {
        program_args.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.amux.headless</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(&log_path.to_string_lossy())
    );

    std::fs::write(&plist_path, plist).map_err(|e| DataError::io(&plist_path, e))?;

    let status = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status()
        .map_err(|e| DataError::Other(format!("launchctl load failed: {e}")))?;

    if !status.success() {
        let _ = std::fs::remove_file(&plist_path);
        return Ok(None);
    }
    Ok(Some(0))
}

fn double_fork_spawn(binary_path: &Path, args: &[String]) -> Result<u32, DataError> {
    let mut cmd = std::process::Command::new(binary_path);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // On Unix this matches old-amux exactly: a single Command::spawn. True
    // setsid daemonization would require `pre_exec`, which is unsafe — and
    // this crate is `#![forbid(unsafe_code)]`. The systemd-run / launchd
    // happy paths above handle real detachment when the OS supports it.

    // On Windows, ensure the child gets its own process group so that a
    // Ctrl-C delivered to the parent console does not also kill the daemon.
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

    #[test]
    fn write_pid_exclusive_rejects_second_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("excl.pid");
        // First writer wins.
        let r1 = write_pid_exclusive(&pid_path, 100).unwrap();
        assert!(r1, "first exclusive write must succeed");
        // Second writer is rejected without overwriting.
        let r2 = write_pid_exclusive(&pid_path, 200).unwrap();
        assert!(!r2, "second exclusive write must be rejected");
        let on_disk = read_pid(&pid_path).unwrap();
        assert_eq!(on_disk, Some(100), "first writer's PID must survive");
    }

    #[test]
    fn pid_file_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("test.pid");
        write_pid(&pid_path, 12345).unwrap();
        assert_eq!(read_pid(&pid_path).unwrap(), Some(12345));
        clear_pid(&pid_path).unwrap();
        assert_eq!(read_pid(&pid_path).unwrap(), None);
    }

    #[test]
    fn clear_pid_idempotent_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("nonexistent.pid");
        assert!(clear_pid(&pid_path).is_ok());
    }

    #[test]
    fn is_process_alive_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn is_process_alive_bogus_pid() {
        assert!(!is_process_alive(u32::MAX - 1));
    }

    #[test]
    fn pid_is_amux_returns_false_for_a_clearly_non_amux_pid() {
        // PID 1 is `init`/`launchd` on Unix and `System Idle Process` on Windows;
        // none of those are named "amux".
        assert!(!pid_is_amux(1), "PID 1 is not amux");
    }

    #[test]
    fn check_already_running_for_unrelated_alive_pid_treats_as_stale() {
        // PID 1 is alive on every Unix-y test host (init/launchd) but is NOT
        // amux. check_already_running must treat that as stale and clean up.
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
}
