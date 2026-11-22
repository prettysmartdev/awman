//! Private metadata store and canonical directory boundary for launch contexts.
use super::{data_paths::data_home, path_guard::canonicalize_lenient, private_files::*};
use crate::data::{
    config::env::EnvSnapshot, error::DataError, launch_context::LaunchContextRecord,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchContextFile {
    pub version: u32,
    pub contexts: Vec<LaunchContextRecord>,
}
impl Default for LaunchContextFile {
    fn default() -> Self {
        Self {
            version: 1,
            contexts: vec![],
        }
    }
}
impl LaunchContextFile {
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let file = read_private_json::<Self>(path)?.unwrap_or_default();
        if file.version != 1 {
            return Err(DataError::InvalidPath {
                path: path.into(),
                reason: "unsupported launch-context store version".into(),
            });
        }
        let mut ids = std::collections::HashSet::new();
        let mut labels = std::collections::HashSet::new();
        for c in &file.contexts {
            if !c.root.is_absolute()
                || c.label.trim().is_empty()
                || !ids.insert(&c.id)
                || !labels.insert(&c.label)
            {
                return Err(DataError::InvalidPath {
                    path: path.into(),
                    reason: "invalid launch-context metadata".into(),
                });
            }
        }
        Ok(file)
    }
    pub fn store(&self, path: &Path) -> Result<(), DataError> {
        if let Some(p) = path.parent() {
            create_private_dir(p)?;
        }
        write_private_json_atomic(path, self, || Ok(()))
    }
}
pub fn canonical_directory(path: &Path) -> Result<PathBuf, DataError> {
    let canonical = std::fs::canonicalize(path).map_err(|e| DataError::io(path, e))?;
    if !canonical.is_dir() {
        return Err(DataError::InvalidPath {
            path: path.into(),
            reason: "expected a directory".into(),
        });
    }
    Ok(canonical)
}
/// Same bidirectional overlap rule as settings mounts: secrets and ancestors
/// are never valid roots. Resolve even missing protected leaves through symlinks.
pub fn protected_root(root: &Path, env: &EnvSnapshot) -> bool {
    let mut protected = vec![];
    if let Some(home) = env.home_dir() {
        protected.extend(
            crate::data::config::agent_definition::HOST_DIR_DENY_LIST
                .iter()
                .map(|dir| home.join(dir)),
        );
    }
    if let Ok(data) = data_home(env) {
        protected.push(data);
    }
    if let Ok(config) = super::data_paths::config_home(env) {
        protected.push(config);
    }
    protected
        .iter()
        .map(|p| canonicalize_lenient(p))
        .any(|p| root.starts_with(&p) || p.starts_with(root))
}
