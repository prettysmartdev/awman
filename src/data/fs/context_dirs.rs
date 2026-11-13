//! Typed access to context overlay directories.
//!
//! Layer 0: resolves host-side context directory paths and ensures they exist.
//! All paths live under `~/.awman/context/` — see WI-0087 Security
//! Reconciliation for the rationale.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;
use crate::data::fs::path_guard::validate_under_root;
use crate::data::fs::remote_slug::RemoteSlug;

/// Resolves host-side context directory paths.
#[derive(Debug, Clone)]
pub struct ContextDirResolver {
    awman_home: PathBuf,
}

impl ContextDirResolver {
    /// Construct from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Construct from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        let awman_home = normalize_home(crate::data::fs::data_paths::data_home(env)?);
        Ok(Self { awman_home })
    }

    /// Construct with an explicit awman home (for testing).
    pub fn at_home(awman_home: impl Into<PathBuf>) -> Self {
        Self {
            awman_home: normalize_home(awman_home.into()),
        }
    }

    /// The resolved awman home directory (`~/.awman` unless overridden).
    /// Callers need this to invoke [`validate_context_path`] against a
    /// resolved context path.
    pub fn awman_home(&self) -> &Path {
        &self.awman_home
    }

    /// `~/.awman/context/global/`
    pub fn global_dir(&self) -> PathBuf {
        self.awman_home.join("context").join("global")
    }

    /// `~/.awman/context/repo/{owner}/{repo}/`
    ///
    /// `remote_url` is the repository's `origin` URL, supplied by the caller —
    /// Layer 1's `GitEngine` reads it (WI 0114 F-29; a Layer 0 path resolver
    /// must not shell out to git). Pass `None` when there is no remote, or
    /// when reading it failed: the slug then falls back to `_local/{dirname}`,
    /// exactly as a failed `git remote get-url` did before. Always normalised
    /// to lowercase with non-alphanumeric chars replaced by dashes.
    pub fn repo_dir(&self, remote_url: Option<&str>, git_root: &Path) -> PathBuf {
        let slug = repo_slug(remote_url, git_root);
        self.awman_home.join("context").join("repo").join(slug)
    }

    /// `~/.awman/context/workflows/{invocation_uuid}/`
    pub fn workflow_dir(&self, invocation_uuid: uuid::Uuid) -> PathBuf {
        self.awman_home
            .join("context")
            .join("workflows")
            .join(invocation_uuid.to_string())
    }

    /// Create the directory if it does not exist. Idempotent.
    pub fn ensure_exists(path: &Path) -> Result<(), DataError> {
        std::fs::create_dir_all(path).map_err(|e| DataError::io(path, e))
    }
}

/// Derive the `{owner}/{repo}` slug from `remote_url`, falling back to
/// `_local/{dirname}` when there is none (or it names no owner/repo pair).
fn repo_slug(remote_url: Option<&str>, git_root: &Path) -> String {
    if let Some(slug) = remote_url.and_then(parse_owner_repo) {
        return slug;
    }
    let dirname = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    format!("_local/{}", normalise_slug(dirname))
}

/// Extract and normalise `owner/repo` from a git remote URL.
pub fn parse_owner_repo(remote_url: &str) -> Option<String> {
    RemoteSlug::parse(remote_url).map(|slug| slug.normalised_path())
}

/// Normalise a slug component: lowercase, non-alphanumeric chars replaced by dashes.
pub fn normalise_slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Validate that a resolved context path stays under `~/.awman/context/`.
/// Returns `Err` if the path escapes (e.g. via `..` in a crafted slug).
pub fn validate_context_path(awman_home: &Path, resolved: &Path) -> Result<(), DataError> {
    let root = awman_home.join("context");
    let relative = resolved
        .strip_prefix(&root)
        .map_err(|_| DataError::InvalidPath {
            path: resolved.into(),
            reason: "context directory must reside under the context root".into(),
        })?;
    let mut path = root.clone();
    crate::data::fs::path_guard::ensure_not_symlinked(&path)?;
    for part in relative.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(DataError::InvalidPath {
                path: resolved.into(),
                reason: "context directory must not contain traversal".into(),
            });
        }
        path.push(part);
        crate::data::fs::path_guard::ensure_not_symlinked(&path)?;
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if !meta.is_dir() {
                return Err(DataError::InvalidPath {
                    path: resolved.into(),
                    reason: "context path is not a directory".into(),
                });
            }
        }
    }
    validate_under_root(
        &root,
        resolved,
        "context directory must reside under ~/.awman/context/",
    )
}

impl ContextDirResolver {
    pub fn keyed_repo_dir(&self, key: &crate::data::context::ContextKey) -> PathBuf {
        self.awman_home
            .join("context/repo/_keyed")
            .join(key.dir_name())
    }
    pub fn keyed_tmp_dir(&self) -> PathBuf {
        self.awman_home.join("context/repo/_keyed/.tmp")
    }
    pub fn lease_repo_root(&self) -> PathBuf {
        self.awman_home.join("context/repo/_lease")
    }
    pub fn lease_repo_dir(&self, lease: &crate::data::ids::LeaseId) -> PathBuf {
        self.lease_repo_root().join(lease.to_string())
    }
    /// Hold for the entire workflow execution, including resumed foreground runs.
    pub fn workflow_lease(&self, invocation: uuid::Uuid) -> Result<std::fs::File, DataError> {
        let path = self.workflow_lock_path(invocation)?;
        crate::data::fs::private_files::lock_private(
            &path,
            crate::data::fs::private_files::LockMode::Shared,
            || Ok(()),
        )
    }
    fn workflow_lock_path(&self, invocation: uuid::Uuid) -> Result<PathBuf, DataError> {
        let dir = self.awman_home.join("context/.workflow-locks");
        validate_context_path(self.awman_home(), &dir)?;
        crate::data::fs::private_files::create_private_dir(&dir)?;
        Ok(dir.join(invocation.to_string()))
    }
    pub fn remove_workflow_dir(&self, invocation: uuid::Uuid) -> Result<(), DataError> {
        let lock_path = self.workflow_lock_path(invocation)?;
        let _lease = crate::data::fs::private_files::lock_private_timeout(
            &lock_path,
            crate::data::fs::private_files::LockMode::Exclusive,
            std::time::Duration::ZERO,
            || Ok(()),
        )?
        .ok_or_else(|| DataError::Other("workflow scratch is still in use".into()))?;
        let path = self.workflow_dir(invocation);
        validate_context_path(self.awman_home(), &path)?;
        match std::fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(path, e)),
        }
    }
}

// Resolve only the trusted home, never an agent-controlled context component.
// Canonicalize the existing prefix too when a first launch has no home yet.
fn normalize_home(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if parent != path {
            return normalize_home(parent.to_path_buf()).join(name);
        }
    }
    std::path::absolute(&path).unwrap_or(path)
}
