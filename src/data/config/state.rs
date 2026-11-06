//! Per-repo state that is not configuration:
//! `<AWMAN_HOME>/data/repos/<repo-hash>/state.json`.
//!
//! Deleting the file clears the state. The hash is stable per git root.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::DataPaths;

/// File name of the per-repo state document.
pub const REPO_STATE_FILENAME: &str = "state.json";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoState {
    /// Whether the user accepted automatic agent authentication for this repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_agent_auth_accepted: Option<bool>,
}

impl RepoState {
    /// `<data>/repos/<hash>/state.json` for `git_root`.
    pub fn path(data: &DataPaths, git_root: &Path) -> PathBuf {
        data.repo_state_dir(git_root).join(REPO_STATE_FILENAME)
    }

    /// Load the state, or the default when the file is absent.
    pub fn load(data: &DataPaths, git_root: &Path) -> Result<Self, DataError> {
        let path = Self::path(data, git_root);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| DataError::config_parse(&path, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(DataError::io(&path, e)),
        }
    }

    /// Write the state, creating its directory.
    pub fn save(&self, data: &DataPaths, git_root: &Path) -> Result<(), DataError> {
        let path = Self::path(data, git_root);
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        crate::data::fs::config_file::write_user_config_file(&path, &content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_is_default_and_save_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::at_root(tmp.path().join("data"));
        let repo = Path::new("/some/repo");
        assert_eq!(RepoState::load(&data, repo).unwrap(), RepoState::default());
        let state = RepoState {
            auto_agent_auth_accepted: Some(true),
        };
        state.save(&data, repo).unwrap();
        let path = RepoState::path(&data, repo);
        assert!(path.starts_with(tmp.path().join("data").join("repos")));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("autoAgentAuthAccepted"));
        assert_eq!(RepoState::load(&data, repo).unwrap(), state);
        // Stable per git root, distinct across roots.
        assert_eq!(path, RepoState::path(&data, repo));
        assert_ne!(path, RepoState::path(&data, Path::new("/other")));
    }
}
