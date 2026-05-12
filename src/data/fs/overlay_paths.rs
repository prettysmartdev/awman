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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn canonicalize_lossy_returns_canonical_for_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let result = OverlayPathResolver::canonicalize_lossy(tmp.path());
        assert!(result.is_absolute());
        // On macOS /var/folders is /private/var/folders — check the final component.
        let result_str = result.to_string_lossy();
        assert!(
            result_str.ends_with(tmp.path().file_name().unwrap().to_str().unwrap()),
            "canonical={result_str} should contain the temp dir name"
        );
    }

    #[test]
    fn canonicalize_lossy_handles_nonexistent_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does_not_exist");
        let result = OverlayPathResolver::canonicalize_lossy(&nonexistent);
        assert!(result.is_absolute());
        // The result should contain the non-existent leaf component appended to
        // the canonicalized parent.
        assert!(
            result.ends_with("does_not_exist"),
            "expected leaf component to be preserved, got {result:?}"
        );
        // The parent portion of the result should be the canonical temp dir.
        let parent = result.parent().unwrap();
        let canon_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(parent, canon_tmp);
    }

    #[test]
    fn canonicalize_lossy_handles_deeply_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c");
        // None of a, b, c exist under tmp.
        let result = OverlayPathResolver::canonicalize_lossy(&deep);
        assert!(result.is_absolute());
        // Should end with the non-existent components.
        assert!(result.ends_with(Path::new("a/b/c")));
    }

    #[test]
    fn canonicalize_lossy_with_dotdot_in_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the real dir so the parent is canonicalizable.
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        // Path: real/ghost/../sibling — ghost doesn't exist, so full canonicalize fails.
        // After lexical normalization ghost/../ collapses, leaving real/sibling.
        let test_path = real_dir.join("ghost").join("..").join("sibling");
        let result = OverlayPathResolver::canonicalize_lossy(&test_path);
        assert!(result.is_absolute(), "got {result:?}");
        // The dotdot must collapse: result should be canonical(real)/sibling, NOT
        // canonical(real)/ghost/sibling.
        let canon_real = std::fs::canonicalize(&real_dir).unwrap();
        assert_eq!(
            result,
            canon_real.join("sibling"),
            "dotdot should collapse: expected {}/sibling, got {result:?}",
            canon_real.display()
        );
    }

    #[test]
    fn canonicalize_lossy_absolute_root_never_panics() {
        // On any platform, "/" always exists.
        #[cfg(unix)]
        {
            let root = Path::new("/");
            let result = OverlayPathResolver::canonicalize_lossy(root);
            assert_eq!(result, Path::new("/"));
        }
    }

    #[test]
    fn expand_tilde_home_only() {
        // When dirs::home_dir() succeeds, "~" should expand to it.
        if let Some(home) = dirs::home_dir() {
            let result = OverlayPathResolver::expand_tilde("~");
            assert_eq!(result, home);
        }
    }

    #[test]
    fn expand_tilde_with_trailing_path() {
        if let Some(home) = dirs::home_dir() {
            let result = OverlayPathResolver::expand_tilde("~/docs/notes");
            assert_eq!(result, home.join("docs/notes"));
        }
    }

    #[test]
    fn expand_tilde_leaves_absolute_path_unchanged() {
        let result = OverlayPathResolver::expand_tilde("/absolute/path");
        assert_eq!(result, Path::new("/absolute/path"));
    }

    #[test]
    fn expand_tilde_leaves_relative_path_unchanged() {
        let result = OverlayPathResolver::expand_tilde("relative/path");
        assert_eq!(result, Path::new("relative/path"));
    }

    #[test]
    fn make_absolute_with_cwd_resolves_relative_against_cwd() {
        let cwd = Path::new("/some/base");
        let result = OverlayPathResolver::make_absolute_with_cwd("subdir/file", cwd);
        assert_eq!(result, Path::new("/some/base/subdir/file"));
    }

    #[test]
    fn make_absolute_with_cwd_leaves_absolute_unchanged() {
        let cwd = Path::new("/some/base");
        let result = OverlayPathResolver::make_absolute_with_cwd("/absolute/path", cwd);
        assert_eq!(result, Path::new("/absolute/path"));
    }

    // ─── normalize_lexically ──────────────────────────────────────────────────

    #[test]
    fn normalize_lexically_resolves_dotdot() {
        let result = OverlayPathResolver::normalize_lexically(Path::new("/foo/baz/../bar"));
        assert_eq!(result, Path::new("/foo/bar"));
    }

    #[test]
    fn normalize_lexically_resolves_multiple_dotdot() {
        let result = OverlayPathResolver::normalize_lexically(Path::new("/a/b/c/../../d"));
        assert_eq!(result, Path::new("/a/d"));
    }

    #[test]
    fn normalize_lexically_skips_cudir() {
        let result = OverlayPathResolver::normalize_lexically(Path::new("/a/./b/./c"));
        assert_eq!(result, Path::new("/a/b/c"));
    }

    #[test]
    fn normalize_lexically_leaves_clean_path_unchanged() {
        let result = OverlayPathResolver::normalize_lexically(Path::new("/a/b/c"));
        assert_eq!(result, Path::new("/a/b/c"));
    }

    #[test]
    fn normalize_lexically_dotdot_cannot_go_above_root() {
        let result = OverlayPathResolver::normalize_lexically(Path::new("/../../etc/passwd"));
        assert_eq!(result, Path::new("/etc/passwd"));
    }

    // ─── conflict_key ─────────────────────────────────────────────────────────

    #[test]
    fn conflict_key_returns_string_for_nonexistent_path() {
        let path = Path::new("/definitely/does/not/exist/at/all");
        let key = OverlayPathResolver::conflict_key(path);
        // Falls back to raw path when canonicalize fails.
        assert!(!key.is_empty());
    }

    #[test]
    fn conflict_key_is_stable_for_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let key1 = OverlayPathResolver::conflict_key(tmp.path());
        let key2 = OverlayPathResolver::conflict_key(tmp.path());
        assert_eq!(key1, key2);
    }
}
