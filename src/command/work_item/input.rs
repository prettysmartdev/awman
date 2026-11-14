//! Read-only execution input for a resolved work item.
//!
//! Local files are used where they are: the container sees them at
//! `/workspace/<repo-relative path>` and no extra mount is added. Remote and
//! inline content is staged in a private owner-only file (unpredictable
//! 0700 directory, exclusive 0600 file) and mounted read-only at
//! `/awman/work-items/<key>.md`. The caller keeps `private_file` alive for
//! the whole run (leader, every step, teardown, cancellation); dropping it
//! deletes the staged file.

use std::path::{Path, PathBuf};

use crate::data::config::env::EnvSnapshot;
use crate::data::config::overlays::{DirectorySpec, OverlayPermission, TypedOverlay};
use crate::data::error::DataError;
use crate::data::fs::work_item_draft::PrivateWorkItemFile;
use crate::data::issue::display_url;
use crate::data::session::Session;
use crate::data::workflow_prompt_template::WorkItemContext;

use super::resolver::{
    is_valid_work_item_key, repo_relative_display, ResolvedWorkItem, WorkItemError, WorkItemOrigin,
};

/// Container directory remote and inline work items are mounted under.
pub const CONTAINER_WORK_ITEMS_DIR: &str = "/awman/work-items";

pub struct PreparedWorkItemInput {
    pub context: WorkItemContext,
    /// Where the agent reads the work item inside the container.
    pub container_path: PathBuf,
    /// Read-only mount for remote/inline content; `None` for local files.
    pub overlay: Option<TypedOverlay>,
    /// Guard for the staged file; keep it alive until the run ends.
    pub private_file: Option<PrivateWorkItemFile>,
}

impl std::fmt::Debug for PreparedWorkItemInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedWorkItemInput")
            .field("container_path", &self.container_path)
            .field("overlay", &self.overlay)
            .field("private_file", &self.private_file)
            .finish_non_exhaustive()
    }
}

/// Build the prompt context and (for remote/inline items) the private file
/// and read-only overlay. Call with the invoking session, before worktree
/// re-rooting.
pub fn prepare_work_item_input(
    item: &ResolvedWorkItem,
    session: &Session,
) -> Result<PreparedWorkItemInput, WorkItemError> {
    if !is_valid_work_item_key(&item.key) {
        return Err(DataError::Other(format!("invalid work item key {:?}", item.key)).into());
    }
    let git_root = session.git_root();
    let context = WorkItemContext {
        number: item.number,
        key: item.key.clone(),
        title: item.title.clone(),
        // Persisted in workflow state inside the checkout and shown to the
        // agent: a remote URL never carries its query, fragment or userinfo.
        // The key still tells two URLs apart.
        source_id: match &item.origin {
            WorkItemOrigin::Remote { .. } => display_url(&item.source_id(git_root)),
            WorkItemOrigin::Local { .. } => item.source_id(git_root),
        },
        content_hash: crate::data::fs::hash::sha256_hex(item.content.as_bytes()),
        content: item.content.clone(),
    };
    match &item.origin {
        WorkItemOrigin::Local { path } => Ok(PreparedWorkItemInput {
            container_path: Path::new("/workspace").join(repo_relative_display(git_root, path)),
            context,
            overlay: None,
            private_file: None,
        }),
        WorkItemOrigin::Remote { .. } => {
            let parent = private_work_item_parent(session.env())?;
            let private_file = PrivateWorkItemFile::create(&parent, &item.content)?;
            let container_path =
                Path::new(CONTAINER_WORK_ITEMS_DIR).join(format!("{}.md", item.key));
            let overlay = TypedOverlay::Directory(DirectorySpec {
                host: private_file.path().display().to_string(),
                container: container_path.display().to_string(),
                permission: OverlayPermission::ReadOnly,
            });
            Ok(PreparedWorkItemInput {
                context,
                container_path,
                overlay: Some(overlay),
                private_file: Some(private_file),
            })
        }
    }
}

/// The owner-only parent for staged work item files:
/// `<awman data home>/tmp/work-items`. Each staged file gets its own
/// unpredictable subdirectory below it.
pub fn private_work_item_parent(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    Ok(crate::data::fs::data_paths::data_home(env)?
        .join("tmp")
        .join("work-items"))
}
