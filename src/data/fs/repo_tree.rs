//! Root-anchored file writes inside a git checkout (Layer 0).
//!
//! Project setup (`init`, session setup's `ready`, agent Dockerfile
//! publication, `aspec/` template extraction) writes into a repository that
//! its owner — or an agent that ran in it — controls. A lexical "is this path
//! under the git root?" check is not enough there: a committed
//! `docker -> /somewhere/else` symlink, or one swapped in between the check
//! and the write, would carry the write outside the checkout.
//!
//! [`RepoTree`] therefore walks every path from the git root one component
//! at a time with descriptor-relative opens (`openat`/`mkdirat`/`renameat`)
//! and `O_NOFOLLOW`: no ancestor directory may be a symbolic link, missing
//! ancestors are created as real directories, and the final file is created
//! exclusively or replaced by an atomic rename — never written through a
//! link. Reads that feed a write decision use the same walk.
//!
//! On platforms without the descriptor-relative calls the walk checks each
//! component with `symlink_metadata` instead; that refuses committed links
//! but cannot exclude a concurrent swap.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Why a [`RepoTree`] operation refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum RepoTreeError {
    /// An ancestor of the target is a symbolic link or not a directory.
    #[error("{path} is a symbolic link or not a directory; awman does not write through it")]
    Unsafe { path: String },
    /// The target exists but is not a regular file.
    #[error("{path} is a {kind}, not a regular file")]
    NotAFile { path: String, kind: &'static str },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

impl RepoTreeError {
    /// A refusal (link, wrong file type) rather than an I/O failure: the
    /// caller can report it as a skip with this error's text as the reason.
    pub fn is_refusal(&self) -> bool {
        !matches!(self, Self::Io { .. })
    }
}

/// A path below a [`RepoTree`] root: one or more normal components, no `.`,
/// `..`, root or prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRelPath(Vec<OsString>);

impl RepoRelPath {
    /// Parse a relative path. `None` for an empty path or any component that
    /// is not a plain name.
    pub fn new(path: &Path) -> Option<Self> {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => parts.push(name.to_os_string()),
                Component::CurDir => {}
                _ => return None,
            }
        }
        (!parts.is_empty()).then_some(Self(parts))
    }

    /// The `/`-separated form, as reported to clients.
    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// This path with `child` appended.
    pub fn join(&self, child: &RepoRelPath) -> Self {
        let mut parts = self.0.clone();
        parts.extend(child.0.iter().cloned());
        Self(parts)
    }

    fn prefix(&self, len: usize) -> String {
        Self(self.0[..len].to_vec()).display()
    }

    fn name(&self) -> &OsString {
        self.0.last().expect("RepoRelPath is never empty")
    }

    fn parents(&self) -> &[OsString] {
        &self.0[..self.0.len() - 1]
    }
}

/// What is at a path below the root, read without following a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoEntry {
    Missing,
    File(Vec<u8>),
    /// A symbolic link, directory or special file.
    Other(&'static str),
}

/// The result of [`RepoTree::create_new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    Created,
    /// A regular file was already there; it was left untouched.
    Existing,
}

/// The result of [`RepoTree::write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Updated,
    /// The file already held exactly these bytes; nothing was written.
    Unchanged,
}

/// A git checkout that files are written into, anchored at its root.
#[derive(Debug, Clone)]
pub struct RepoTree {
    /// The root as the caller named it, lexically normalised.
    given: PathBuf,
    /// The root with every link resolved: where descriptor walks start.
    root: PathBuf,
}

impl RepoTree {
    /// Anchor at `git_root`, which must exist.
    pub fn open(git_root: &Path) -> Result<Self, RepoTreeError> {
        let root = std::fs::canonicalize(git_root).map_err(|source| RepoTreeError::Io {
            path: git_root.display().to_string(),
            source,
        })?;
        Ok(Self {
            given: normalize(git_root),
            root,
        })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` (absolute, or relative to the root) as a path below the root,
    /// compared lexically after resolving `.` and `..`. `None` when it is
    /// outside the root or is the root itself. Links are not resolved here;
    /// every operation refuses to traverse one.
    pub fn relative(&self, path: &Path) -> Option<RepoRelPath> {
        let absolute = if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.given.join(path))
        };
        let rest = absolute
            .strip_prefix(&self.given)
            .or_else(|_| absolute.strip_prefix(&self.root))
            .ok()?;
        RepoRelPath::new(rest)
    }

    /// The absolute path of `rel` under the canonical root (for messages and
    /// for callers that hand the path to another tool).
    pub fn path_of(&self, rel: &RepoRelPath) -> PathBuf {
        rel.0.iter().fold(self.root.clone(), |p, c| p.join(c))
    }

    /// What is at `rel`. A missing ancestor reads as [`RepoEntry::Missing`];
    /// a linked ancestor is refused.
    pub fn read(&self, rel: &RepoRelPath) -> Result<RepoEntry, RepoTreeError> {
        imp::read(self, rel)
    }

    /// Create `rel` with `contents` unless something is already there,
    /// creating missing parent directories. An existing regular file (even
    /// empty) is kept byte-for-byte; anything else there is refused.
    pub fn create_new(
        &self,
        rel: &RepoRelPath,
        contents: &[u8],
    ) -> Result<CreateOutcome, RepoTreeError> {
        imp::create_new(self, rel, contents)
    }

    /// Make `rel` a regular file holding `contents`, creating missing parent
    /// directories. An existing file is replaced atomically (write a sibling,
    /// rename over it); a directory, link or special file there is refused.
    pub fn write(&self, rel: &RepoRelPath, contents: &[u8]) -> Result<WriteOutcome, RepoTreeError> {
        imp::write(self, rel, contents)
    }

    /// Make `rel` and its ancestors real directories.
    pub fn create_dir_all(&self, rel: &RepoRelPath) -> Result<(), RepoTreeError> {
        imp::create_dir_all(self, rel)
    }
}

/// Lexically resolve `.` and `..` (no filesystem access).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn io_err(path: String) -> impl FnOnce(io::Error) -> RepoTreeError {
    move |source| RepoTreeError::Io { path, source }
}

/// A unique sibling name for an atomic replace.
fn temp_name(name: &OsString) -> OsString {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut tmp = OsString::from(".");
    tmp.push(name);
    tmp.push(format!(
        ".awman-tmp-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    tmp
}

#[cfg(unix)]
mod imp {
    use super::*;
    use nix::errno::Errno;
    use nix::fcntl::{openat, renameat, OFlag, AT_FDCWD};
    use nix::sys::stat::{fstat, mkdirat, Mode, SFlag};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;

    const DIR: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_CLOEXEC)
        .union(OFlag::O_DIRECTORY);

    fn errno(path: String) -> impl FnOnce(Errno) -> RepoTreeError {
        move |e| RepoTreeError::Io {
            path,
            source: io::Error::from(e),
        }
    }

    /// Open the directory holding `rel`'s final component. `Ok(None)` when an
    /// ancestor is missing and `create` is false.
    fn open_parent(
        tree: &RepoTree,
        rel: &RepoRelPath,
        create: bool,
    ) -> Result<Option<OwnedFd>, RepoTreeError> {
        let mut dir = openat(AT_FDCWD, tree.root.as_path(), DIR, Mode::empty())
            .map_err(errno(tree.root.display().to_string()))?;
        for (i, name) in rel.parents().iter().enumerate() {
            let shown = || rel.prefix(i + 1);
            let name = Path::new(name);
            let next = match openat(&dir, name, DIR | OFlag::O_NOFOLLOW, Mode::empty()) {
                Err(Errno::ENOENT) if create => {
                    match mkdirat(&dir, name, Mode::from_bits_truncate(0o777)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(e) => return Err(errno(shown())(e)),
                    }
                    // Reopen without following: a link swapped in after the
                    // mkdir is refused like any other.
                    openat(&dir, name, DIR | OFlag::O_NOFOLLOW, Mode::empty())
                }
                other => other,
            };
            dir = match next {
                Ok(fd) => fd,
                Err(Errno::ENOENT) => return Ok(None),
                Err(Errno::ELOOP | Errno::ENOTDIR) => {
                    return Err(RepoTreeError::Unsafe { path: shown() })
                }
                Err(e) => return Err(errno(shown())(e)),
            };
        }
        Ok(Some(dir))
    }

    fn kind_of(file_type: SFlag) -> &'static str {
        match file_type {
            SFlag::S_IFDIR => "directory",
            SFlag::S_IFLNK => "symbolic link",
            _ => "special file",
        }
    }

    fn read_at(dir: &OwnedFd, rel: &RepoRelPath) -> Result<RepoEntry, RepoTreeError> {
        let shown = rel.display();
        let fd = match openat(
            dir,
            Path::new(rel.name()),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(RepoEntry::Missing),
            Err(Errno::ELOOP) => return Ok(RepoEntry::Other("symbolic link")),
            Err(e) => return Err(errno(shown)(e)),
        };
        let stat = fstat(&fd).map_err(errno(shown.clone()))?;
        // `st_mode` is `mode_t`, whose width differs by platform.
        let file_type = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
        if file_type != SFlag::S_IFREG {
            return Ok(RepoEntry::Other(kind_of(file_type)));
        }
        let mut bytes = Vec::new();
        std::fs::File::from(fd)
            .read_to_end(&mut bytes)
            .map_err(io_err(shown))?;
        Ok(RepoEntry::File(bytes))
    }

    pub(super) fn read(tree: &RepoTree, rel: &RepoRelPath) -> Result<RepoEntry, RepoTreeError> {
        match open_parent(tree, rel, false)? {
            Some(dir) => read_at(&dir, rel),
            None => Ok(RepoEntry::Missing),
        }
    }

    fn create_at(
        dir: &OwnedFd,
        name: &OsString,
        contents: &[u8],
        shown: &str,
    ) -> Result<Option<()>, RepoTreeError> {
        let fd = match openat(
            dir,
            Path::new(name),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o666),
        ) {
            Ok(fd) => fd,
            Err(Errno::EEXIST) => return Ok(None),
            Err(e) => return Err(errno(shown.to_string())(e)),
        };
        let mut file = std::fs::File::from(fd);
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(io_err(shown.to_string()))?;
        Ok(Some(()))
    }

    pub(super) fn create_new(
        tree: &RepoTree,
        rel: &RepoRelPath,
        contents: &[u8],
    ) -> Result<CreateOutcome, RepoTreeError> {
        let dir = open_parent(tree, rel, true)?.expect("create walks every ancestor");
        let shown = rel.display();
        if create_at(&dir, rel.name(), contents, &shown)?.is_some() {
            return Ok(CreateOutcome::Created);
        }
        match read_at(&dir, rel)? {
            RepoEntry::File(_) => Ok(CreateOutcome::Existing),
            RepoEntry::Other(kind) => Err(RepoTreeError::NotAFile { path: shown, kind }),
            // Removed again between the two calls: report the race rather
            // than retrying into it.
            RepoEntry::Missing => Err(RepoTreeError::Io {
                path: shown,
                source: io::Error::new(io::ErrorKind::Interrupted, "file changed during creation"),
            }),
        }
    }

    pub(super) fn write(
        tree: &RepoTree,
        rel: &RepoRelPath,
        contents: &[u8],
    ) -> Result<WriteOutcome, RepoTreeError> {
        let dir = open_parent(tree, rel, true)?.expect("write walks every ancestor");
        let shown = rel.display();
        let outcome = match read_at(&dir, rel)? {
            RepoEntry::File(existing) if existing == contents => {
                return Ok(WriteOutcome::Unchanged)
            }
            RepoEntry::File(_) => WriteOutcome::Updated,
            RepoEntry::Missing => WriteOutcome::Created,
            RepoEntry::Other(kind) => return Err(RepoTreeError::NotAFile { path: shown, kind }),
        };
        let tmp = temp_name(rel.name());
        if create_at(&dir, &tmp, contents, &shown)?.is_none() {
            return Err(RepoTreeError::Io {
                path: shown,
                source: io::Error::new(io::ErrorKind::AlreadyExists, "temporary name in use"),
            });
        }
        // rename(2) replaces the final name itself, never what it links to.
        if let Err(e) = renameat(&dir, Path::new(&tmp), &dir, Path::new(rel.name())) {
            let _ = unlinkat(&dir, Path::new(&tmp), UnlinkatFlags::NoRemoveDir);
            return Err(errno(shown)(e));
        }
        Ok(outcome)
    }

    pub(super) fn create_dir_all(tree: &RepoTree, rel: &RepoRelPath) -> Result<(), RepoTreeError> {
        // Walk `rel` as if it were the parent of one more component.
        let mut deeper = rel.0.clone();
        deeper.push(OsString::from("."));
        open_parent(tree, &RepoRelPath(deeper), true)?;
        Ok(())
    }
}

#[cfg(not(unix))]
mod imp {
    //! Check-based walk: refuses committed links but cannot exclude a
    //! concurrent swap (see the module documentation).
    use super::*;
    use std::io::Write;

    fn kind_of(meta: &std::fs::Metadata) -> &'static str {
        if meta.file_type().is_symlink() {
            "symbolic link"
        } else if meta.is_dir() {
            "directory"
        } else {
            "special file"
        }
    }

    fn walk_parents(
        tree: &RepoTree,
        rel: &RepoRelPath,
        create: bool,
    ) -> Result<Option<PathBuf>, RepoTreeError> {
        let mut dir = tree.root.clone();
        for (i, name) in rel.parents().iter().enumerate() {
            dir.push(name);
            match std::fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(RepoTreeError::Unsafe {
                        path: rel.prefix(i + 1),
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    match std::fs::create_dir(&dir) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(io_err(rel.prefix(i + 1))(e)),
                    }
                    let meta =
                        std::fs::symlink_metadata(&dir).map_err(io_err(rel.prefix(i + 1)))?;
                    if !meta.is_dir() || meta.file_type().is_symlink() {
                        return Err(RepoTreeError::Unsafe {
                            path: rel.prefix(i + 1),
                        });
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io_err(rel.prefix(i + 1))(e)),
            }
        }
        Ok(Some(dir))
    }

    fn read_at(dir: &Path, rel: &RepoRelPath) -> Result<RepoEntry, RepoTreeError> {
        let path = dir.join(rel.name());
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => std::fs::read(&path)
                .map(RepoEntry::File)
                .map_err(io_err(rel.display())),
            Ok(meta) => Ok(RepoEntry::Other(kind_of(&meta))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RepoEntry::Missing),
            Err(e) => Err(io_err(rel.display())(e)),
        }
    }

    pub(super) fn read(tree: &RepoTree, rel: &RepoRelPath) -> Result<RepoEntry, RepoTreeError> {
        match walk_parents(tree, rel, false)? {
            Some(dir) => read_at(&dir, rel),
            None => Ok(RepoEntry::Missing),
        }
    }

    pub(super) fn create_new(
        tree: &RepoTree,
        rel: &RepoRelPath,
        contents: &[u8],
    ) -> Result<CreateOutcome, RepoTreeError> {
        let dir = walk_parents(tree, rel, true)?.expect("create walks every ancestor");
        let shown = rel.display();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(rel.name()))
        {
            Ok(mut file) => {
                file.write_all(contents)
                    .and_then(|()| file.sync_all())
                    .map_err(io_err(shown))?;
                Ok(CreateOutcome::Created)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match read_at(&dir, rel)? {
                RepoEntry::File(_) => Ok(CreateOutcome::Existing),
                RepoEntry::Other(kind) => Err(RepoTreeError::NotAFile { path: shown, kind }),
                RepoEntry::Missing => Err(io_err(shown)(e)),
            },
            Err(e) => Err(io_err(shown)(e)),
        }
    }

    pub(super) fn write(
        tree: &RepoTree,
        rel: &RepoRelPath,
        contents: &[u8],
    ) -> Result<WriteOutcome, RepoTreeError> {
        let dir = walk_parents(tree, rel, true)?.expect("write walks every ancestor");
        let shown = rel.display();
        let outcome = match read_at(&dir, rel)? {
            RepoEntry::File(existing) if existing == contents => {
                return Ok(WriteOutcome::Unchanged)
            }
            RepoEntry::File(_) => WriteOutcome::Updated,
            RepoEntry::Missing => WriteOutcome::Created,
            RepoEntry::Other(kind) => return Err(RepoTreeError::NotAFile { path: shown, kind }),
        };
        let tmp = dir.join(temp_name(rel.name()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(io_err(shown.clone()))?;
        let written = file.write_all(contents).and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, dir.join(rel.name()))) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err(shown)(e));
        }
        Ok(outcome)
    }

    pub(super) fn create_dir_all(tree: &RepoTree, rel: &RepoRelPath) -> Result<(), RepoTreeError> {
        let mut deeper = rel.0.clone();
        deeper.push(OsString::from("."));
        walk_parents(tree, &RepoRelPath(deeper), true)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    fn sandbox(name: &str) -> TestSandbox {
        TestSandbox::for_test(TestCase::component(Box::leak(
            format!("data::fs::repo_tree::tests::{name}").into_boxed_str(),
        )))
    }

    fn rel(path: &str) -> RepoRelPath {
        RepoRelPath::new(Path::new(path)).unwrap()
    }

    /// A repo and a sibling directory outside it.
    fn layout(s: &TestSandbox) -> (RepoTree, PathBuf, PathBuf) {
        let root = s.work().join("repo");
        let outside = s.work().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (RepoTree::open(&root).unwrap(), root, outside)
    }

    #[test]
    fn relative_accepts_paths_below_the_root_only() {
        let s = sandbox("relative_accepts_paths_below_the_root_only");
        let (tree, root, _) = layout(&s);
        assert_eq!(
            tree.relative(Path::new("docker/./Dockerfile")),
            Some(rel("docker/Dockerfile"))
        );
        assert_eq!(tree.relative(&root.join("a/b")), Some(rel("a/b")));
        assert_eq!(tree.relative(&root.join("a/../b")), Some(rel("b")));
        assert_eq!(tree.relative(&root.join("../outside/x")), None);
        assert_eq!(tree.relative(Path::new("../x")), None);
        assert_eq!(tree.relative(&root), None);
        assert_eq!(tree.relative(Path::new("/elsewhere/x")), None);
    }

    #[test]
    fn create_new_creates_parents_and_never_overwrites() {
        let s = sandbox("create_new_creates_parents_and_never_overwrites");
        let (tree, root, _) = layout(&s);
        let target = rel("docker/dev/Dockerfile");
        assert_eq!(
            tree.create_new(&target, b"FROM a").unwrap(),
            CreateOutcome::Created
        );
        assert_eq!(
            std::fs::read(root.join("docker/dev/Dockerfile")).unwrap(),
            b"FROM a"
        );
        assert_eq!(
            tree.create_new(&target, b"FROM b").unwrap(),
            CreateOutcome::Existing
        );
        std::fs::write(root.join("empty"), b"").unwrap();
        assert_eq!(
            tree.create_new(&rel("empty"), b"x").unwrap(),
            CreateOutcome::Existing
        );
        assert_eq!(std::fs::read(root.join("empty")).unwrap(), b"");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestors_are_refused_for_every_operation() {
        use std::os::unix::fs::symlink;
        let s = sandbox("symlinked_ancestors_are_refused_for_every_operation");
        let (tree, root, outside) = layout(&s);
        symlink(&outside, root.join("docker")).unwrap();
        // A link that stays inside the repo is refused too: the walk never
        // follows links, wherever they point.
        std::fs::create_dir_all(root.join("real")).unwrap();
        symlink(root.join("real"), root.join("inner")).unwrap();
        for target in [
            "docker/Project.Dockerfile",
            "docker/new/Dockerfile",
            "inner/x",
        ] {
            let target = rel(target);
            for result in [
                tree.create_new(&target, b"x").map(|_| ()),
                tree.write(&target, b"x").map(|_| ()),
                tree.read(&target).map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(RepoTreeError::Unsafe { .. })),
                    "{result:?}"
                );
            }
        }
        assert!(matches!(
            tree.create_dir_all(&rel("docker/sub")),
            Err(RepoTreeError::Unsafe { .. })
        ));
        // Symlink plus `..`: lexically inside, physically outside.
        assert!(tree.relative(Path::new("docker/../escape")).is_some());
        assert!(matches!(
            tree.create_new(&rel("docker/x"), b"x"),
            Err(RepoTreeError::Unsafe { .. })
        ));
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing written outside"
        );
        assert_eq!(std::fs::read_dir(root.join("real")).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn final_links_directories_and_dangling_links_are_not_files() {
        use std::os::unix::fs::symlink;
        let s = sandbox("final_links_directories_and_dangling_links_are_not_files");
        let (tree, root, outside) = layout(&s);
        symlink(outside.join("not-created"), root.join("dangling")).unwrap();
        std::fs::write(outside.join("target"), b"outside").unwrap();
        symlink(outside.join("target"), root.join("link")).unwrap();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        for (name, kind) in [
            ("dangling", "symbolic link"),
            ("link", "symbolic link"),
            ("dir", "directory"),
        ] {
            assert_eq!(tree.read(&rel(name)).unwrap(), RepoEntry::Other(kind));
            for result in [
                tree.create_new(&rel(name), b"x").map(|_| ()),
                tree.write(&rel(name), b"x").map(|_| ()),
            ] {
                assert!(
                    matches!(&result, Err(RepoTreeError::NotAFile { kind: k, .. }) if *k == kind),
                    "{name}: {result:?}"
                );
            }
        }
        assert!(!outside.join("not-created").exists());
        assert_eq!(std::fs::read(outside.join("target")).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn an_ancestor_swapped_for_a_link_after_classification_is_refused() {
        use std::os::unix::fs::symlink;
        let s = sandbox("an_ancestor_swapped_for_a_link_after_classification_is_refused");
        let (tree, root, outside) = layout(&s);
        std::fs::create_dir_all(root.join("docker")).unwrap();
        // Classified while `docker/` is a real directory...
        let target = tree.relative(&root.join("docker/Dockerfile")).unwrap();
        // ...then replaced by a link before the write.
        std::fs::remove_dir(root.join("docker")).unwrap();
        symlink(&outside, root.join("docker")).unwrap();
        assert!(matches!(
            tree.create_new(&target, b"x"),
            Err(RepoTreeError::Unsafe { .. })
        ));
        assert!(!outside.join("Dockerfile").exists());
    }

    #[test]
    fn write_replaces_atomically_and_reports_what_changed() {
        let s = sandbox("write_replaces_atomically_and_reports_what_changed");
        let (tree, root, _) = layout(&s);
        let target = rel(".awman/Dockerfile.claude");
        assert_eq!(tree.write(&target, b"one").unwrap(), WriteOutcome::Created);
        assert_eq!(
            tree.write(&target, b"one").unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(tree.write(&target, b"two").unwrap(), WriteOutcome::Updated);
        assert_eq!(
            std::fs::read(root.join(".awman/Dockerfile.claude")).unwrap(),
            b"two"
        );
        let names: Vec<_> = std::fs::read_dir(root.join(".awman"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            ["Dockerfile.claude"],
            "no temporary file left behind"
        );
        assert_eq!(
            tree.read(&target).unwrap(),
            RepoEntry::File(b"two".to_vec())
        );
        assert_eq!(tree.read(&rel("missing/x")).unwrap(), RepoEntry::Missing);
    }
}
