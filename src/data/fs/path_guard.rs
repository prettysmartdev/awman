//! Shared path-escape validation (Layer 0).
//!
//! `validate_under_root` generalizes the canonicalize-with-fallback +
//! `starts_with` check that `context_dirs::validate_context_path` previously
//! hardcoded to `~/.awman/context/`. Any accessor that resolves a
//! user-influenced sub-path (a task slug, a context slug) under a fixed
//! root uses this to guarantee the resolved path cannot escape the root via
//! `..` or a crafted component.

use std::path::{Component, Path, PathBuf};

use crate::data::error::DataError;

/// Reject a symlink at a managed path, including a dangling symlink.
/// Missing paths are allowed so create-if-missing callers can use this guard.
pub fn ensure_not_symlinked(path: &Path) -> Result<(), DataError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(DataError::InvalidPath {
            path: path.into(),
            reason: "must not be a symlink".into(),
        }),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(path, e)),
    }
}

/// Verify that `resolved` stays under `root`. Both are resolved with
/// [`canonicalize_lenient`], so the check holds for not-yet-created
/// directories as well as existing ones.
///
/// Returns `DataError::InvalidPath { path: resolved, reason }` when `resolved`
/// escapes `root`.
pub fn validate_under_root(root: &Path, resolved: &Path, reason: &str) -> Result<(), DataError> {
    let canonical_root = canonicalize_lenient(root);
    let canonical_resolved = canonicalize_lenient(resolved);
    if !canonical_resolved.starts_with(&canonical_root) {
        return Err(DataError::InvalidPath {
            path: resolved.to_path_buf(),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

/// Canonicalize as much of `path` as exists on disk.
///
/// `std::fs::canonicalize` fails outright when the leaf does not exist yet, and
/// the previous fallback — treating the whole path lexically — compared a
/// symlink-resolved root against an unresolved child. Wherever the root sits
/// behind a symlink (macOS resolves `/var/folders/...` to
/// `/private/var/folders/...`) that made a perfectly contained path look like
/// an escape.
///
/// So canonicalize the deepest existing ancestor, then re-apply the trailing
/// components lexically. Those components do not exist, so none of them can be
/// a symlink and resolving them lexically cannot hide a traversal — a trailing
/// `..` pops a directory exactly as the kernel would.
pub(crate) fn canonicalize_lenient(path: &Path) -> PathBuf {
    // Walk up to the deepest existing ancestor, collecting what we skip past.
    let mut tail: Vec<Component<'_>> = Vec::new();
    let mut cursor = path;
    let base = loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            break canonical;
        }
        let mut components = cursor.components();
        let Some(last) = components.next_back() else {
            // Nothing exists anywhere along the path, so keep it lexical.
            break PathBuf::new();
        };
        tail.push(last);
        cursor = components.as_path();
    };

    let mut resolved = base;
    for component in tail.into_iter().rev() {
        match component {
            Component::CurDir => {}
            // `pop` at the root is a no-op, matching the kernel's `/.. == /`.
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}
