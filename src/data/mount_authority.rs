//! The authorized mount boundary of a session.
//!
//! Admission decides which host directory a caller may mount at `/workspace`;
//! a [`MountAuthority`] records that decision so setup, persistence, recovery
//! and every launch can be held to it. The launch may never exceed it: the
//! final workspace mount must be exactly [`MountAuthority::root`], and every
//! auxiliary Git mount must be one of [`MountAuthority::git_dirs`] or lie
//! inside the root.
//!
//! Layer 0: pure path and file inspection only. Nothing here runs Git; code
//! that needs Git's opinion (worktree ownership) asks Layer 1 first and then
//! passes the verified result to a named constructor here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Version written into the persisted JSON form.
const PERSISTED_VERSION: u32 = 1;

/// How a [`MountAuthority`] was granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAuthoritySource {
    /// A CLI/TUI session: the cwd or the Git root the user confirmed.
    LocalInvocation,
    /// An exact `workDirs` allowlist entry.
    ExactWorkdir,
    /// A direct child of a `workdirRoots` entry that is its own Git root.
    WorkdirRootChild,
    /// awman's own clone of a remote repository under the API root.
    RemoteClone,
    /// A linked worktree derived from another authority.
    VerifiedWorktree,
}

impl MountAuthoritySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalInvocation => "local_invocation",
            Self::ExactWorkdir => "exact_workdir",
            Self::WorkdirRootChild => "workdir_root_child",
            Self::RemoteClone => "remote_clone",
            Self::VerifiedWorktree => "verified_worktree",
        }
    }

    /// Granted by an API allowlist or API-owned clone rather than by a user at
    /// a terminal. Such authorities fail closed on anything uncertain.
    pub fn is_policy_granted(self) -> bool {
        !matches!(self, Self::LocalInvocation)
    }
}

/// The canonical boundary a session's launches may mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountAuthority {
    root: PathBuf,
    git_dirs: Vec<PathBuf>,
    source: MountAuthoritySource,
    /// Whether the policy that granted this authority came from an API
    /// allowlist (inherited by re-rooted worktrees).
    policy_granted: bool,
}

/// Why a path cannot be held to, or does not fit, an authority.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MountAuthorityError {
    #[error("cannot resolve {}: {reason}", path.display())]
    Unresolvable { path: PathBuf, reason: String },
    #[error("{} is outside the mount authority {}", path.display(), root.display())]
    Outside { path: PathBuf, root: PathBuf },
    #[error("{} is not inside the awman clone directory {}", path.display(), base.display())]
    NotAwmanClone { path: PathBuf, base: PathBuf },
    #[error("{} cannot be mounted: {problem}", root.display())]
    GitMetadata {
        root: PathBuf,
        problem: GitMetadataProblem,
    },
    #[error("invalid persisted mount authority: {0}")]
    Persisted(String),
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    v: u32,
    root: PathBuf,
    git_dirs: Vec<PathBuf>,
    source: MountAuthoritySource,
    #[serde(default)]
    policy_granted: Option<bool>,
}

/// Canonicalize `path`, failing closed.
pub fn canonical(path: &Path) -> Result<PathBuf, MountAuthorityError> {
    std::fs::canonicalize(path).map_err(|e| MountAuthorityError::Unresolvable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Canonical form when the path exists, otherwise the path as given.
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl MountAuthority {
    fn new(root: PathBuf, git_dirs: Vec<PathBuf>, source: MountAuthoritySource) -> Self {
        Self {
            root,
            git_dirs,
            source,
            policy_granted: source.is_policy_granted(),
        }
    }

    /// The authority of a CLI/TUI session: the root its own open resolved.
    /// Kept as given when it does not exist (in-memory fixtures).
    pub fn local_invocation(root: &Path) -> Self {
        Self::new(
            canonical_or_self(root),
            Vec::new(),
            MountAuthoritySource::LocalInvocation,
        )
    }

    /// An exact allowlisted directory, already checked by the policy.
    pub fn exact_workdir(root: &Path) -> Result<Self, MountAuthorityError> {
        Ok(Self::new(
            canonical(root)?,
            Vec::new(),
            MountAuthoritySource::ExactWorkdir,
        ))
    }

    /// A direct child of an allowlisted root, already checked by the policy.
    pub fn workdir_root_child(root: &Path) -> Result<Self, MountAuthorityError> {
        Ok(Self::new(
            canonical(root)?,
            Vec::new(),
            MountAuthoritySource::WorkdirRootChild,
        ))
    }

    /// awman's own clone. The clone must exist and lie strictly inside
    /// `clone_base` (the API sessions directory), after resolving symlinks.
    pub fn remote_clone(clone: &Path, clone_base: &Path) -> Result<Self, MountAuthorityError> {
        let root = canonical(clone)?;
        let base = canonical(clone_base)?;
        if root == base || !root.starts_with(&base) {
            return Err(MountAuthorityError::NotAwmanClone { path: root, base });
        }
        Ok(Self::new(
            root,
            Vec::new(),
            MountAuthoritySource::RemoteClone,
        ))
    }

    /// Re-root this authority at a linked worktree checkout. The worktree's
    /// Git metadata may only come from this authority's own repository: its
    /// `.git` directory (or, for a local invocation inside a linked worktree,
    /// the common directory its pointer names), plus what it already allowed.
    ///
    /// The caller must have verified with Git that `worktree` belongs to this
    /// repository; this constructor only decides which Git directories the
    /// re-rooted launch may mount, and never widens them beyond the source.
    pub fn verified_worktree(&self, worktree: &Path) -> Self {
        let mut git_dirs = self.git_dirs.clone();
        if let Some(dir) = repository_git_dir(&self.root, !self.policy_granted) {
            if !git_dirs.contains(&dir) {
                git_dirs.push(dir);
            }
        }
        Self {
            root: canonical_or_self(worktree),
            git_dirs,
            source: MountAuthoritySource::VerifiedWorktree,
            policy_granted: self.policy_granted,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dirs(&self) -> &[PathBuf] {
        &self.git_dirs
    }

    pub fn source(&self) -> MountAuthoritySource {
        self.source
    }

    /// Granted by an API allowlist or clone (directly or via a worktree).
    pub fn is_policy_granted(&self) -> bool {
        self.policy_granted
    }

    /// The versioned JSON stored in `sessions.mount_authority`.
    pub fn to_persisted(&self) -> String {
        serde_json::to_string(&Persisted {
            v: PERSISTED_VERSION,
            root: self.root.clone(),
            git_dirs: self.git_dirs.clone(),
            source: self.source,
            policy_granted: Some(self.policy_granted),
        })
        .expect("mount authority serializes")
    }

    /// Parse the persisted JSON. Unknown versions are refused.
    pub fn from_persisted(text: &str) -> Result<Self, MountAuthorityError> {
        let p: Persisted = serde_json::from_str(text)
            .map_err(|e| MountAuthorityError::Persisted(e.to_string()))?;
        if p.v != PERSISTED_VERSION {
            return Err(MountAuthorityError::Persisted(format!(
                "unsupported version {}",
                p.v
            )));
        }
        Ok(Self {
            policy_granted: p
                .policy_granted
                .unwrap_or_else(|| p.source.is_policy_granted()),
            root: p.root,
            git_dirs: p.git_dirs,
            source: p.source,
        })
    }

    /// Check the host source of the `/workspace` mount. Policy-granted
    /// authorities require the path to resolve now; a local invocation over a
    /// path that does not exist compares the path as given.
    pub fn check_workspace(&self, host_path: &Path) -> Result<(), MountAuthorityError> {
        let resolved = if self.policy_granted {
            canonical(host_path)?
        } else {
            canonical_or_self(host_path)
        };
        if resolved != self.root {
            return Err(MountAuthorityError::Outside {
                path: resolved,
                root: self.root.clone(),
            });
        }
        if self.policy_granted {
            if let Some(problem) = git_metadata_problem(&self.root) {
                if problem != GitMetadataProblem::PointerFile
                    || self.source != MountAuthoritySource::VerifiedWorktree
                {
                    return Err(MountAuthorityError::GitMetadata {
                        root: self.root.clone(),
                        problem,
                    });
                }
            }
        }
        Ok(())
    }

    /// Check one host mount source. A mount is Git metadata when its target
    /// is a Git directory now, *or* when it was declared as one: its host or
    /// guest path names a `.git` component or lies in one of `git_dirs`. A
    /// declared Git mount is judged by where its source resolves, whatever
    /// that target now contains, so replacing a verified `.git` with a link
    /// to an unrelated (even non-Git) directory cannot authorize it. Git
    /// metadata must resolve inside the root or inside one of `git_dirs`.
    pub fn check_git_mount(
        &self,
        host_path: &Path,
        container_path: &Path,
    ) -> Result<(), MountAuthorityError> {
        let declared = self.declares_git_mount(host_path, container_path);
        let resolved = if declared {
            canonical(host_path)?
        } else {
            canonical_or_self(host_path)
        };
        if !declared && enclosing_git_dir(&resolved).is_none() {
            return Ok(());
        }
        if resolved.starts_with(&self.root) || self.git_dirs.iter().any(|d| resolved.starts_with(d))
        {
            return Ok(());
        }
        Err(MountAuthorityError::Outside {
            path: resolved,
            root: self.root.clone(),
        })
    }

    /// Whether a mount was declared as Git metadata by its spelling: its host
    /// or guest path has a `.git` component or lies in one of `git_dirs`.
    pub fn declares_git_mount(&self, host_path: &Path, container_path: &Path) -> bool {
        names_git_metadata(host_path)
            || names_git_metadata(container_path)
            || self
                .git_dirs
                .iter()
                .any(|d| host_path.starts_with(d) || container_path.starts_with(d))
    }

    /// Whether a restored authority is still within what `current` grants:
    /// same root and source, and no Git directory `current` does not allow.
    pub fn is_within(&self, current: &MountAuthority) -> bool {
        self.root == current.root
            && self.source == current.source
            && self
                .git_dirs
                .iter()
                .all(|d| d.starts_with(&current.root) || current.git_dirs.contains(d))
    }
}

/// Why a directory is not a plain, self-contained Git root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitMetadataProblem {
    /// `.git` is a pointer file (a linked worktree or submodule checkout).
    PointerFile,
    /// `.git` is a symlink.
    Symlink,
    /// `.git` is neither a file, directory nor symlink.
    Special,
    /// The repository config sets `core.worktree`, so Git operates on a
    /// directory other than this one.
    CoreWorktree,
    /// The repository config includes other files, which could set
    /// `core.worktree` out of sight.
    ConfigInclude,
}

impl std::fmt::Display for GitMetadataProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PointerFile => "its .git is a pointer file",
            Self::Symlink => "its .git is a symlink",
            Self::Special => "its .git is not a regular directory",
            Self::CoreWorktree => "its Git config sets core.worktree",
            Self::ConfigInclude => "its Git config includes other files",
        })
    }
}

/// Inspect `root/.git` without following it. `None` when there is no `.git`
/// at all or it is a real directory whose config keeps the work tree here.
pub fn git_metadata_problem(root: &Path) -> Option<GitMetadataProblem> {
    let dot_git = root.join(".git");
    let metadata = std::fs::symlink_metadata(&dot_git).ok()?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return Some(GitMetadataProblem::Symlink);
    }
    if kind.is_file() {
        return Some(GitMetadataProblem::PointerFile);
    }
    if !kind.is_dir() {
        return Some(GitMetadataProblem::Special);
    }
    config_problem(&dot_git.join("config"))
}

/// Scan a Git config file for `core.worktree` and include directives. A
/// minimal reader: section headers and `key = value` lines, case-insensitive
/// names, comments ignored. Unreadable config is not judged here; Git would
/// fail on it as well.
fn config_problem(config: &Path) -> Option<GitMetadataProblem> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut rest = line;
        if let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else {
                continue;
            };
            section = stripped[..end]
                .split(|c: char| c.is_whitespace() || c == '"')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if section == "include" || section == "includeif" {
                return Some(GitMetadataProblem::ConfigInclude);
            }
            rest = stripped[end + 1..].trim();
            if rest.is_empty() {
                continue;
            }
        }
        let key = rest
            .split(|c: char| c == '=' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if section == "core" && key == "worktree" {
            return Some(GitMetadataProblem::CoreWorktree);
        }
    }
    None
}

/// The Git directory of the repository rooted at `root`: `root/.git` when it
/// is a real directory, or (only when `follow_pointer`) the common directory
/// named by a linked worktree's `.git` pointer file.
fn repository_git_dir(root: &Path, follow_pointer: bool) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let metadata = std::fs::symlink_metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return std::fs::canonicalize(&dot_git).ok();
    }
    if !follow_pointer || !metadata.is_file() {
        return None;
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let admin = root.join(pointer.trim_end().strip_prefix("gitdir: ")?);
    let commondir = std::fs::read_to_string(admin.join("commondir")).ok()?;
    std::fs::canonicalize(admin.join(commondir.trim())).ok()
}

/// Whether a path's spelling declares Git metadata: one of its components
/// is `.git`.
fn names_git_metadata(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
}

/// The on-disk identity of a mount source. It is pinned when a launch's
/// mounts are first checked and compared again just before they are handed
/// to the runtime, so a directory renamed or replaced in between is refused
/// even when its path still resolves to the same place. `None` where the
/// platform offers Layer 0 no stable identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceIdentity {
    dev: u64,
    ino: u64,
}

impl SourceIdentity {
    /// The identity of what `path` resolves to now, following links.
    pub fn of(path: &Path) -> Result<Option<Self>, MountAuthorityError> {
        let metadata = std::fs::metadata(path).map_err(|e| MountAuthorityError::Unresolvable {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        Ok(Self::from_metadata(&metadata))
    }

    #[cfg(unix)]
    fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    #[cfg(not(unix))]
    fn from_metadata(_: &std::fs::Metadata) -> Option<Self> {
        None
    }
}

/// Whether `path` is a Git directory: named `.git`, or holding `HEAD`,
/// `objects/` and `refs/` like a bare or common directory.
pub fn is_git_dir(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    if path.file_name().is_some_and(|n| n == ".git") {
        return true;
    }
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

/// The nearest Git directory that is `path` or one of its ancestors.
fn enclosing_git_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| is_git_dir(p))
        .map(Path::to_path_buf)
}

/// Carries the engine's authorized mount identities to the backend's create boundary.
#[derive(Debug, Clone)]
pub struct MountSourceGuard {
    pub authority: MountAuthority,
    pub sources: Vec<(PathBuf, Option<SourceIdentity>)>,
}
impl MountSourceGuard {
    pub fn verify(&self) -> Result<(), MountAuthorityError> {
        self.authority.check_workspace(self.authority.root())?;
        for (path, identity) in &self.sources {
            if SourceIdentity::of(path)? != *identity {
                return Err(MountAuthorityError::Unresolvable {
                    path: path.clone(),
                    reason: "mount source identity changed before runtime create".into(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_dir(at: &Path) {
        std::fs::create_dir_all(at.join("objects")).unwrap();
        std::fs::create_dir_all(at.join("refs")).unwrap();
        std::fs::write(at.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn persisted_form_round_trips_and_refuses_unknown_versions() {
        let dir = tempfile::tempdir().unwrap();
        let a = MountAuthority::exact_workdir(dir.path()).unwrap();
        let text = a.to_persisted();
        assert!(text.contains("\"v\":1"), "{text}");
        assert!(text.contains("\"source\":\"exact_workdir\""), "{text}");
        assert_eq!(MountAuthority::from_persisted(&text).unwrap(), a);
        let future = text.replace("\"v\":1", "\"v\":2");
        assert!(MountAuthority::from_persisted(&future).is_err());
        assert!(MountAuthority::from_persisted("not json").is_err());
    }

    #[test]
    fn workspace_must_equal_the_root_and_a_parent_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("allowed");
        std::fs::create_dir(&child).unwrap();
        let a = MountAuthority::exact_workdir(&child).unwrap();
        a.check_workspace(&child).unwrap();
        assert!(matches!(
            a.check_workspace(dir.path()),
            Err(MountAuthorityError::Outside { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_replaced_by_a_symlink_after_grant_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("allowed");
        let other = dir.path().join("other");
        std::fs::create_dir(&child).unwrap();
        std::fs::create_dir(&other).unwrap();
        let a = MountAuthority::exact_workdir(&child).unwrap();
        std::fs::remove_dir(&child).unwrap();
        std::os::unix::fs::symlink(&other, &child).unwrap();
        assert!(a.check_workspace(&child).is_err());
    }

    #[test]
    fn a_policy_granted_root_that_vanished_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("allowed");
        std::fs::create_dir(&child).unwrap();
        let a = MountAuthority::exact_workdir(&child).unwrap();
        std::fs::remove_dir(&child).unwrap();
        assert!(matches!(
            a.check_workspace(&child),
            Err(MountAuthorityError::Unresolvable { .. })
        ));
    }

    #[test]
    fn git_metadata_problems_are_detected_without_running_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(git_metadata_problem(root), None);
        git_dir(&root.join(".git"));
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        assert_eq!(git_metadata_problem(root), None);
        std::fs::write(
            root.join(".git/config"),
            "[core]\n\tbare = false\n\tWorkTree = /elsewhere\n",
        )
        .unwrap();
        assert_eq!(
            git_metadata_problem(root),
            Some(GitMetadataProblem::CoreWorktree)
        );
        std::fs::write(
            root.join(".git/config"),
            "[includeIf \"gitdir:/x\"]\npath = y\n",
        )
        .unwrap();
        assert_eq!(
            git_metadata_problem(root),
            Some(GitMetadataProblem::ConfigInclude)
        );
        let linked = tempfile::tempdir().unwrap();
        std::fs::write(linked.path().join(".git"), "gitdir: /x/.git/worktrees/a\n").unwrap();
        assert_eq!(
            git_metadata_problem(linked.path()),
            Some(GitMetadataProblem::PointerFile)
        );
    }

    #[test]
    fn a_policy_granted_root_whose_git_config_gained_core_worktree_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        git_dir(&dir.path().join(".git"));
        let a = MountAuthority::exact_workdir(dir.path()).unwrap();
        a.check_workspace(dir.path()).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\nworktree=/\n").unwrap();
        assert!(a.check_workspace(dir.path()).is_err());
        // A local invocation is the user's own checkout; not judged here.
        MountAuthority::local_invocation(dir.path())
            .check_workspace(dir.path())
            .unwrap();
    }

    #[test]
    fn git_mounts_must_be_inside_the_root_or_listed() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        git_dir(&main.join(".git"));
        let foreign = dir.path().join("foreign");
        git_dir(&foreign.join(".git"));
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let worktree = dir.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();

        let a = MountAuthority::exact_workdir(&main).unwrap();
        let check = |a: &MountAuthority, p: &Path| a.check_git_mount(p, p);
        check(&a, &main.join(".git")).unwrap();
        check(&a, &plain).unwrap();
        assert!(check(&a, &foreign.join(".git")).is_err());
        assert!(check(&a, &foreign.join(".git/objects")).is_err());

        let wt = a.verified_worktree(&worktree);
        assert_eq!(wt.source(), MountAuthoritySource::VerifiedWorktree);
        assert!(wt.is_policy_granted());
        assert_eq!(wt.root(), worktree.canonicalize().unwrap());
        assert_eq!(wt.git_dirs(), [main.join(".git").canonicalize().unwrap()]);
        check(&wt, &main.join(".git")).unwrap();
        assert!(check(&wt, &foreign.join(".git")).is_err());
    }

    /// A verified Git directory swapped for a link to an unrelated directory
    /// (with or without Git contents) is still judged as the declared Git
    /// mount and refused; a dangling declared source fails closed.
    #[cfg(unix)]
    #[test]
    fn a_declared_git_mount_is_judged_by_its_source_whatever_it_now_contains() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        git_dir(&main.join(".git"));
        let worktree = dir.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let wt = MountAuthority::exact_workdir(&main)
            .unwrap()
            .verified_worktree(&worktree);
        let git = wt.git_dirs()[0].clone();
        wt.check_git_mount(&git, &git).unwrap();

        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let foreign_repo = dir.path().join("foreign_repo");
        git_dir(&foreign_repo);
        let saved = dir.path().join("saved-git");
        std::fs::rename(&git, &saved).unwrap();
        for target in [&plain, &foreign_repo] {
            std::os::unix::fs::symlink(target, &git).unwrap();
            assert!(
                matches!(
                    wt.check_git_mount(&git, &git),
                    Err(MountAuthorityError::Outside { .. })
                ),
                "{target:?}"
            );
            std::fs::remove_file(&git).unwrap();
        }
        // A `.git` link in a plain directory, mounted elsewhere in the guest,
        // is declared by its host spelling.
        let holder = dir.path().join("holder");
        std::fs::create_dir(&holder).unwrap();
        std::os::unix::fs::symlink(&plain, holder.join(".git")).unwrap();
        assert!(wt
            .check_git_mount(&holder.join(".git"), Path::new("/opt/data"))
            .is_err());
        assert!(matches!(
            wt.check_git_mount(&git, &git),
            Err(MountAuthorityError::Unresolvable { .. })
        ));
        std::fs::rename(&saved, &git).unwrap();
        wt.check_git_mount(&git, &git).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn source_identity_changes_when_a_directory_is_replaced_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let pinned = SourceIdentity::of(&root).unwrap();
        assert!(pinned.is_some());
        assert_eq!(SourceIdentity::of(&root).unwrap(), pinned);
        std::fs::rename(&root, dir.path().join("old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert_ne!(SourceIdentity::of(&root).unwrap(), pinned);
    }

    #[test]
    fn a_policy_grant_never_follows_a_pointer_file_to_new_git_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("elsewhere/.git");
        git_dir(&common);
        let admin = common.join("worktrees/a");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let linked = dir.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let local = MountAuthority::local_invocation(&linked).verified_worktree(&wt);
        assert_eq!(local.git_dirs(), [common.canonicalize().unwrap()]);
        let granted = MountAuthority::exact_workdir(&linked)
            .unwrap()
            .verified_worktree(&wt);
        assert!(granted.git_dirs().is_empty());
    }

    #[test]
    fn remote_clone_must_be_inside_the_clone_base() {
        let base = tempfile::tempdir().unwrap();
        let clone = base.path().join("ses_1/repo");
        std::fs::create_dir_all(&clone).unwrap();
        MountAuthority::remote_clone(&clone, base.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        assert!(MountAuthority::remote_clone(outside.path(), base.path()).is_err());
        assert!(MountAuthority::remote_clone(base.path(), base.path()).is_err());
    }

    #[test]
    fn a_restored_authority_is_bounded_by_the_current_grant() {
        let dir = tempfile::tempdir().unwrap();
        let a = MountAuthority::exact_workdir(dir.path()).unwrap();
        assert!(a.is_within(&a.clone()));
        let child = MountAuthority::workdir_root_child(dir.path()).unwrap();
        assert!(!a.is_within(&child));
        let other = tempfile::tempdir().unwrap();
        assert!(!a.is_within(&MountAuthority::exact_workdir(other.path()).unwrap()));
    }
}
