//! Typed access to the container failure-log directory.
//!
//! Layer 0: resolves host-side log paths under `~/.awman/logs/` and performs
//! the file writes. Higher layers hand this type the buffered container output
//! and a `(workflow, step, container)` identity; they never touch `std::fs`
//! themselves.

use std::path::PathBuf;

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::global::GlobalConfig;
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
        let awman_home = GlobalConfig::data_home_with(env)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> uuid::Uuid {
        uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
    }

    #[test]
    fn logs_dir_is_under_awman_home() {
        let paths = WorkflowLogPaths::at_home("/home/user/.awman");
        assert_eq!(paths.logs_dir(), PathBuf::from("/home/user/.awman/logs"));
    }

    #[test]
    fn container_log_path_uses_workflow_step_container_template() {
        let paths = WorkflowLogPaths::at_home("/home/user/.awman");
        let path = paths.container_log_path(uuid(), "build", "awman-abc123");
        assert_eq!(
            path,
            PathBuf::from(
                "/home/user/.awman/logs/550e8400-e29b-41d4-a716-446655440000-build-awman-abc123.log"
            )
        );
    }

    #[test]
    fn sanitise_replaces_path_separators_and_spaces() {
        assert_eq!(sanitise("deploy/prod step"), "deploy-prod-step");
        assert_eq!(sanitise("../escape"), "..-escape");
    }

    #[test]
    fn sanitise_empty_becomes_unnamed() {
        assert_eq!(sanitise(""), "unnamed");
    }

    #[test]
    fn crafted_step_name_cannot_escape_logs_dir() {
        let paths = WorkflowLogPaths::at_home("/home/user/.awman");
        let path = paths.container_log_path(uuid(), "../../etc/passwd", "c");
        // No path traversal survives sanitisation: the file stays directly
        // under the logs directory.
        assert_eq!(path.parent().unwrap(), paths.logs_dir());
    }

    #[test]
    fn write_container_log_creates_dir_and_writes_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = WorkflowLogPaths::at_home(tmp.path().join(".awman"));
        let written = paths
            .write_container_log(uuid(), "build", "awman-xyz", "line 1\nline 2\n")
            .unwrap();
        assert!(written.exists(), "log file must be created");
        let body = std::fs::read_to_string(&written).unwrap();
        assert_eq!(body, "line 1\nline 2\n");
        assert!(written.starts_with(paths.logs_dir()));
    }
}
