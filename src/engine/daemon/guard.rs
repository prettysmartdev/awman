//! Single daemon identity and startup arbitration.
use crate::data::config::env::EnvSnapshot;
use crate::data::error::DataError;
use crate::data::fs::{daemon_paths::DaemonPaths, data_paths::DataPaths};
use crate::engine::daemon::supervisor::DaemonSupervisor;
use crate::engine::error::EngineError;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug)]
pub enum AcquireError {
    AlreadyRunning { pid: u32 },
    Blocked(EngineError),
}
impl AcquireError {
    pub fn into_engine_error(self) -> EngineError {
        match self {
            Self::AlreadyRunning { pid } => {
                EngineError::Other(format!("the awman daemon is already running (PID {pid})"))
            }
            Self::Blocked(e) => e,
        }
    }
}
impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { pid } => {
                write!(f, "the awman daemon is already running (PID {pid})")
            }
            Self::Blocked(e) => write!(f, "{e}"),
        }
    }
}
impl From<EngineError> for AcquireError {
    fn from(e: EngineError) -> Self {
        Self::Blocked(e)
    }
}
impl From<DataError> for AcquireError {
    fn from(e: DataError) -> Self {
        Self::Blocked(e.into())
    }
}

struct StartupLock {
    path: PathBuf,
    identity: String,
}
impl StartupLock {
    fn acquire(paths: &DaemonPaths) -> Result<Self, DataError> {
        use std::io::Write;
        paths.ensure_root()?;
        let path = paths.startup_lock_file();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    let identity = format!("{} {}", std::process::id(), uuid::Uuid::new_v4());
                    let lock = Self {
                        path: path.clone(),
                        identity,
                    };
                    file.write_all(lock.identity.as_bytes())
                        .map_err(|e| DataError::io(&path, e))?;
                    return Ok(lock);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let old = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(30));
                    // A slow, live startup keeps its claim even past the stale threshold.
                    let alive = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.split_whitespace().next()?.parse().ok())
                        .is_some_and(DaemonSupervisor::is_process_alive);
                    if old && !alive {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(DataError::Other(format!(
                            "another awman daemon is starting (lock held at {}); retry in a moment",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(DataError::io(&path, e)),
            }
        }
    }
}
impl Drop for StartupLock {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(&self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct DaemonClaim {
    _lock: StartupLock,
    pid: u32,
    paths: DaemonPaths,
}
impl DaemonClaim {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn remove_stale_endpoint(&self, path: &Path) -> Result<(), EngineError> {
        if path != self.paths.local_endpoint() {
            return Err(EngineError::Other(
                "endpoint does not belong to this daemon startup claim".into(),
            ));
        }
        if crate::data::fs::daemon_process::DaemonProcess::unified(self.paths.clone()).read_pid()?
            != Some(self.pid)
        {
            return Err(EngineError::Other(
                "daemon startup claim is no longer owned".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_socket() => {
                    std::fs::remove_file(path).map_err(|e| DataError::io(path, e))?
                }
                Ok(_) => {
                    return Err(EngineError::Other(format!(
                        "refusing to remove non-socket endpoint {}",
                        path.display()
                    )))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(DataError::io(path, e).into()),
            }
        }
        Ok(())
    }
    pub fn finish(self) {}
}

pub struct DaemonGuard {
    paths: DaemonPaths,
    supervisor: DaemonSupervisor,
}
impl DaemonGuard {
    pub fn new(paths: DaemonPaths) -> Self {
        Self {
            supervisor: DaemonSupervisor::for_paths(paths.clone()),
            paths,
        }
    }
    pub fn for_env(env: &EnvSnapshot) -> Result<Self, EngineError> {
        Ok(Self::new(DataPaths::from_env(env)?.daemon()))
    }
    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }
    pub fn supervisor(&self) -> &DaemonSupervisor {
        &self.supervisor
    }
    pub fn running_pid(&self) -> Result<Option<u32>, EngineError> {
        self.supervisor.running_pid()
    }
    pub fn acquire(&self, pid: u32) -> Result<DaemonClaim, AcquireError> {
        let lock = StartupLock::acquire(&self.paths)?;
        if let Some(pid) = self.running_pid()? {
            return Err(AcquireError::AlreadyRunning { pid });
        }
        if !self.supervisor.process().claim_pidfile(pid)? {
            return Err(AcquireError::Blocked(EngineError::Other(
                "daemon pidfile changed during startup".into(),
            )));
        }
        Ok(DaemonClaim {
            _lock: lock,
            pid,
            paths: self.paths.clone(),
        })
    }
    pub fn release_owned_by(&self, pid: u32) -> Result<bool, EngineError> {
        Ok(self.supervisor.process().release_pidfile_owned_by(pid)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_identity_lock_and_stale_pid() {
        let dir = tempfile::tempdir().unwrap();
        let guard = DaemonGuard::new(DaemonPaths::new(dir.path().join("daemon")));
        guard.paths().ensure_root().unwrap();
        std::fs::write(guard.paths.pid_file(), "2147483647").unwrap();
        let claim = guard.acquire(std::process::id()).unwrap();
        assert!(guard.paths.startup_lock_file().exists());
        claim.finish();
        assert!(!guard.paths.startup_lock_file().exists());
        assert!(matches!(
            guard.acquire(std::process::id()),
            Err(AcquireError::AlreadyRunning { .. })
        ));
        assert!(!guard.release_owned_by(123).unwrap());
        assert!(guard.release_owned_by(std::process::id()).unwrap());
    }
    #[test]
    fn stale_startup_lock_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::new(dir.path().join("daemon"));
        paths.ensure_root().unwrap();
        let file = std::fs::File::create(paths.startup_lock_file()).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(31))
            .unwrap();
        let guard = DaemonGuard::new(paths);
        guard.acquire(std::process::id()).unwrap().finish();
    }
    #[cfg(unix)]
    #[test]
    fn claim_removes_only_stale_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let guard = DaemonGuard::new(DaemonPaths::new(dir.path().join("daemon")));
        guard.paths().ensure_root().unwrap();
        let endpoint = guard.paths().local_endpoint();
        let socket = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        drop(socket);
        let claim = guard.acquire(std::process::id()).unwrap();
        claim.remove_stale_endpoint(&endpoint).unwrap();
        std::fs::write(&endpoint, "keep").unwrap();
        assert!(claim.remove_stale_endpoint(&endpoint).is_err());
    }
}
