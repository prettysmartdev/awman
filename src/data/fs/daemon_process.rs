//! `DaemonProcess` — PID-file lifecycle and server-meta persistence for a
//! long-lived awman daemon (the API server or squad).
//!
//! Layer 0 owns the on-disk state: the PID file, the `ServerMeta` sidecar,
//! the daemon's paths, and the systemd unit name / launchd plist label that
//! identify it, so two daemons never collide on `--unit=awman-api` or the
//! `io.awman.api` plist.
//!
//! The *process* half — liveness probes, `systemd-run`, launchd, the direct
//! spawn, SIGTERM/`taskkill` — moved to `engine::daemon::DaemonSupervisor` in
//! WI 0114 F-29 (decision Q1: no process spawning in Layer 0). A supervisor
//! owns one of these and reads the pidfile through it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;

/// systemd unit name / launchd plist label for the API daemon.
pub const API_UNIT_NAME: &str = "awman-api";
pub const API_PLIST_LABEL: &str = "io.awman.api";

/// systemd unit name / launchd plist label for the squad daemon.
pub const SQUAD_UNIT_NAME: &str = "awman-squad";
pub const SQUAD_PLIST_LABEL: &str = "io.awman.squad";

/// Sidecar metadata for a running daemon. Written next to the PID file when
/// the server boots so other commands (status, kill) can locate the bound
/// endpoint without re-parsing flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerMeta {
    pub port: u16,
    pub bind_ip: String,
    pub scheme: String,
    /// True when the daemon was started with `--dangerously-skip-auth` and is
    /// therefore serving unauthenticated. Clients read this to avoid minting a
    /// bearer key (and writing a key hash) the running daemon will never check.
    /// Absent in sidecars written by older versions, which always required auth.
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
    pub fn new(paths: DaemonPaths, unit_name: &'static str, plist_label: &'static str) -> Self {
        Self {
            paths,
            unit_name,
            plist_label,
        }
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
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    std::fs::write(pid_path, pid.to_string()).map_err(|e| DataError::io(pid_path, e))
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
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    }
    let tmp_path = pid_path.with_file_name(format!(
        "{}.tmp.{}",
        pid_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pidfile"),
        std::process::id()
    ));
    std::fs::write(&tmp_path, pid.to_string()).map_err(|e| DataError::io(&tmp_path, e))?;
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
    std::fs::write(meta_path, json).map_err(|e| DataError::io(meta_path, e))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn write_pid_exclusive_rejects_second_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("excl.pid");
        let r1 = write_pid_exclusive(&pid_path, 100).unwrap();
        assert!(r1, "first exclusive write must succeed");
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

    fn api_daemon(root: &Path) -> DaemonProcess {
        DaemonProcess::new(
            DaemonPaths::new(root, "api_key"),
            API_UNIT_NAME,
            API_PLIST_LABEL,
        )
    }

    #[test]
    fn claim_and_release_pidfile_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        assert!(d.claim_pidfile(4242).unwrap(), "first claim wins");
        assert!(!d.claim_pidfile(9999).unwrap(), "second claim rejected");
        assert_eq!(d.read_pid().unwrap(), Some(4242));
        d.release_pidfile().unwrap();
        assert_eq!(d.read_pid().unwrap(), None);
    }

    /// The shutdown path's release: a daemon that still holds the claim drops
    /// it, exactly as the unconditional release would.
    #[test]
    fn release_pidfile_owned_by_drops_our_own_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        d.claim_pidfile(4242).unwrap();
        assert!(d.owns_pidfile(4242).unwrap());
        assert!(d.release_pidfile_owned_by(4242).unwrap(), "ours to release");
        assert_eq!(d.read_pid().unwrap(), None);
    }

    /// The reason the ownership check exists. `awman squad stop` releases the
    /// pidfile as soon as it has signalled, so a successor can claim the root
    /// while the old daemon is still on its way out. That daemon's teardown
    /// must leave the successor's claim — and therefore its endpoint sidecar —
    /// alone, or the next command starts a *third* daemon holding none of the
    /// payload environment the successor was handed.
    #[test]
    fn release_pidfile_owned_by_leaves_a_successors_claim_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        // The stopper released our pidfile; the successor claimed it.
        d.claim_pidfile(777).unwrap();

        assert!(!d.owns_pidfile(4242).unwrap(), "not ours any more");
        assert!(
            !d.release_pidfile_owned_by(4242).unwrap(),
            "a dying daemon must report that it had nothing to release"
        );
        assert_eq!(
            d.read_pid().unwrap(),
            Some(777),
            "the successor's claim must survive its predecessor's teardown"
        );
    }

    /// Nothing to own is not something to delete, and neither is a pidfile
    /// this process cannot even parse.
    #[test]
    fn release_pidfile_owned_by_is_a_no_op_on_an_absent_or_unreadable_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        assert!(!d.release_pidfile_owned_by(4242).unwrap(), "nothing there");

        std::fs::write(d.paths().pid_file(), "not-a-pid").unwrap();
        assert!(!d.release_pidfile_owned_by(4242).unwrap(), "not parseable");
        assert!(
            d.paths().pid_file().exists(),
            "an unparseable pidfile is left for running_pid to report on"
        );
    }

    #[test]
    fn meta_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = api_daemon(tmp.path());
        assert_eq!(d.read_meta().unwrap(), None);
        let meta = ServerMeta {
            port: 8080,
            bind_ip: "127.0.0.1".into(),
            scheme: "https".into(),
            auth_disabled: false,
        };
        d.write_meta(&meta).unwrap();
        assert_eq!(d.read_meta().unwrap(), Some(meta));
        d.clear_meta().unwrap();
        assert_eq!(d.read_meta().unwrap(), None);
    }

    #[test]
    fn distinct_unit_and_plist_for_api_and_squad() {
        assert_ne!(API_UNIT_NAME, SQUAD_UNIT_NAME);
        assert_ne!(API_PLIST_LABEL, SQUAD_PLIST_LABEL);
    }
}
