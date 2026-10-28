//! Durable inputs for finishing cancelled teardown after a daemon restart.
//! Stored outside the agent mount; overlays remain specifications, never
//! resolved credential contents or ephemeral credential-file paths.

use crate::data::config::overlays::{OverlayPermission, TypedOverlay};
use crate::data::workflow_definition::Workflow;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowCleanup {
    pub workflow: Workflow,
    pub work_item: Option<(u32, String)>,
    pub workdir: PathBuf,
    pub mount_path: PathBuf,
    pub state_root: Option<PathBuf>,
    pub image: String,
    pub cli_overlays: Vec<TypedOverlay>,
    pub worktree_git_mount: Option<(PathBuf, PathBuf, OverlayPermission)>,
}
