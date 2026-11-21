//! Filesystem-resolution for overlay host paths.
//!
//! Layer 0 *resolves* paths (canonicalize, expand `~`, dedup keys); Layer 1
//! *mounts* them. Per the grand architecture, both concerns are kept apart.

use std::path::{Path, PathBuf};

/// Resolves overlay host paths from raw user input.
#[derive(Debug, Default, Clone)]
pub struct OverlayPathResolver;

impl OverlayPathResolver {
    pub fn new() -> Self {
        Self
    }

    /// Expand a leading `~` to the user's home directory.
    pub fn expand_tilde(path: &str) -> PathBuf {
        if path == "~" {
            return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        }
        if let Some(rest) = path.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        PathBuf::from(path)
    }

    /// Expand `~` and resolve a relative path to absolute (against `cwd`).
    pub fn make_absolute_with_cwd(path: &str, cwd: &Path) -> PathBuf {
        let expanded = Self::expand_tilde(path);
        if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        }
    }

    /// Resolve `.` and `..` components without touching the filesystem.
    ///
    /// Necessary before `canonicalize_lossy` so that `..` segments in
    /// non-existent subtrees collapse correctly: `/foo/baz/../bar` → `/foo/bar`
    /// regardless of whether `/foo/baz` exists.
    pub fn normalize_lexically(path: &Path) -> PathBuf {
        let mut result = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    match result.components().next_back() {
                        Some(std::path::Component::Normal(_)) => {
                            result.pop();
                        }
                        Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {
                            // Cannot go above the filesystem root — discard `..`.
                        }
                        _ => result.push(".."),
                    }
                }
                other => result.push(other),
            }
        }
        result
    }

    /// Best-effort canonicalisation that tolerates non-existent leaf paths.
    ///
    /// First normalises `.`/`..` lexically so that `foo/../bar` correctly
    /// collapses to `bar` even when `foo` does not exist.  Then walks up to
    /// the nearest existing ancestor, canonicalises that, and re-appends the
    /// missing trailing components.
    pub fn canonicalize_lossy(path: &Path) -> PathBuf {
        let normalized = Self::normalize_lexically(path);
        if let Ok(c) = std::fs::canonicalize(&normalized) {
            return c;
        }

        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        let mut cursor = normalized.as_path();
        loop {
            match cursor.parent() {
                None => break,
                Some(parent) => {
                    if let Some(name) = cursor.file_name() {
                        suffix.push(name.to_owned());
                    }
                    if let Ok(canon) = std::fs::canonicalize(parent) {
                        let mut out = canon;
                        for name in suffix.iter().rev() {
                            out.push(name);
                        }
                        return out;
                    }
                    cursor = parent;
                }
            }
        }
        normalized
    }

    /// Stable string key for deduplication — canonical path string with
    /// fallback to the raw input when canonicalisation fails entirely.
    pub fn conflict_key(path: &Path) -> String {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }
}
