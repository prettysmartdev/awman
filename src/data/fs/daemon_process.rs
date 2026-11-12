//! `DaemonProcess` — PID-file lifecycle and server-meta persistence for a
//! single awman daemon.
//!
//! Layer 0 owns the on-disk state: the PID file, the `ServerMeta` sidecar,
//! the daemon's paths, and its fixed `awman-daemon` systemd unit and
//! `io.awman.daemon` launchd label.
//!
//! The *process* half — liveness probes, `systemd-run`, launchd, the direct
//! spawn, SIGTERM/`taskkill` — moved to `engine::daemon::DaemonSupervisor` in
//! WI 0114 F-29 (decision Q1: no process spawning in Layer 0). A supervisor
//! owns one of these and reads the pidfile through it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::config_file::write_config_file_atomically;
use crate::data::fs::daemon_paths::DaemonPaths;

/// systemd unit name / launchd plist label for the API daemon.
pub const DAEMON_UNIT_NAME: &str = "awman-daemon";
pub const DAEMON_PLIST_LABEL: &str = "io.awman.daemon";
pub const API_UNIT_NAME: &str = DAEMON_UNIT_NAME;
pub const API_PLIST_LABEL: &str = DAEMON_PLIST_LABEL;

/// systemd unit name / launchd plist label for the squad daemon.
pub const SQUAD_UNIT_NAME: &str = DAEMON_UNIT_NAME;
pub const SQUAD_PLIST_LABEL: &str = DAEMON_PLIST_LABEL;

/// Sidecar metadata for a running daemon. Written next to the PID file when
/// the server boots so other commands (status, kill) can locate the bound
/// endpoint without re-parsing flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerMeta {
    pub port: u16,
    pub bind_ip: String,
    pub scheme: String,
    /// Legacy sidecar migration field. Current daemon startup does not permit
    /// unauthenticated network listeners; the removed skip-auth flag is rejected.
    #[serde(default)]
    pub auth_disabled: bool,
}

/// Typed owner of one daemon's PID / meta / spawn lifecycle.
pub struct DaemonProcess {
    paths: DaemonPaths,
    unit_name: &'static str,
    plist_label: &'static str,
}

impl DaemonProcess {
    /// Construct over a daemon's paths and its systemd/launchd identity.
    pub fn new(paths: DaemonPaths, _unit_name: &'static str, _plist_label: &'static str) -> Self {
        Self {
            paths,
            unit_name: DAEMON_UNIT_NAME,
            plist_label: DAEMON_PLIST_LABEL,
        }
    }

    /// Unified constructor; `new` is a temporary legacy shim.
    pub fn unified(paths: DaemonPaths) -> Self {
        Self::new(paths, DAEMON_UNIT_NAME, DAEMON_PLIST_LABEL)
    }

    /// The daemon's paths.
    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    /// The systemd unit name this daemon runs under.
    pub fn unit_name(&self) -> &'static str {
        self.unit_name
    }

    /// The launchd plist label this daemon runs under.
    pub fn plist_label(&self) -> &'static str {
        self.plist_label
    }

    /// Raw PID read with no liveness check.
    pub fn read_pid(&self) -> Result<Option<u32>, DataError> {
        read_pid(&self.paths.pid_file())
    }

    /// Race-safe exclusive PID claim (`O_CREAT|O_EXCL`). Returns `Ok(false)`
    /// when the file already exists. (Was `write_pid_exclusive`.)
    pub fn claim_pidfile(&self, pid: u32) -> Result<bool, DataError> {
        write_pid_exclusive(&self.paths.pid_file(), pid)
    }

    /// Truncating PID overwrite. (Was `write_pid`.)
    pub fn force_write_pidfile(&self, pid: u32) -> Result<(), DataError> {
        write_pid(&self.paths.pid_file(), pid)
    }

    /// Remove the PID file (idempotent). (Was `clear_pid`.)
    pub fn release_pidfile(&self) -> Result<(), DataError> {
        clear_pid(&self.paths.pid_file())
    }

    /// Remove the PID file **only while it still names `pid`**, reporting
    /// whether it did.
    ///
    /// The pidfile is the claim on a daemon root, so it is also the one
    /// ownership token an exiting daemon can check. `awman squad stop` sends
    /// SIGTERM and releases the pidfile immediately, without waiting for the
    /// process to go: by the time the dying daemon reaches its own teardown a
    /// *successor* may already have claimed the root. An unconditional
    /// [`release_pidfile`](Self::release_pidfile) there deletes the
    /// successor's claim, and the next command — finding no pidfile — starts a
    /// third daemon that inherits none of the payload environment the
    /// successor was just handed.
    pub fn release_pidfile_owned_by(&self, pid: u32) -> Result<bool, DataError> {
        clear_pid_owned_by(&self.paths.pid_file(), pid)
    }

    /// Whether the pidfile currently names `pid` — that is, whether this
    /// process still holds the claim on the daemon root.
    pub fn owns_pidfile(&self, pid: u32) -> Result<bool, DataError> {
        Ok(self.read_pid()? == Some(pid))
    }

    /// Persist server bind metadata.
    pub fn write_meta(&self, meta: &ServerMeta) -> Result<(), DataError> {
        write_server_meta(&self.paths.server_meta_file(), meta)
    }

    /// Read server bind metadata, or `None` when absent.
    pub fn read_meta(&self) -> Result<Option<ServerMeta>, DataError> {
        read_server_meta(&self.paths.server_meta_file())
    }

    /// Remove the server metadata file (idempotent).
    pub fn clear_meta(&self) -> Result<(), DataError> {
        clear_server_meta(&self.paths.server_meta_file())
    }
}

// ─── Ported free functions (now pub(crate) implementation details) ──────────

/// Truncating PID write — overwrites whatever is already on disk.
pub(crate) fn write_pid(pid_path: &Path, pid: u32) -> Result<(), DataError> {
    let parent = pid_path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        crate::data::fs::private_files::create_private_dir(parent)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| DataError::io(parent, e))?;
    std::io::Write::write_all(&mut file, pid.to_string().as_bytes())
        .map_err(|e| DataError::io(pid_path, e))?;
    file.persist(pid_path)
        .map_err(|e| DataError::io(pid_path, e.error))?;
    Ok(())
}

/// Race-safe PID write via `O_CREAT|O_EXCL`. `Ok(false)` when the file exists.
///
/// Writes the content to a private temp file first, then publishes it with
/// `hard_link` (which fails with `AlreadyExists` exactly like `create_new`
/// would). Publishing this way — rather than `create_new` followed by a
/// separate `write_all` — closes a real race: a concurrent reader (e.g. a
/// second daemon's `DaemonGuard::check` racing this claim) could otherwise
/// observe the freshly-created-but-still-empty pidfile between the two
/// syscalls and fail with a spurious "invalid PID" error instead of either
/// seeing the claim or finding nothing.
pub(crate) fn write_pid_exclusive(pid_path: &Path, pid: u32) -> Result<bool, DataError> {
    if let Some(parent) = pid_path.parent() {
        if !parent.exists() {
            crate::data::fs::private_files::create_private_dir(parent)?;
        }
    }
    let tmp_path = pid_path.with_file_name(format!(
        "{}.tmp.{}",
        pid_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pidfile"),
        uuid::Uuid::new_v4()
    ));
    let mut file = crate::data::fs::private_files::create_private_file_new(&tmp_path)?;
    if let Err(e) = std::io::Write::write_all(&mut file, pid.to_string().as_bytes()) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(DataError::io(&tmp_path, e));
    }
    let result = std::fs::hard_link(&tmp_path, pid_path);
    let _ = std::fs::remove_file(&tmp_path);
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

pub(crate) fn read_pid(pid_path: &Path) -> Result<Option<u32>, DataError> {
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

pub(crate) fn clear_pid(pid_path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(pid_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(pid_path, e)),
    }
}

/// `clear_pid`, but only when the file still names `pid`. `Ok(false)` — and no
/// removal — when it is absent, unreadable, or names somebody else.
pub(crate) fn clear_pid_owned_by(pid_path: &Path, pid: u32) -> Result<bool, DataError> {
    // A pidfile we cannot parse is not one we can claim to own, so it is left
    // exactly where it is: `running_pid` already reports it, and guessing here
    // would delete a file this process has no evidence about.
    match read_pid(pid_path) {
        Ok(Some(existing)) if existing == pid => {
            clear_pid(pid_path)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(_) => Ok(false),
    }
}

/// Persist server bind metadata (port, scheme, bind IP).
pub(crate) fn write_server_meta(meta_path: &Path, meta: &ServerMeta) -> Result<(), DataError> {
    if let Some(parent) = meta_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    let json = serde_json::to_string(meta)
        .map_err(|e| DataError::Other(format!("serialize ServerMeta: {e}")))?;
    // Atomic: `read_server_meta` treats a partial file as a parse error.
    write_config_file_atomically(meta_path, &json)
}

pub(crate) fn read_server_meta(meta_path: &Path) -> Result<Option<ServerMeta>, DataError> {
    match std::fs::read_to_string(meta_path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| DataError::Other(format!("parse ServerMeta: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(meta_path, e)),
    }
}

pub(crate) fn clear_server_meta(meta_path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(meta_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(meta_path, e)),
    }
}

/// Create (or tighten) the daemon log file so it is owner-read/write only.
/// Existing files keep their contents; only the mode is enforced.
pub(crate) fn ensure_private_log(log_path: &Path) -> Result<(), DataError> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(log_path)
        .map_err(|e| DataError::io(log_path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(log_path)
            .map_err(|e| DataError::io(log_path, e))?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(log_path, perms).map_err(|e| DataError::io(log_path, e))?;
    }
    Ok(())
}
