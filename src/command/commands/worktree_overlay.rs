//! Shared validated mount for linked-worktree Git metadata.

/// Build an [`OverlaySpec`](crate::engine::container::options::OverlaySpec)
/// that mounts the main repo's `.git` directory into
/// a container so git operations work inside a worktree checkout.
///
/// A worktree's `.git` is a pointer file referencing an absolute path inside
/// the main repo's `.git/worktrees/<name>/` directory. When only the worktree
/// is bind-mounted, that pointer dangles and every git command fails. This
/// overlay mounts the main `.git` directory at its host-absolute path so the
/// pointer resolves identically inside the container.
///
/// Both the pointer and its administrative backlink must belong to
/// `git_root`; a tampered pointer never chooses a host mount source.
pub(crate) fn worktree_git_overlay(
    git: &crate::engine::git::GitEngine,
    git_root: &std::path::Path,
    worktree_path: &std::path::Path,
) -> Result<Option<crate::engine::container::options::OverlaySpec>, crate::engine::error::EngineError>
{
    let main_git_dir = git.validate_worktree_owner(git_root, worktree_path)?;
    Ok(Some(crate::engine::container::options::OverlaySpec {
        host_path: main_git_dir.clone(),
        container_path: main_git_dir,
        permission: crate::engine::container::options::OverlayPermission::ReadWrite,
    }))
}
