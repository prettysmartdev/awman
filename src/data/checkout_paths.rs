//! Repository checkout naming and paths — Layer 0.
//!
//! Resolves `~/.awman/worktrees/<dirname>-<hash8>/...` paths and the
//! deterministic branch names that go with them. Pure path computation — no
//! git invocation and no canonicalization (that's `GitEngine`): the caller
//! passes the canonical git root.

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;

fn ascii_component(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A generated filename/ref component. Preserve ordinary names; escape unsafe
/// input and bound its length, retaining an identity hash whenever it changes.
/// Explicit user-selected branches and paths must be validated, not rewritten.
fn generated_component(name: &str) -> String {
    let mut safe = ascii_component(name);
    if safe.len() > 200 {
        safe.truncate(180);
    }
    while safe.contains("..") {
        safe = safe.replace("..", "--");
    }
    safe = safe.trim_matches('.').to_string();
    if safe.ends_with(".lock") {
        safe.push('-');
    }
    // Windows device names are invalid even with an extension.
    let stem = safe.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        safe.insert_str(0, "repo-");
    }
    if safe.is_empty() {
        safe.push_str("repo");
    }
    if safe != name {
        safe.truncate(180);
        safe.push('-');
        safe.push_str(&sha256_hex(name.as_bytes())[..8]);
    }
    safe
}

/// An awman-owned branch from a generated issue or worker-run name.
pub fn awman_branch_name(name: &str) -> String {
    format!("awman/{}", generated_component(name))
}

/// Directory component for a named workflow worktree.
pub fn workflow_worktree_dir_name(name: &str) -> String {
    format!("wf-{}", generated_component(name))
}

/// A workflow's logical name, before the shared branch/path encoding.
pub fn workflow_name_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("workflow")
        .to_string()
}

/// GitHub issue worktrees use a number plus a bounded readable title.
pub fn github_worktree_name(number: u32, title: &str) -> String {
    let title = crate::data::issue::slugify(title, 40);
    if title.is_empty() {
        number.to_string()
    } else {
        format!("{number}-{title}")
    }
}

/// Non-GitHub remote items include their provider slug and stable key.
pub fn remote_worktree_name(slug: Option<&str>, key: &str) -> String {
    format!("{}-{key}", slug.unwrap_or("work-item"))
}

/// Local branch for fork/deleted-head pull requests.
pub fn pull_request_branch_name(number: u32) -> String {
    format!("pr/{number}")
}

/// Private cache ref for a PR head; `remote` has passed Git remote validation.
pub fn pull_request_cache_ref(remote: &str, number: u32) -> String {
    format!("refs/awman/pull-heads/{remote}/{number}")
}

/// Default clone directory for URL, SCP-style and local repository sources.
/// Transport overrides do not change the logical source's directory name.
pub fn repo_folder_from_url(url: &str) -> String {
    let source = url.trim().trim_end_matches(['/', '\\']);
    let leaf = source.rsplit(['/', '\\', ':']).next().unwrap_or("");
    let name = leaf.strip_suffix(".git").unwrap_or(leaf);
    if name.is_empty() || name == "." || name == ".." {
        "repo".into()
    } else {
        generated_component(name)
    }
}

/// An explicit workflow `into` is a path, not a generated name: preserve it.
pub fn clone_target_dir(url: &str, into: Option<&str>) -> String {
    into.map(str::to_string)
        .unwrap_or_else(|| repo_folder_from_url(url))
}

/// A remote session's clone under its private session directory.
pub fn clone_path(base: &Path, url: &str) -> PathBuf {
    base.join(repo_folder_from_url(url))
}

/// Pulled leases have their own lifetime and namespace, not URL-based names.
pub fn leases_dir(work_root: &Path) -> PathBuf {
    work_root.join("leases")
}

/// `<work_root>/leases/<lease_id>/`.
pub fn lease_dir(work_root: &Path, lease: &crate::data::ids::LeaseId) -> PathBuf {
    leases_dir(work_root).join(lease.as_str())
}

/// Persistent squad workspace under an already-validated task directory.
pub fn squad_workspace_path(task_dir: &Path) -> PathBuf {
    task_dir.join(SQUAD_WORKSPACE_LEAF)
}

/// Shared with image identity recognition of squad workspace paths.
pub const SQUAD_WORKSPACE_LEAF: &str = "workspace";

/// Skill library clone under its managed root; `slug` is already validated by
/// the library parser (or the configured built-in library name).
pub fn skill_library_clone_path(library_root: &Path, slug: &str) -> PathBuf {
    library_root.join(slug)
}

/// Branch name for a work-item worktree: `awman/work-item-NNNN`.
pub fn worktree_branch_name(work_item: u32) -> String {
    awman_branch_name(&format!("work-item-{}", work_item_dir_name(work_item)))
}

/// Canonical local work-item directory component, also used for numeric input.
pub fn work_item_dir_name(work_item: u32) -> String {
    format!("{work_item:04}")
}

/// Branch name for a named workflow worktree: `awman/workflow-<name>`.
pub fn worktree_branch_name_for_workflow(name: &str) -> String {
    format!("awman/workflow-{}", generated_component(name))
}

/// Directory name for a session worktree created by `--branch <branch>`:
/// `br-<sanitized>`. Every character outside ASCII `[A-Za-z0-9._-]` becomes
/// `-` (case is preserved). When that changed the name, `-<hash8>` of the
/// original branch name's SHA-256 is appended, so `a/b` and `a-b` map to
/// different directories. Names are never truncated.
pub fn branch_worktree_dir_name(branch: &str) -> String {
    let sanitized = ascii_component(branch);
    if sanitized == branch {
        format!("br-{sanitized}")
    } else {
        let hash = sha256_hex(branch.as_bytes());
        format!("br-{sanitized}-{}", &hash[..8])
    }
}

/// Directory name for a session worktree created by `--pr <N>`: `pr-<N>`.
pub fn pull_request_worktree_dir_name(number: u32) -> String {
    format!("pr-{number}")
}

/// `<dirname>-<hash8>`: dirname = last component of `git_root` (or `repo`),
/// hash8 = first 8 lowercase hex digits of SHA-256 over the path's
/// `to_string_lossy()` bytes. Two checkouts that share a directory name get
/// different keys. `git_root` must already be canonical.
pub fn repo_key(git_root: &Path) -> String {
    let hash = sha256_hex(git_root.to_string_lossy().as_bytes());
    format!("{}-{}", repo_dirname(git_root), &hash[..8])
}

/// Last path component of `git_root`, or `repo` when there is none.
fn repo_dirname(git_root: &Path) -> &str {
    git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
}

/// Resolves worktree paths beneath `<HOME>/.awman/worktrees/<dirname>-<hash8>/`.
#[derive(Debug, Clone)]
pub struct WorktreePaths {
    home: PathBuf,
}

impl WorktreePaths {
    /// Resolve against the invoking instance, including when a daemon's
    /// own process has a different HOME.
    pub fn from_env(env: &crate::data::config::env::EnvSnapshot) -> Result<Self, DataError> {
        Ok(Self::with_home(
            env.home_dir().ok_or(DataError::HomeNotFound)?,
        ))
    }
    /// Construct using the OS home dir. Returns `HomeNotFound` when no home is
    /// resolvable.
    pub fn from_home() -> Result<Self, DataError> {
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self { home })
    }

    /// Construct with an explicit home directory (mostly for tests).
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/<NNNN>/`.
    pub fn for_work_item(&self, git_root: &Path, work_item: u32) -> PathBuf {
        self.repo_dir(git_root).join(work_item_dir_name(work_item))
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/wf-<name>/`.
    pub fn for_workflow(&self, git_root: &Path, name: &str) -> PathBuf {
        self.repo_dir(git_root)
            .join(workflow_worktree_dir_name(name))
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/<name>/`, with `name` joined
    /// verbatim (no `wf-` prefix). The caller must pass one validated, normal
    /// path component; this layer does no validation.
    pub fn named(&self, git_root: &Path, name: &str) -> PathBuf {
        self.repo_dir(git_root).join(name)
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/`.
    fn repo_dir(&self, git_root: &Path) -> PathBuf {
        self.home
            .join(".awman")
            .join("worktrees")
            .join(repo_key(git_root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn existing_names_and_storage_namespaces_remain_stable() {
        let root = Path::new("/repos/app");
        let paths = WorktreePaths::with_home("/home/test");
        let base = Path::new("/home/test/.awman/worktrees").join(repo_key(root));
        assert_eq!(paths.for_work_item(root, 42), base.join("0042"));
        assert_eq!(worktree_branch_name(42), "awman/work-item-0042");
        assert_eq!(
            paths.for_workflow(root, "build-v2"),
            base.join("wf-build-v2")
        );
        assert_eq!(
            worktree_branch_name_for_workflow("build-v2"),
            "awman/workflow-build-v2"
        );
        assert_eq!(
            github_worktree_name(84, "Fix login timeout"),
            "84-fix-login-timeout"
        );
        assert_eq!(github_worktree_name(84, "💥"), "84");
        assert_eq!(
            awman_branch_name("84-fix-login-timeout"),
            "awman/84-fix-login-timeout"
        );
        assert_eq!(
            remote_worktree_name(Some("ghb84-fix"), "remote-deadbeef"),
            "ghb84-fix-remote-deadbeef"
        );
        assert_eq!(pull_request_branch_name(12), "pr/12");
        assert_eq!(pull_request_worktree_dir_name(12), "pr-12");
        assert_eq!(branch_worktree_dir_name("main"), "br-main");
        assert_ne!(
            branch_worktree_dir_name("feature/a"),
            branch_worktree_dir_name("feature-a")
        );
        assert_ne!(repo_key(root), repo_key(Path::new("/other/app")));
        let lease = crate::data::ids::LeaseId::parse("lease_00000000000000000000000000000000");
        // The typed ID's prefix is part of the worker protocol, not a URL slug.
        let lease = lease.unwrap();
        assert_eq!(
            lease_dir(Path::new("/worker"), &lease),
            Path::new("/worker/leases").join(lease.as_str())
        );
    }

    #[test]
    fn clone_destinations_share_one_policy_and_preserve_explicit_paths() {
        for url in [
            "https://host/team/Repo.git/",
            "git@host:team/Repo.git",
            "ssh://git@host/team/Repo.git",
            "/local/Repo.git",
            r"C:\local\Repo.git",
        ] {
            assert_eq!(repo_folder_from_url(url), "Repo");
            assert_eq!(clone_target_dir(url, None), "Repo");
            assert_eq!(
                clone_path(Path::new("/session"), url),
                Path::new("/session/Repo")
            );
            assert_eq!(
                clone_target_dir(url, Some("../chosen checkout")),
                "../chosen checkout"
            );
        }
        assert_eq!(repo_folder_from_url("https://host/.git"), "repo");
        assert_ne!(
            repo_folder_from_url("https://host/a b.git"),
            repo_folder_from_url("https://host/a-b.git")
        );
        for name in [
            "../escape",
            "a..b",
            "foo.lock",
            "a/b",
            "a\\b",
            "💥",
            "CON",
            "",
        ] {
            let component = generated_component(name);
            assert_eq!(Path::new(&component).components().count(), 1);
            assert!(!component.contains(".."));
            assert!(!component.ends_with(".lock"));
            assert!(!component.starts_with('.'));
            assert!(!component.ends_with('.'));
        }
    }

    proptest! {
        #[test]
        fn workflow_branch_and_directory_use_the_same_safe_identity(name in ".{0,300}") {
            let branch = worktree_branch_name_for_workflow(&name);
            let directory = workflow_worktree_dir_name(&name);
            let component = directory.strip_prefix("wf-").unwrap();
            prop_assert_eq!(branch.strip_prefix("awman/workflow-").unwrap(), component);
            prop_assert!(!component.is_empty());
            prop_assert!(component.len() <= 200);
            prop_assert!(component.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
            prop_assert!(!component.contains(".."));
            prop_assert!(!component.ends_with(".lock"));
            prop_assert_eq!(generated_component(component), component);
        }
    }
}
