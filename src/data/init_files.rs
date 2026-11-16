//! What `awman init` wrote — Layer 0 data and file writes.
//!
//! `init` reports every project file it created, updated or kept as an
//! [`InitFile`], with its path relative to the git root, so a client (an API
//! caller doing project setup, say) knows exactly what to commit without
//! diffing the tree. The project Dockerfile can also be reported
//! [`InitFileStatus::Skipped`] with a reason.
//!
//! Every write goes through [`RepoTree`]: no ancestor may be a symbolic
//! link, and the project Dockerfile is created exclusively, so a file that
//! appears between the existence check and the write — or one that exists
//! but is empty — is kept byte-for-byte.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::fs::repo_tree::{
    normalize, CreateOutcome, RepoEntry, RepoRelPath, RepoTree, RepoTreeError, WriteOutcome,
};

/// Which project file an [`InitFile`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitFileKind {
    /// The project Dockerfile: `runtime.dockerfile` when configured, else
    /// `Dockerfile.dev`.
    ProjectDockerfile,
    /// `.awman/config.json`.
    RepoConfig,
    /// `.awman/Dockerfile.<agent>`.
    AgentDockerfile,
    /// A file under `aspec/` from the template (`init --aspec`).
    AspecFile,
}

/// What `init` did to an [`InitFile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InitFileStatus {
    /// The file did not exist; init created it (from a template, a rendered
    /// agent definition or a fresh config).
    Created,
    /// The file existed and init changed it (a config field merged in, a
    /// generated agent Dockerfile re-rendered).
    Updated,
    /// The file existed and init left it byte-for-byte unchanged.
    Kept,
    /// Init did not create the file. Only the project Dockerfile is reported
    /// this way, with why.
    Skipped { reason: String },
}

impl InitFileStatus {
    /// Whether init wrote this file.
    pub fn is_written(&self) -> bool {
        matches!(self, Self::Created | Self::Updated)
    }
}

impl From<WriteOutcome> for InitFileStatus {
    fn from(outcome: WriteOutcome) -> Self {
        match outcome {
            WriteOutcome::Created => Self::Created,
            WriteOutcome::Updated => Self::Updated,
            WriteOutcome::Unchanged => Self::Kept,
        }
    }
}

/// One project file `init` created, updated, kept or skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitFile {
    /// `/`-separated path relative to the git root. A file outside the git
    /// root (an absolute `runtime.dockerfile`) carries the configured path
    /// verbatim.
    pub path: String,
    pub kind: InitFileKind,
    #[serde(flatten)]
    pub status: InitFileStatus,
    /// Whether the file differs from the repository's committed tree (new or
    /// changed), i.e. whether a client has something to commit. This covers
    /// files an earlier step created — session setup provisions a missing
    /// `Dockerfile.dev` before init runs, so init reports it `kept` but
    /// `uncommitted`.
    #[serde(default)]
    pub uncommitted: bool,
}

impl InitFile {
    /// A file at the `/`-separated `path`; `uncommitted` defaults to whether
    /// init wrote it until the caller consults git.
    pub fn new(path: String, kind: InitFileKind, status: InitFileStatus) -> Self {
        Self {
            path,
            kind,
            uncommitted: status.is_written(),
            status,
        }
    }

    /// A file at `path`, reported relative to `git_root` when it is inside it.
    pub fn at(git_root: &Path, path: &Path, kind: InitFileKind, status: InitFileStatus) -> Self {
        Self::new(display_relative(git_root, path), kind, status)
    }
}

/// Where the project Dockerfile lives, relative to the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectDockerfileTarget {
    /// Inside the git root: setup may create it (through [`RepoTree`], which
    /// still refuses a linked ancestor).
    InRepo(RepoRelPath),
    /// Outside the git root (an absolute or `..`-escaping
    /// `runtime.dockerfile`): setup never creates it, because a client could
    /// not commit it.
    OutsideRepo(PathBuf),
}

impl ProjectDockerfileTarget {
    /// Classify the resolved project Dockerfile path against `tree`'s root.
    pub fn classify(tree: &RepoTree, path: &Path) -> Self {
        match tree.relative(path) {
            Some(rel) => Self::InRepo(rel),
            None => Self::OutsideRepo(path.to_path_buf()),
        }
    }

    /// Why setup does not create an outside target.
    pub fn outside_reason(path: &Path) -> String {
        format!(
            "runtime.dockerfile {} is outside the git root; awman does not create it",
            path.display()
        )
    }
}

/// What is at a configured project Dockerfile path before setup decides
/// whether to create it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingDockerfile {
    /// Nothing there: setup may create it.
    Missing,
    /// A regular file, or a link resolving to one (the build reads it; setup
    /// never writes through it).
    Usable,
    /// Something a build cannot use — a dangling link, a directory, a
    /// special file — with why.
    Unusable(String),
}

/// Inspect the project Dockerfile at `path`, following a final link: this
/// only decides whether a build can read it, never where to write.
pub fn inspect_project_dockerfile(path: &Path) -> ExistingDockerfile {
    let shown = path.display();
    let Ok(link) = std::fs::symlink_metadata(path) else {
        return ExistingDockerfile::Missing;
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => ExistingDockerfile::Usable,
        Ok(meta) if meta.is_dir() => {
            ExistingDockerfile::Unusable(format!("{shown} is a directory, not a Dockerfile"))
        }
        Ok(_) => {
            ExistingDockerfile::Unusable(format!("{shown} is a special file, not a Dockerfile"))
        }
        Err(_) if link.file_type().is_symlink() => ExistingDockerfile::Unusable(format!(
            "{shown} is a symbolic link whose target does not exist"
        )),
        Err(e) => ExistingDockerfile::Unusable(format!("{shown} cannot be read: {e}")),
    }
}

/// Create the project Dockerfile at `rel` from `contents` unless something is
/// already there. Never overwrites: an existing regular file — even empty, or
/// one that appeared since it was inspected — is `Kept`. A linked ancestor or
/// a non-file at `rel` is `Skipped` with the reason; only an I/O failure is
/// an error.
pub fn create_project_dockerfile(
    tree: &RepoTree,
    rel: &RepoRelPath,
    contents: &[u8],
) -> Result<InitFileStatus, RepoTreeError> {
    match tree.create_new(rel, contents) {
        Ok(CreateOutcome::Created) => Ok(InitFileStatus::Created),
        Ok(CreateOutcome::Existing) => Ok(InitFileStatus::Kept),
        Err(e) if e.is_refusal() => Ok(InitFileStatus::Skipped {
            reason: format!("{e}; awman does not create the Dockerfile there"),
        }),
        Err(e) => Err(e),
    }
}

/// A project file's bytes when a run started, to report it created, updated
/// or kept when the run ends — whichever step wrote it, however many times.
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    rel: RepoRelPath,
    kind: InitFileKind,
    before: RepoEntry,
}

impl FileSnapshot {
    /// Read `rel` now, without following links.
    pub fn take(
        tree: &RepoTree,
        rel: RepoRelPath,
        kind: InitFileKind,
    ) -> Result<Self, RepoTreeError> {
        let before = tree.read(&rel)?;
        Ok(Self { rel, kind, before })
    }

    /// The file as an [`InitFile`], comparing it with the snapshot. `None`
    /// when it is (still) absent or is not a regular file.
    pub fn finish(&self, tree: &RepoTree) -> Result<Option<InitFile>, RepoTreeError> {
        let status = match (&self.before, tree.read(&self.rel)?) {
            (_, RepoEntry::Missing | RepoEntry::Other(_)) => return Ok(None),
            (RepoEntry::File(before), RepoEntry::File(after)) if *before == after => {
                InitFileStatus::Kept
            }
            (RepoEntry::File(_), RepoEntry::File(_)) => InitFileStatus::Updated,
            (_, RepoEntry::File(_)) => InitFileStatus::Created,
        };
        Ok(Some(InitFile::new(self.rel.display(), self.kind, status)))
    }
}

/// `path` relative to `git_root` with `/` separators, or `path` verbatim when
/// it is outside.
pub fn display_relative(git_root: &Path, path: &Path) -> String {
    let normalized = normalize(path);
    let relative = normalized
        .strip_prefix(normalize(git_root))
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            std::fs::canonicalize(git_root)
                .ok()
                .and_then(|root| normalized.strip_prefix(root).ok().map(Path::to_path_buf))
        });
    match relative {
        Some(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    fn sandbox(name: &str) -> TestSandbox {
        TestSandbox::for_test(TestCase::component(Box::leak(
            format!("data::init_files::tests::{name}").into_boxed_str(),
        )))
    }

    fn rel(path: &str) -> RepoRelPath {
        RepoRelPath::new(Path::new(path)).unwrap()
    }

    #[test]
    fn create_project_dockerfile_never_overwrites_an_existing_file_even_empty() {
        let sandbox =
            sandbox("create_project_dockerfile_never_overwrites_an_existing_file_even_empty");
        let tree = RepoTree::open(&sandbox.work()).unwrap();
        std::fs::write(sandbox.work().join("Dockerfile.dev"), b"").unwrap();
        assert_eq!(
            create_project_dockerfile(&tree, &rel("Dockerfile.dev"), b"FROM template").unwrap(),
            InitFileStatus::Kept
        );
        assert_eq!(
            std::fs::read(sandbox.work().join("Dockerfile.dev")).unwrap(),
            b""
        );
        assert_eq!(
            create_project_dockerfile(&tree, &rel("docker/dev/Dockerfile"), b"FROM t").unwrap(),
            InitFileStatus::Created
        );
        assert_eq!(
            std::fs::read(sandbox.work().join("docker/dev/Dockerfile")).unwrap(),
            b"FROM t"
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_dockerfile_through_a_link_or_at_a_non_file_is_skipped_with_a_reason() {
        use std::os::unix::fs::symlink;
        let sandbox =
            sandbox("project_dockerfile_through_a_link_or_at_a_non_file_is_skipped_with_a_reason");
        let root = sandbox.work().join("repo");
        let outside = sandbox.work().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("docker")).unwrap();
        symlink(outside.join("gone"), root.join("dangling")).unwrap();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        let tree = RepoTree::open(&root).unwrap();
        for (target, why) in [
            (
                "docker/Project.Dockerfile",
                "symbolic link or not a directory",
            ),
            ("dangling", "is a symbolic link"),
            ("dir", "is a directory"),
        ] {
            let status = create_project_dockerfile(&tree, &rel(target), b"FROM t").unwrap();
            assert!(
                matches!(&status, InitFileStatus::Skipped { reason } if reason.contains(why)),
                "{target}: {status:?}"
            );
        }
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(matches!(
            inspect_project_dockerfile(&root.join("dangling")),
            ExistingDockerfile::Unusable(r) if r.contains("does not exist")
        ));
        assert!(matches!(
            inspect_project_dockerfile(&root.join("dir")),
            ExistingDockerfile::Unusable(r) if r.contains("directory")
        ));
        std::fs::write(root.join("real"), b"").unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        assert_eq!(
            inspect_project_dockerfile(&root.join("alias")),
            ExistingDockerfile::Usable
        );
        assert_eq!(
            inspect_project_dockerfile(&root.join("none")),
            ExistingDockerfile::Missing
        );
    }

    #[test]
    fn snapshots_report_created_updated_and_kept() {
        let sandbox = sandbox("snapshots_report_created_updated_and_kept");
        let tree = RepoTree::open(&sandbox.work()).unwrap();
        std::fs::write(sandbox.work().join("same"), b"x").unwrap();
        std::fs::write(sandbox.work().join("changed"), b"x").unwrap();
        let snaps: Vec<_> = ["same", "changed", "new", "absent"]
            .into_iter()
            .map(|p| FileSnapshot::take(&tree, rel(p), InitFileKind::AspecFile).unwrap())
            .collect();
        std::fs::write(sandbox.work().join("changed"), b"y").unwrap();
        std::fs::write(sandbox.work().join("new"), b"y").unwrap();
        let statuses: Vec<_> = snaps
            .iter()
            .map(|s| {
                s.finish(&tree)
                    .unwrap()
                    .map(|f| (f.path, f.status, f.uncommitted))
            })
            .collect();
        assert_eq!(
            statuses,
            [
                Some(("same".into(), InitFileStatus::Kept, false)),
                Some(("changed".into(), InitFileStatus::Updated, true)),
                Some(("new".into(), InitFileStatus::Created, true)),
                None,
            ]
        );
    }

    #[test]
    fn classify_and_display_paths_against_the_git_root() {
        let sandbox = sandbox("classify_and_display_paths_against_the_git_root");
        let root = sandbox.work();
        let tree = RepoTree::open(&root).unwrap();
        assert_eq!(
            ProjectDockerfileTarget::classify(&tree, &root.join("docker/Dockerfile")),
            ProjectDockerfileTarget::InRepo(rel("docker/Dockerfile"))
        );
        assert!(matches!(
            ProjectDockerfileTarget::classify(&tree, Path::new("/elsewhere/Dockerfile")),
            ProjectDockerfileTarget::OutsideRepo(_)
        ));
        assert!(matches!(
            ProjectDockerfileTarget::classify(&tree, &root.join("../Dockerfile")),
            ProjectDockerfileTarget::OutsideRepo(_)
        ));
        let root = Path::new("/repo");
        assert_eq!(
            display_relative(root, Path::new("/repo/./docker/Dockerfile")),
            "docker/Dockerfile"
        );
        assert_eq!(
            display_relative(root, Path::new("/elsewhere/Dockerfile")),
            "/elsewhere/Dockerfile"
        );
    }

    #[test]
    fn init_file_serializes_status_inline() {
        let file = InitFile::new(
            "Dockerfile.dev".into(),
            InitFileKind::ProjectDockerfile,
            InitFileStatus::Skipped {
                reason: "outside".into(),
            },
        );
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            serde_json::json!({
                "path": "Dockerfile.dev",
                "kind": "project_dockerfile",
                "status": "skipped",
                "reason": "outside",
                "uncommitted": false,
            })
        );
    }
}
