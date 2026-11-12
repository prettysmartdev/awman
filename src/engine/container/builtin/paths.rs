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
