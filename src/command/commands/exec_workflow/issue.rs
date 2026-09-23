//! Issue- and work-item-sourced prompts: finding the work item file, and the
//! temp file an issue overlay is mounted from.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::*;

/// Extract a numeric work item number from strings like "0069", "69", "WI-69",
/// etc. Returns the first run of decimal digits found in `s`, parsed as `u32`.
pub(crate) fn parse_work_item_number(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Find a work item file whose filename starts with the zero-padded four-digit
/// number (e.g. `0069-*.md`). The search directory is determined by the repo
/// config's `workItems.dir` setting; falls back to `<git_root>/aspec/work-items/`.
pub(crate) fn find_work_item_file(
    session: &Session,
    git_root: &std::path::Path,
    number: u32,
) -> Option<std::path::PathBuf> {
    // The session's own repo config, not a fresh load (F-31): the session
    // already merged it, and a second read here could answer from a file the
    // command was not built against.
    let dir = session
        .effective_config()
        .repo()
        .work_items_dir_or_default(git_root);
    let prefix = format!("{:04}-", number);
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
}

/// Build an [`OverlaySpec`] that mounts the main repo's `.git` directory into
/// a container so git operations work inside a worktree checkout.
///
/// A worktree's `.git` is a pointer file referencing an absolute path inside
/// the main repo's `.git/worktrees/<name>/` directory. When only the worktree
/// is bind-mounted, that pointer dangles and every git command fails. This
/// overlay mounts the main `.git` directory at its host-absolute path so the
/// pointer resolves identically inside the container.
///
/// Returns `Ok(None)` when `worktree_path` is a regular repo or has no `.git`.
pub(crate) fn worktree_git_overlay(
    worktree_path: &std::path::Path,
) -> Result<Option<crate::engine::container::options::OverlaySpec>, EngineError> {
    let main_git_dir = match crate::engine::git::resolve_worktree_git_dir(worktree_path)? {
        Some(p) => p,
        None => return Ok(None),
    };
    Ok(Some(crate::engine::container::options::OverlaySpec {
        host_path: main_git_dir.clone(),
        container_path: main_git_dir,
        permission: crate::engine::container::options::OverlayPermission::ReadWrite,
    }))
}

/// Guards an on-disk temp file: deleted when this value is dropped, regardless
/// of how the surrounding scope exits (success, `?`, panic). Used for the
/// issue overlay temp file so cleanup survives every early-return path.
pub(crate) struct IssueTempFile {
    pub(crate) path: PathBuf,
}

impl IssueTempFile {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for IssueTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Result of `issue_source_overlay`: everything the caller needs to inject
/// an issue-derived file into the workflow's containers, plus a Drop guard
/// for the underlying temp file.
pub(crate) struct IssueOverlayBuild {
    pub temp_file: IssueTempFile,
    pub overlay: TypedOverlay,
    pub slug: String,
    pub number: u32,
    pub content: String,
}

/// Build the workflow overlay for an `Issue` produced by an `IssueSource`.
///
/// Writes the rendered markdown to a unique temp file (returned wrapped in
/// `IssueTempFile` so the caller can keep it alive for the duration of the
/// workflow) and constructs a read-only `TypedOverlay::Directory` mapping the
/// temp file to `/workspace/<work_items_relative>/NNNN-<slug>.md` inside the
/// container.
///
/// Signature takes `&dyn IssueSource` and `&Issue` — no concrete provider types.
pub(crate) fn issue_source_overlay(
    source: &dyn crate::engine::issue::IssueSource,
    issue: &crate::engine::issue::Issue,
    git_root: &std::path::Path,
    work_items_dir: &std::path::Path,
) -> std::io::Result<IssueOverlayBuild> {
    let slug = source.title_slug(issue);
    let content = source.format_as_markdown(issue);
    let number = issue.numeric_id().unwrap_or(0);

    let pid = std::process::id();
    let temp_filename = format!("awman-issue-{pid}-{slug}.md");
    let temp_path = std::env::temp_dir().join(&temp_filename);
    std::fs::write(&temp_path, &content)?;
    let temp_file = IssueTempFile {
        path: temp_path.clone(),
    };

    let relative = work_items_dir
        .strip_prefix(git_root)
        .unwrap_or_else(|_| std::path::Path::new("aspec/work-items"));
    let container_filename = format!("{number:04}-{slug}.md");
    let container_path = std::path::PathBuf::from("/workspace")
        .join(relative)
        .join(&container_filename);

    let overlay = TypedOverlay::Directory(crate::engine::overlay::DirectorySpec {
        host: temp_path.display().to_string(),
        container: container_path.display().to_string(),
        permission: crate::engine::container::options::OverlayPermission::ReadOnly,
    });

    Ok(IssueOverlayBuild {
        temp_file,
        overlay,
        slug,
        number,
        content,
    })
}
