//! `server.json` — owner-private endpoint metadata for the running daemon.
//!
//! Layer 0. The daemon writes this after binding its local listener so local
//! clients can learn *which* transport it bound and *which* build/protocol it
//! speaks. It never holds a secret: access to the daemon is decided by the
//! local transport itself (socket mode and peer UID on Unix, owner-only pipe
//! ACL and caller identity on Windows). A missing, stale or tampered sidecar
//! therefore cannot grant access; at worst it makes a client refuse to talk.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::private_files::{
    create_private_dir, read_private_json, remove_private_file, write_private_json_atomic,
};

/// Protocol family name for the HTTP resource API.
pub const PROTOCOL_API: &str = "api";
/// The API protocol version this build speaks.
pub const PROTOCOL_API_VERSION: u32 = 1;

/// The kind of local transport the daemon bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarTransportKind {
    UnixSocket,
    NamedPipe,
}

impl SidecarTransportKind {
    /// The only kind this platform can serve or connect to.
    pub fn native() -> Self {
        if cfg!(windows) {
            SidecarTransportKind::NamedPipe
        } else {
            SidecarTransportKind::UnixSocket
        }
    }
}

/// Where the local listener is: a socket path or a pipe name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarTransport {
    pub kind: SidecarTransportKind,
    pub address: String,
}

/// Endpoint metadata only, never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSidecar {
    /// `CARGO_PKG_VERSION` of the daemon that wrote it.
    pub version: String,
    /// Protocol versions spoken, e.g. `{"api": 1}`.
    pub protocol: BTreeMap<String, u32>,
    pub pid: u32,
    pub transport: SidecarTransport,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl ServerSidecar {
    /// The sidecar this build writes for `pid` serving on `transport`.
    pub fn current(pid: u32, transport: SidecarTransport) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: BTreeMap::from([
                (PROTOCOL_API.to_string(), PROTOCOL_API_VERSION),
                ("frontend".into(), 1),
            ]),
            pid,
            transport,
            started_at: chrono::Utc::now(),
        }
    }

    /// The API protocol version recorded, if any.
    pub fn api_protocol(&self) -> Option<u32> {
        self.protocol.get(PROTOCOL_API).copied()
    }

    /// Atomically replace `path` with this record (0600, private parent dir).
    pub fn write(&self, path: &Path) -> Result<(), DataError> {
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        write_private_json_atomic(path, self, || Ok(()))
    }

    /// `Ok(None)` when absent. A file that is malformed, a symlink, not owned
    /// by this user or open to others is an error, never adopted.
    pub fn read(path: &Path) -> Result<Option<ServerSidecar>, DataError> {
        read_private_json(path)
    }

    /// Remove the record. Absent is success.
    pub fn remove(path: &Path) -> Result<(), DataError> {
        remove_private_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServerSidecar {
        ServerSidecar::current(
            42,
            SidecarTransport {
                kind: SidecarTransportKind::UnixSocket,
                address: "/x/api.sock".into(),
            },
        )
    }

    #[test]
    fn round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon").join("server.json");
        let sidecar = sample();
        sidecar.write(&path).unwrap();
        assert_eq!(ServerSidecar::read(&path).unwrap(), Some(sidecar));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o077, 0);
        }
    }

    #[test]
    fn wire_names_are_snake_case_and_hold_no_secret() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(json["transport"]["kind"], "unix_socket");
        assert_eq!(json["protocol"]["api"], 1);
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["pid", "protocol", "started_at", "transport", "version"]
        );
    }

    #[test]
    fn absent_is_none_and_remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        assert_eq!(ServerSidecar::read(&path).unwrap(), None);
        ServerSidecar::remove(&path).unwrap();
    }

    #[test]
    fn malformed_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let mut file = crate::data::fs::private_files::create_private_file_new(&path).unwrap();
        std::io::Write::write_all(&mut file, b"{not json").unwrap();
        assert!(ServerSidecar::read(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn world_readable_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon").join("server.json");
        sample().write(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ServerSidecar::read(&path).is_err());
    }
}
