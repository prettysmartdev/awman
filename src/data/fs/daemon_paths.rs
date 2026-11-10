//! Paths for the single daemon identity.
use std::path::{Path, PathBuf};

use crate::data::error::DataError;

/// Path accessors for one daemon's identity files, all under a single `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    root: PathBuf,
}

impl DaemonPaths {
    /// Construct over a root and the API-key filename stem (`"api_key"` /
    /// `"squad_key"`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The daemon root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// PID file: `<root>/awman.pid`.
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("awman.pid")
    }

    /// Log file: `<root>/awman.log`.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("awman.log")
    }

    /// Server metadata sidecar: `<root>/server.json`.
    pub fn server_meta_file(&self) -> PathBuf {
        self.root.join("server.json")
    }

    /// API-key hash file: `<root>/<key_stem>.hash`.
    pub fn key_hash_file(&self) -> PathBuf {
        self.root.join("api_key.hash")
    }

    /// Owner-authenticated resource API endpoint, separate from the TCP listener.
    pub fn local_endpoint(&self) -> PathBuf {
        #[cfg(unix)]
        {
            use sha2::{Digest, Sha256};
            let direct = self.root.join("api.sock");
            if direct.as_os_str().as_encoded_bytes().len() < 100 {
                return direct;
            }
            let uid = nix::unistd::geteuid().as_raw();
            let hash = Sha256::digest(self.root.to_string_lossy().as_bytes());
            let hex: String = hash[..4].iter().map(|b| format!("{b:02x}")).collect();
            let suffix = format!("awman-{uid}-{hex}.sock");
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute());
            // Clients can have a different TMPDIR from the daemon (for example
            // when a service manager starts it). The owner endpoint must still
            // be discoverable from the same daemon home.
            let mut dir = runtime_dir.clone().unwrap_or_else(|| PathBuf::from("/tmp"));
            // Keep the Unix-domain socket below the strictest common SUN_LEN.
            if dir.join(&suffix).as_os_str().as_encoded_bytes().len() >= 100 {
                dir = PathBuf::from("/tmp").join(format!("awman-{uid}"));
            } else if runtime_dir.is_none() {
                dir = dir.join(format!("awman-{uid}"));
            }
            dir.join(suffix)
        }
        #[cfg(windows)]
        {
            let hash = crate::data::fs::hash::sha256_hex(self.root.to_string_lossy().as_bytes());
            PathBuf::from(format!(r"\\.\pipe\awman-{hash}"))
        }
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        crate::data::fs::private_files::create_private_dir(&self.root)
    }

    /// Remove only a socket, never a symlink or regular file, during owned teardown.
    pub fn remove_local_endpoint(&self, path: &Path) -> Result<(), DataError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_socket() => {
                    std::fs::remove_file(path).map_err(|e| DataError::io(path, e))?
                }
                Ok(_) => {
                    return Err(DataError::Other(format!(
                        "refusing to remove non-socket {}",
                        path.display()
                    )))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(DataError::io(path, e)),
            }
        }
        Ok(())
    }
    pub fn startup_lock_file(&self) -> PathBuf {
        self.root.join("startup.lock")
    }
    pub fn api_keys_file(&self) -> PathBuf {
        self.root.join("api_keys.json")
    }
    /// Browser grant metadata (hashes only), owner-private like the keys.
    pub fn launch_contexts_file(&self) -> PathBuf {
        self.root.join("launch_contexts.json")
    }

    pub fn browser_grants_file(&self) -> PathBuf {
        self.root.join("browser_grants.json")
    }
    pub fn tls_dir(&self) -> PathBuf {
        self.root.join("tls")
    }
    pub fn tls_cert_file(&self) -> PathBuf {
        self.tls_dir().join("cert.pem")
    }
    pub fn tls_key_file(&self) -> PathBuf {
        self.tls_dir().join("key.pem")
    }
    pub fn tls_sans_file(&self) -> PathBuf {
        self.tls_dir().join("sans.json")
    }
    pub fn tls_fingerprint_file(&self) -> PathBuf {
        self.tls_dir().join("fingerprint.sha256")
    }

    /// Read the persisted key hash, trimmed, or `None` when absent.
    ///
    /// Reproduces `AuthEngine::read_api_key_hash` so Layer 3 / the auth engine
    /// never touch `std::fs` for this file directly.
    pub fn read_key_hash(&self) -> Result<Option<String>, DataError> {
        let path = self.key_hash_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DataError::io(path, e)),
        }
    }

    /// Persist the key hash (hex) with mode `0o600` on Unix, creating the root
    /// directory if needed. Reproduces `AuthEngine::write_api_key_hash`
    /// byte-for-byte, including the secure-write permissions.
    pub fn write_key_hash(&self, hex: &str) -> Result<(), DataError> {
        self.ensure_root()?;
        let path = self.key_hash_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        write_file_secure(&path, hex.as_bytes())
    }
}

/// Write `content` to `path`, creating it with mode `0o600` on Unix and
/// truncating an existing file. Mirrors `engine::auth::write_file_secure`.
fn write_file_secure(path: &Path, content: &[u8]) -> Result<(), DataError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| DataError::io(path, e))?;
        std::io::Write::write_all(&mut f, content).map_err(|e| DataError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content).map_err(|e| DataError::io(path, e))?;
    }
    Ok(())
}
