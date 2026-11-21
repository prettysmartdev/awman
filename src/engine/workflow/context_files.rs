//! Owned by workflow-scratch-push: bounded read-only workflow file access.
use crate::data::{
    context::{ContextFileMeta, ContextLimits, ContextPathError, ContextRelPath},
    error::DataError,
    fs::{
        context_dirs::{validate_context_path, ContextDirResolver},
        safe_tree::{SafeTree, SafeTreeError, TreeBudget},
    },
};
use std::sync::atomic::AtomicBool;
pub struct WorkflowContextReader {
    tree: SafeTree,
}
#[derive(Debug, thiserror::Error)]
pub enum WorkflowContextError {
    #[error("not found")]
    NotFound,
    #[error("file is {size_bytes} bytes; the largest served is {limit_bytes}")]
    TooLarge { size_bytes: u64, limit_bytes: u64 },
    #[error("listing could not complete")]
    Incomplete,
    #[error(transparent)]
    Path(#[from] ContextPathError),
    #[error(transparent)]
    Data(#[from] DataError),
}
impl WorkflowContextReader {
    pub fn for_invocation(
        resolver: &ContextDirResolver,
        invocation: uuid::Uuid,
    ) -> Result<Self, DataError> {
        let dir = resolver.workflow_dir(invocation);
        validate_context_path(resolver.awman_home(), &dir)?;
        Ok(Self {
            tree: SafeTree::new(dir),
        })
    }
    pub fn list(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ContextFileMeta>, WorkflowContextError> {
        // Hashing and traversal have independent bounds; never return a partial list.
        self.tree
            .list(
                TreeBudget {
                    max_entries: ContextLimits::WORKFLOW.max_files,
                    max_hash_bytes: ContextLimits::WORKFLOW.max_total_bytes,
                    ..TreeBudget::default()
                },
                cancelled,
            )
            .map_err(map_tree_error)
    }
    pub fn list_for_collection(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<crate::data::fs::safe_tree::TreeListing, WorkflowContextError> {
        self.tree
            .list_report(
                TreeBudget {
                    max_entries: ContextLimits::WORKFLOW.max_files,
                    max_hash_bytes: ContextLimits::WORKFLOW.max_total_bytes,
                    deadline: std::time::Duration::from_secs(30),
                },
                cancelled,
            )
            .map_err(map_tree_error)
    }
    pub fn read(&self, path: &str) -> Result<(Vec<u8>, ContextFileMeta), WorkflowContextError> {
        let path = ContextRelPath::parse(path)?;
        // Reopen on every download. SafeTree checks every component no-follow,
        // verifies the opened inode is regular and bounds bytes read through it.
        self.tree
            .read(&path, ContextLimits::WORKFLOW.max_file_bytes)
            .map_err(map_tree_error)
    }
}

fn map_tree_error(error: SafeTreeError) -> WorkflowContextError {
    match error {
        SafeTreeError::NotFound | SafeTreeError::NotADirectory => WorkflowContextError::NotFound,
        SafeTreeError::TooLarge { size_bytes } => WorkflowContextError::TooLarge {
            size_bytes,
            limit_bytes: ContextLimits::WORKFLOW.max_file_bytes,
        },
        SafeTreeError::Incomplete | SafeTreeError::Io(_) => WorkflowContextError::Incomplete,
    }
}
