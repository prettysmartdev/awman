//! Private, stable runtime roots. Nothing is silently moved to shared /tmp.
//!
//! The layout, locks and attach-socket paths live in Layer 0
//! (`data::builtin_state`); this handle adds the engine's policy: the
//! socket-path budget and the mapping of Layer 0 failures onto `EngineError`.
use crate::data::builtin_state::{BuiltinState, RuntimeMarker};
use crate::data::error::DataError;
use crate::engine::error::{run_checked, EngineError};
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;

pub const SOCKET_PATH_LIMIT: usize = 103;

#[derive(Clone)]
pub struct BuiltinPaths {
    pub home: PathBuf,
}

/// Map a Layer 0 state failure onto the engine's error vocabulary.
pub(super) fn state_error(error: DataError) -> EngineError {
    match error {
        DataError::BuiltinState(message) => EngineError::Config(message),
        busy @ DataError::BuiltinStateBusy { .. } => EngineError::BuiltinRuntimeUnavailable {
            reason: busy.to_string(),
        },
        DataError::Io { path, source } => EngineError::io(path, source),
        other => EngineError::Data(other),
    }
}

impl BuiltinPaths {
    pub fn resolve(root: &Path) -> Result<Self, EngineError> {
        // The socket budget is engine policy, applied by Layer 0 to the root
        // before anything is created and again to its canonical form.
        let mut over_budget = None;
        let resolved = BuiltinState::resolve(root, |candidate| {
            Self {
                home: candidate.to_path_buf(),
            }
            .check_socket_budget()
            .map_err(|e| {
                over_budget = Some(e);
                DataError::Other("socket path budget exceeded".into())
            })
        });
        match (resolved, over_budget) {
            (_, Some(e)) => Err(e),
            (Ok(state), None) => Ok(Self { home: state.home }),
            (Err(e), None) => Err(state_error(e)),
        }
    }

    pub(super) fn state(&self) -> BuiltinState {
        BuiltinState::at(&self.home)
    }

    /// Short catalog lock (version marker, image identities). Never held
    /// across VM boot or stop; waits a bounded time, then reports "busy".
    pub fn lock(&self) -> Result<File, EngineError> {
        let state = self.state();
        super::blocking::BlockingOperation::run(move || state.lock().map_err(state_error))
    }

    /// Serialises image imports, which can take minutes, without blocking
    /// catalog readers such as `awman agents status`.
    #[cfg(test)]
    pub fn import_lock(&self) -> Result<File, EngineError> {
        self.state().import_lock().map_err(state_error)
    }

    pub fn import_lock_cancellable(
        &self,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<File, EngineError> {
        let state = self.state();
        let cancel = cancel.clone();
        super::blocking::BlockingOperation::run(move || {
            run_checked(
                || cancel.check(),
                |check| state.import_lock_cancellable(check),
            )
            .map_err(|e| match e {
                EngineError::Data(data) => state_error(data),
                e => e,
            })
        })
    }

    /// Protect the interval before the SDK publishes a sandbox's rootfs
    /// reference. Afterwards its transactional ImageInUse check owns retention.
    /// Mutations fail promptly rather than waiting for a planned launch.
    pub fn image_lease(&self, image: &str, exclusive: bool) -> Result<File, EngineError> {
        self.state()
            .image_lease(image, exclusive)
            .map_err(state_error)
    }

    #[cfg(test)]
    fn lock_named(&self, name: &str, wait: Duration) -> Result<File, EngineError> {
        self.state()
            .lock_controlled(name, wait, false, || Ok(()))
            .map_err(state_error)
    }

    /// Check (or write) the runtime-version marker; see
    /// `BuiltinState::check_runtime_marker`.
    pub fn check_runtime_marker(&self, expected: &str) -> Result<RuntimeMarker, EngineError> {
        let state = self.state();
        let expected = expected.to_owned();
        super::blocking::BlockingOperation::run(move || {
            state.check_runtime_marker(&expected).map_err(state_error)
        })
    }

    pub fn attach(&self, id: &str, owner: &str) -> PathBuf {
        self.state()
            .attach_socket(&super::naming::sandbox_name_for(&format!("{id}/{owner}")))
    }
}

impl BuiltinPaths {
    pub fn check_socket_budget(&self) -> Result<(), EngineError> {
        // SDK hashes names again: canonical uses 24 hex characters, compatibility
        // uses 32. The latter control endpoint is the longest derived socket.
        let path = self
            .home
            .join("run/agent/00000000000000000000000000000000.control.sock");
        if path.as_os_str().as_encoded_bytes().len() > SOCKET_PATH_LIMIT {
            return Err(EngineError::SocketPathTooLong {
                path,
                limit: SOCKET_PATH_LIMIT,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
fn check_socket_budget(home: &Path) -> Result<(), EngineError> {
    BuiltinPaths {
        home: home.to_path_buf(),
    }
    .check_socket_budget()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn import_lock_wait_observes_cancellation() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths = BuiltinPaths::resolve(&temp.path().join("s")).unwrap();
        let held = paths.import_lock().unwrap();
        let token = crate::engine::oci::CancelToken::new();
        let signal = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            signal.cancel();
        });
        let start = Instant::now();
        let err = paths.import_lock_cancellable(&token).unwrap_err();
        canceller.join().unwrap();
        assert!(crate::engine::oci::retry::is_cancelled(&err));
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
        assert!(paths.import_lock().is_ok());
    }

    #[test]
    fn full_socket_budget_counts_utf8_bytes() {
        assert!(check_socket_budget(Path::new("/short")).is_ok());
        assert!(check_socket_budget(Path::new(&format!("/{}", "é".repeat(35)))).is_err());
    }
    #[test]
    fn socket_budget_includes_the_longest_sdk_compatibility_endpoint() {
        let suffix = "/run/agent/00000000000000000000000000000000.control.sock";
        let root = format!("/{}", "a".repeat(SOCKET_PATH_LIMIT - suffix.len() - 1));
        assert!(check_socket_budget(Path::new(&root)).is_ok());
        assert!(check_socket_budget(Path::new(&(root + "a"))).is_err());
        let paths = BuiltinPaths {
            home: "/private".into(),
        };
        assert_ne!(
            paths.attach("same-name", "old-owner"),
            paths.attach("same-name", "new-owner")
        );
        assert_eq!(
            paths.attach("../escape", "owner").parent().unwrap(),
            Path::new("/private/attach")
        );
    }

    #[test]
    fn busy_lock_times_out_with_a_clear_error() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        let _held = paths.lock().unwrap();
        assert!(matches!(
            paths.lock_named("awman.lock", Duration::from_millis(100)),
            Err(EngineError::BuiltinRuntimeUnavailable { reason }) if reason.contains("busy")
        ));
        // The import lock is independent of the catalog lock.
        assert!(paths.import_lock().is_ok());
    }

    #[test]
    fn a_hostile_state_root_maps_to_a_config_error() {
        assert!(matches!(
            BuiltinPaths::resolve(Path::new("relative")),
            Err(EngineError::Config(_))
        ));
    }
}

#[cfg(test)]
mod hardening_tests;
