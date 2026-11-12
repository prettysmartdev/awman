//! Typed access to the container failure-log directory.
//!
//! Layer 0: resolves host-side log paths under `~/.awman/logs/` and performs
//! the file writes. Higher layers hand this type the buffered container output
//! and a `(workflow, step, container)` identity; they never touch `std::fs`
//! themselves.

use std::path::PathBuf;

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;

/// Resolves and writes per-container workflow log files under `~/.awman/logs/`.
#[derive(Debug, Clone)]
pub struct WorkflowLogPaths {
    awman_home: PathBuf,
}

impl WorkflowLogPaths {
    /// Construct from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Construct from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        let awman_home = crate::data::fs::data_paths::data_home(env)?;
        Ok(Self { awman_home })
    }

    /// Construct with an explicit awman home (for testing).
    pub fn at_home(awman_home: impl Into<PathBuf>) -> Self {
        Self {
            awman_home: awman_home.into(),
        }
    }

    /// `~/.awman/logs/`
    pub fn logs_dir(&self) -> PathBuf {
        self.awman_home.join("logs")
    }

    /// `~/.awman/logs/{workflow-id}-{step-name}-{container-name}.log`
    ///
    /// The workflow id is a UUID (already filesystem-safe); step and container
    /// names are sanitised so an exotic step name can never escape the logs
    /// directory or produce an invalid filename.
    pub fn container_log_path(
        &self,
        workflow_id: uuid::Uuid,
        step_name: &str,
        container_name: &str,
    ) -> PathBuf {
        let filename = format!(
            "{}-{}-{}.log",
            workflow_id,
            sanitise(step_name),
            sanitise(container_name),
        );
        self.logs_dir().join(filename)
    }

    /// Write `contents` to the per-container log file, creating the logs
    /// directory if necessary. Returns the path written so callers can point
    /// the user at it.
    ///
    /// Left as a direct write: the file name carries a fresh workflow id and
    /// the content is written once and never re-read by awman, so there is no
    /// earlier record to protect and no reader that needs it complete.
    pub fn write_container_log(
        &self,
        workflow_id: uuid::Uuid,
        step_name: &str,
        container_name: &str,
        contents: &str,
    ) -> Result<PathBuf, DataError> {
        let dir = self.logs_dir();
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        let path = self.container_log_path(workflow_id, step_name, container_name);
        std::fs::write(&path, contents).map_err(|e| DataError::io(&path, e))?;
        Ok(path)
    }
}

/// Normalise a filename component: keep ASCII alphanumerics plus `.`, `_`, `-`;
/// replace everything else with `-`. Empty input becomes `unnamed` so the
/// filename never collapses to just its separators.
fn sanitise(component: &str) -> String {
    let cleaned: String = component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}
