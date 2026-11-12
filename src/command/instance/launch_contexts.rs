//! Owner-authorised roots with process environments kept only in daemon memory.
use crate::data::{
    config::env::EnvSnapshot,
    fs::launch_context_store::{canonical_directory, protected_root, LaunchContextFile},
    ids::LaunchContextId,
    launch_context::*,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ContextError {
    pub code: &'static str,
    pub message: String,
}
fn err(code: &'static str, message: impl Into<String>) -> ContextError {
    ContextError {
        code,
        message: message.into(),
    }
}
#[derive(Debug)]
pub struct LaunchContextRegistry {
    file: Option<PathBuf>,
    metadata: LaunchContextFile,
    env: HashMap<LaunchContextId, EnvSnapshot>,
    fallback: EnvSnapshot,
    load_error: Option<String>,
}
impl Default for LaunchContextRegistry {
    fn default() -> Self {
        Self {
            file: None,
            metadata: Default::default(),
            env: HashMap::new(),
            fallback: EnvSnapshot::empty(),
            load_error: None,
        }
    }
}
impl LaunchContextRegistry {
    pub fn open(path: PathBuf, fallback: EnvSnapshot) -> Self {
        let loaded = LaunchContextFile::load(&path);
        Self {
            file: Some(path),
            fallback,
            load_error: loaded.as_ref().err().map(ToString::to_string),
            metadata: loaded.unwrap_or_default(),
            env: HashMap::new(),
        }
    }
    fn check(&self) -> Result<(), ContextError> {
        match &self.load_error {
            Some(e) => Err(err("context_store_unavailable", e.clone())),
            None => Ok(()),
        }
    }
    fn save(&self, metadata: &LaunchContextFile) -> Result<(), ContextError> {
        self.check()?;
        if let Some(path) = &self.file {
            metadata.store(path).map_err(|_| {
                err(
                    "context_store_unavailable",
                    "could not save launch contexts",
                )
            })?;
        }
        Ok(())
    }
    pub fn list(&self) -> Result<LaunchContextList, ContextError> {
        self.check()?;
        Ok(LaunchContextList {
            items: self
                .metadata
                .contexts
                .iter()
                .map(|c| LaunchContextSummary {
                    record: c.clone(),
                    env_state: if self.env.contains_key(&c.id) {
                        "live"
                    } else {
                        "stale"
                    },
                })
                .collect(),
            default_id: self
                .metadata
                .contexts
                .iter()
                .find(|c| c.is_default)
                .or_else(|| self.metadata.contexts.iter().max_by_key(|c| c.refreshed_at))
                .map(|c| c.id.clone()),
            next_cursor: None,
        })
    }
    pub fn create(
        &mut self,
        input: CreateLaunchContext,
    ) -> Result<LaunchContextRecord, ContextError> {
        self.check()?;
        let root = canonical_directory(&input.root)
            .map_err(|_| err("invalid_request", "root must be an existing directory"))?;
        if protected_root(&root, &self.fallback) {
            return Err(err(
                "forbidden_workdir",
                "root overlaps a protected directory",
            ));
        }
        let label = input.label.unwrap_or_else(|| {
            root.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        if label.trim().is_empty() || label.len() > 128 || label.chars().any(char::is_control) {
            return Err(err("invalid_request", "invalid launch context label"));
        }
        if self.metadata.contexts.iter().any(|c| c.label == label) {
            return Err(err("conflict", "launch context label already exists"));
        }
        let now = chrono::Utc::now();
        let record = LaunchContextRecord {
            id: LaunchContextId::new(),
            root: root.clone(),
            label,
            created_at: now,
            refreshed_at: now,
            is_default: input.default,
        };
        let mut metadata = self.metadata.clone();
        if input.default {
            for c in &mut metadata.contexts {
                c.is_default = false;
            }
        }
        metadata.contexts.push(record.clone());
        self.save(&metadata)?;
        self.metadata = metadata;
        self.env.insert(
            record.id.clone(),
            EnvSnapshot::from_instance_env(
                input.env.into_inner().into_iter().collect(),
                root,
                vec!["awman".into()],
            ),
        );
        Ok(record)
    }
    pub fn refresh(
        &mut self,
        id: &LaunchContextId,
        input: RefreshLaunchContext,
    ) -> Result<(), ContextError> {
        self.check()?;
        let mut metadata = self.metadata.clone();
        let c = metadata
            .contexts
            .iter_mut()
            .find(|c| &c.id == id)
            .ok_or_else(|| err("not_found", "launch context not found"))?;
        if canonical_directory(&input.root).ok().as_ref() != Some(&c.root) {
            return Err(err("invalid_request", "run refresh from the context root"));
        }
        c.refreshed_at = chrono::Utc::now();
        let root = c.root.clone();
        self.save(&metadata)?;
        self.metadata = metadata;
        self.env.insert(
            id.clone(),
            EnvSnapshot::from_instance_env(
                input.env.into_inner().into_iter().collect(),
                root,
                vec!["awman".into()],
            ),
        );
        Ok(())
    }
    pub fn delete(&mut self, id: &LaunchContextId) -> Result<(), ContextError> {
        self.check()?;
        let mut metadata = self.metadata.clone();
        let before = metadata.contexts.len();
        metadata.contexts.retain(|c| &c.id != id);
        if before == metadata.contexts.len() {
            return Err(err("not_found", "launch context not found"));
        }
        self.save(&metadata)?;
        self.metadata = metadata;
        self.env.remove(id);
        Ok(())
    }
    pub fn resolve_env(&self, id: &LaunchContextId) -> Result<(EnvSnapshot, bool), ContextError> {
        self.check()?;
        let c = self
            .metadata
            .contexts
            .iter()
            .find(|c| &c.id == id)
            .ok_or_else(|| err("not_found", "launch context not found"))?;
        // Revalidate the root at use: replacing the directory with a symlink must
        // not change the authorised mount boundary.
        if canonical_directory(&c.root).ok().as_ref() != Some(&c.root)
            || protected_root(&c.root, &self.fallback)
        {
            return Err(err("dir_outside_context", "context root changed"));
        }
        match self.env.get(id) {
            Some(e) => Ok((e.clone(), false)),
            None => Ok((
                self.fallback
                    .clone()
                    .with_invocation(c.root.clone(), vec!["awman".into()]),
                true,
            )),
        }
    }
    pub fn roots(&self) -> Vec<PathBuf> {
        self.metadata
            .contexts
            .iter()
            .map(|c| c.root.clone())
            .collect()
    }
    pub fn resolve_dir(
        roots: &[PathBuf],
        base: &Path,
        dir: &Path,
    ) -> Result<PathBuf, ContextError> {
        let path = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            base.join(dir)
        };
        let canonical = canonical_directory(&path).map_err(|_| {
            err(
                "dir_outside_context",
                "directory is outside the launch contexts",
            )
        })?;
        if !roots.iter().any(|root| canonical.starts_with(root)) {
            return Err(err(
                "dir_outside_context",
                "directory is outside the launch contexts",
            ));
        }
        Ok(canonical)
    }
}
