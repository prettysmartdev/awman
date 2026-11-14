//! Atomic bounded durable context storage. Callers serialize writes per key.
use super::{
    ContextFileMeta, ContextKey, ContextPathError, ContextRelPath, ContextSnapshot, StagedFile,
};
use crate::data::error::DataError;
use crate::data::fs::{
    context_dirs::{validate_context_path, ContextDirResolver},
    private_files,
    safe_tree::{SafeTree, SafeTreeError, TreeBudget},
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_files: usize,
}
impl ContextLimits {
    pub const REPO: Self = Self {
        max_file_bytes: 1 << 20,
        max_total_bytes: 16 << 20,
        max_files: 1_000,
    };
    pub const WORKFLOW: Self = Self {
        max_file_bytes: 16 << 20,
        max_total_bytes: 256 << 20,
        max_files: 100_000,
    };
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    None,
    Exists,
    Sha256(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Replaced,
}
#[derive(Debug, thiserror::Error)]
pub enum ContextStoreError {
    #[error("not found")]
    NotFound,
    #[error("the file changed: expected {expected}, found {actual:?}")]
    PreconditionFailed {
        expected: String,
        actual: Option<String>,
    },
    #[error("{0}")]
    TooLarge(String),
    #[error("{0}")]
    Conflict(String),
    #[error("listing could not complete")]
    Incomplete,
    #[error(transparent)]
    Path(#[from] ContextPathError),
    #[error(transparent)]
    Data(#[from] DataError),
}
impl From<SafeTreeError> for ContextStoreError {
    fn from(e: SafeTreeError) -> Self {
        match e {
            SafeTreeError::NotFound => Self::NotFound,
            SafeTreeError::NotADirectory => {
                Self::Conflict("context path is not a regular file or directory".into())
            }
            SafeTreeError::TooLarge { .. } => {
                Self::TooLarge("context file exceeds the size limit".into())
            }
            SafeTreeError::Incomplete => Self::Incomplete,
            SafeTreeError::Io(e) => Self::Data(DataError::io("context", e)),
        }
    }
}
pub struct RepoContextStore {
    dir: PathBuf,
    tmp_dir: PathBuf,
    limits: ContextLimits,
    awman_home: PathBuf,
}
impl RepoContextStore {
    pub fn open(
        resolver: &ContextDirResolver,
        dir: PathBuf,
        limits: ContextLimits,
    ) -> Result<Self, DataError> {
        let tmp_dir = resolver.awman_home().join("context/.tmp");
        validate_context_path(resolver.awman_home(), &dir)?;
        validate_context_path(resolver.awman_home(), &tmp_dir)?;
        Ok(Self {
            dir,
            tmp_dir,
            limits,
            awman_home: resolver.awman_home().to_path_buf(),
        })
    }
    pub fn for_key(resolver: &ContextDirResolver, key: &ContextKey) -> Result<Self, DataError> {
        Self::open(resolver, resolver.keyed_repo_dir(key), ContextLimits::REPO)
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn list(&self, cancelled: &AtomicBool) -> Result<Vec<ContextFileMeta>, ContextStoreError> {
        self.guard()?;
        let files = SafeTree::new(&self.dir).list(TreeBudget::default(), cancelled)?;
        self.check_caps(&files)?;
        Ok(files)
    }
    fn check_caps(&self, files: &[ContextFileMeta]) -> Result<(), ContextStoreError> {
        if files.len() > self.limits.max_files {
            return Err(ContextStoreError::TooLarge(
                "context file count exceeds the limit".into(),
            ));
        }
        let mut total = 0u64;
        for file in files {
            if file.size_bytes > self.limits.max_file_bytes {
                return Err(ContextStoreError::TooLarge(
                    "context file exceeds the size limit".into(),
                ));
            }
            total = total.checked_add(file.size_bytes).ok_or_else(|| {
                ContextStoreError::TooLarge("context exceeds the size limit".into())
            })?;
        }
        if total > self.limits.max_total_bytes {
            return Err(ContextStoreError::TooLarge(
                "context exceeds the total size limit".into(),
            ));
        }
        Ok(())
    }
    pub fn read(
        &self,
        path: &ContextRelPath,
    ) -> Result<(Vec<u8>, ContextFileMeta), ContextStoreError> {
        self.guard()?;
        Ok(SafeTree::new(&self.dir).read(path, self.limits.max_file_bytes)?)
    }
    fn check_match(
        &self,
        path: &ContextRelPath,
        precondition: &Precondition,
    ) -> Result<bool, ContextStoreError> {
        self.guard()?;
        if !matches!(precondition, Precondition::Sha256(_)) {
            let exists = match SafeTree::new(&self.dir).open(path) {
                Ok(_) => true,
                Err(SafeTreeError::NotFound) => false,
                Err(e) => return Err(e.into()),
            };
            if matches!(precondition, Precondition::Exists) && !exists {
                return Err(ContextStoreError::PreconditionFailed {
                    expected: "*".into(),
                    actual: None,
                });
            }
            return Ok(exists);
        }
        let current = match self.read(path) {
            Ok((_, meta)) => Some(meta),
            Err(ContextStoreError::NotFound) => None,
            Err(e) => return Err(e),
        };
        if let Precondition::Sha256(expected) = precondition {
            let actual = current.as_ref().map(|f| f.sha256.clone());
            if actual.as_deref() != Some(expected.as_str()) {
                return Err(ContextStoreError::PreconditionFailed {
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        Ok(current.is_some())
    }
    pub fn write(
        &self,
        path: &ContextRelPath,
        bytes: &[u8],
        precondition: &Precondition,
    ) -> Result<(ContextFileMeta, WriteOutcome), ContextStoreError> {
        self.guard()?;
        if !cfg!(unix) {
            return Err(DataError::Other(
                "safe context mutations are unavailable on this platform".into(),
            )
            .into());
        }
        if bytes.len() as u64 > self.limits.max_file_bytes {
            return Err(ContextStoreError::TooLarge(
                "context file exceeds the size limit".into(),
            ));
        }
        private_files::create_private_dir(&self.tmp_dir)?;
        let mut staged = tempfile::NamedTempFile::new_in(&self.tmp_dir)
            .map_err(|e| DataError::io(&self.tmp_dir, e))?;
        staged
            .write_all(bytes)
            .and_then(|_| staged.as_file().sync_all())
            .map_err(|e| DataError::io(staged.path(), e))?;
        let previous = self.check_match(path, precondition)?;
        // Check the replacement tree, allowing a PUT to shrink an oversized file.
        let mut files = SafeTree::new(&self.dir).inventory()?;
        files.retain(|f| f.path != path.as_str());
        use sha2::{Digest, Sha256};
        let meta = ContextFileMeta {
            path: path.as_str().to_owned(),
            size_bytes: bytes.len() as u64,
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            modified_at: staged
                .as_file()
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(Into::into),
        };
        files.push(meta.clone());
        self.check_caps(&files)?;
        // Context ancestors are host-owned; the mount root is re-opened no-follow
        // by replace_atomic, and each agent-controlled parent uses mkdirat/openat.
        SafeTree::new(&self.dir).ensure_root()?;
        SafeTree::new(&self.dir).replace_atomic(path, staged.path())?;
        Ok((
            meta,
            if previous {
                WriteOutcome::Replaced
            } else {
                WriteOutcome::Created
            },
        ))
    }
    pub fn delete(
        &self,
        path: &ContextRelPath,
        precondition: &Precondition,
    ) -> Result<(), ContextStoreError> {
        if !self.check_match(path, precondition)? {
            return Err(ContextStoreError::NotFound);
        }
        Ok(SafeTree::new(&self.dir).remove(path)?)
    }
    pub fn snapshot(&self, cancelled: &AtomicBool) -> Result<ContextSnapshot, ContextStoreError> {
        Ok(ContextSnapshot::from_files(self.list(cancelled)?))
    }
    pub fn stage_all(&self, files: &[StagedFile]) -> Result<(), ContextStoreError> {
        self.guard()?;
        // Revalidate public StagedFile input, including duplicate/prefix collisions.
        use base64::Engine;
        let encoded: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    base64::engine::general_purpose::STANDARD.encode(&f.bytes),
                )
            })
            .collect();
        super::staging::validate_staged_files(
            encoded.iter().map(|(p, b)| (*p, b.as_str())),
            self.limits,
        )
        .map_err(|e| ContextStoreError::Conflict(e.to_string()))?;
        let parent = self
            .dir
            .parent()
            .ok_or_else(|| DataError::Other("context directory has no parent".into()))?;
        SafeTree::new(parent).ensure_root()?;
        private_files::create_private_dir_new(&self.dir)?;
        let result = files.iter().try_for_each(|f| {
            self.write(&f.path, &f.bytes, &Precondition::None)
                .map(|_| ())
        });
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        result
    }
    pub fn remove_all(self) -> Result<(), DataError> {
        self.guard()?;
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(&self.dir, e)),
        }
    }
    fn guard(&self) -> Result<(), DataError> {
        validate_context_path(&self.awman_home, &self.dir)?;
        validate_context_path(&self.awman_home, &self.tmp_dir)
    }
}
