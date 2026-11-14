//! Scratch drafts and private files for work items (Layer 0).
//!
//! A draft is a markdown file in the repo `context(repo)` overlay that the
//! agent edits while awman uploads it to a tracker. The agent controls that
//! directory, so **every** stamp, read and removal here is untrusted:
//!
//! * the directory chain below the (trusted) repo context directory is held as
//!   anchored descriptors, opened one component at a time without following
//!   symlinks;
//! * the file is reopened by its generated basename on every call (editors
//!   that save by rename replace the inode), without following symlinks and
//!   without blocking on a FIFO, then checked on the *opened descriptor*:
//!   regular file, single hard link, size cap, strict UTF-8, unchanged while
//!   being read.
//!
//! Diagnostics never include body bytes. A [`WorkItemDraft`] does not delete
//! itself on drop: a draft that failed to upload must survive for recovery.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tempfile::TempDir;

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;

/// Largest draft or inline work item awman reads, writes or uploads.
pub const MAX_WORK_ITEM_BYTES: u64 = 1_048_576;

/// Host and container locations of one draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemDraftPaths {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
}

/// Cheap change detector: size and modification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftStamp {
    pub len: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone)]
pub struct DraftSnapshot {
    pub stamp: DraftStamp,
    pub content: String,
    /// Full lowercase hex SHA-256 over the exact UTF-8 bytes.
    pub sha256: String,
}

#[derive(Debug)]
pub enum DraftReadError {
    Missing,
    Symlink {
        path: PathBuf,
    },
    NotRegular {
        path: PathBuf,
    },
    UnsafeAncestor {
        path: PathBuf,
    },
    MultipleLinks {
        path: PathBuf,
    },
    TooLarge {
        limit: u64,
    },
    InvalidUtf8,
    ChangedDuringRead,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for DraftReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "the draft file is missing"),
            Self::Symlink { path } => {
                write!(
                    f,
                    "the draft {} is a symlink; refusing to read it",
                    path.display()
                )
            }
            Self::NotRegular { path } => {
                write!(f, "the draft {} is not a regular file", path.display())
            }
            Self::UnsafeAncestor { path } => write!(
                f,
                "a directory above the draft {} was replaced; refusing to read it",
                path.display()
            ),
            Self::MultipleLinks { path } => {
                write!(
                    f,
                    "the draft {} has more than one hard link",
                    path.display()
                )
            }
            Self::TooLarge { limit } => {
                write!(f, "the draft is larger than the {limit}-byte limit")
            }
            Self::InvalidUtf8 => write!(f, "the draft is not valid UTF-8"),
            Self::ChangedDuringRead => write!(f, "the draft changed while it was being read"),
            Self::Io { path, source } => {
                write!(f, "cannot read the draft {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for DraftReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A created draft: anchored directory handles plus the generated filename.
pub struct WorkItemDraft {
    paths: WorkItemDraftPaths,
    name: std::ffi::OsString,
    anchor: imp::Anchor,
}

impl std::fmt::Debug for WorkItemDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkItemDraft")
            .field("paths", &self.paths)
            .finish()
    }
}

impl WorkItemDraft {
    /// Create the draft file seeded with `seed`.
    ///
    /// `paths.host_path` must end in `<repo context dir>/scratch/work-items/<name>`.
    /// The repo context directory is trusted (it sits under the awman home);
    /// `scratch` and `work-items` are created owner-only if missing, never
    /// followed through a symlink, and an existing directory's mode is not
    /// widened. The file is created exclusively with mode 0600.
    pub fn create(paths: WorkItemDraftPaths, seed: &str) -> Result<Self, DataError> {
        if seed.len() as u64 > MAX_WORK_ITEM_BYTES {
            return Err(DataError::InvalidPath {
                path: paths.host_path,
                reason: format!("draft seed exceeds the {MAX_WORK_ITEM_BYTES}-byte limit"),
            });
        }
        let host = paths.host_path.clone();
        let bad = |reason: &str| DataError::InvalidPath {
            path: host.clone(),
            reason: reason.to_string(),
        };
        let name = host
            .file_name()
            .ok_or_else(|| bad("draft path has no file name"))?
            .to_os_string();
        let work_items = host
            .parent()
            .ok_or_else(|| bad("draft path has no parent"))?;
        let scratch = work_items
            .parent()
            .ok_or_else(|| bad("draft path is too shallow"))?;
        let repo_dir = scratch
            .parent()
            .ok_or_else(|| bad("draft path is too shallow"))?;
        if !host.is_absolute()
            || work_items.file_name().and_then(|n| n.to_str()) != Some("work-items")
            || scratch.file_name().and_then(|n| n.to_str()) != Some("scratch")
        {
            return Err(bad(
                "draft path must be <repo context>/scratch/work-items/<name>",
            ));
        }
        let anchor = imp::Anchor::create(repo_dir, work_items)?;
        imp::create_file(&anchor, &name, seed.as_bytes(), &host)?;
        Ok(Self {
            paths,
            name,
            anchor,
        })
    }

    pub fn paths(&self) -> &WorkItemDraftPaths {
        &self.paths
    }

    /// Size and modification time of the draft as it is now.
    pub fn stamp(&self) -> Result<DraftStamp, DraftReadError> {
        let file = self.open_checked()?;
        let meta = file.metadata().map_err(|e| self.io(e))?;
        stamp_of(&meta).map_err(|e| self.io(e))
    }

    /// Delete the draft file (only the anchored basename, never anything the
    /// agent replaced it with outside the scratch directory). A missing file
    /// is success.
    pub fn remove(self) -> Result<(), DataError> {
        imp::remove_file(&self.anchor, &self.name, &self.paths.host_path)
    }

    fn io(&self, source: std::io::Error) -> DraftReadError {
        DraftReadError::Io {
            path: self.paths.host_path.clone(),
            source,
        }
    }

    fn open_checked(&self) -> Result<File, DraftReadError> {
        let host = &self.paths.host_path;
        imp::verify_anchor(&self.anchor, host)?;
        let file = imp::open_file(&self.anchor, &self.name, host)?;
        let meta = file.metadata().map_err(|e| self.io(e))?;
        if !meta.is_file() {
            return Err(DraftReadError::NotRegular { path: host.clone() });
        }
        if imp::link_count(&meta) != 1 {
            return Err(DraftReadError::MultipleLinks { path: host.clone() });
        }
        Ok(file)
    }
}

/// Read the draft once, safely. See the module docs for the checks.
pub fn read_work_item_draft(draft: &WorkItemDraft) -> Result<DraftSnapshot, DraftReadError> {
    let file = draft.open_checked()?;
    let before = file.metadata().map_err(|e| draft.io(e))?;
    if before.len() > MAX_WORK_ITEM_BYTES {
        return Err(DraftReadError::TooLarge {
            limit: MAX_WORK_ITEM_BYTES,
        });
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_WORK_ITEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| draft.io(e))?;
    if bytes.len() as u64 > MAX_WORK_ITEM_BYTES {
        return Err(DraftReadError::TooLarge {
            limit: MAX_WORK_ITEM_BYTES,
        });
    }
    let after = file.metadata().map_err(|e| draft.io(e))?;
    let (stamp_before, stamp_after) = (
        stamp_of(&before).map_err(|e| draft.io(e))?,
        stamp_of(&after).map_err(|e| draft.io(e))?,
    );
    if stamp_before != stamp_after || bytes.len() as u64 != after.len() {
        return Err(DraftReadError::ChangedDuringRead);
    }
    let sha256 = sha256_hex(&bytes);
    let content = String::from_utf8(bytes).map_err(|_| DraftReadError::InvalidUtf8)?;
    Ok(DraftSnapshot {
        stamp: stamp_after,
        content,
        sha256,
    })
}

fn stamp_of(meta: &std::fs::Metadata) -> std::io::Result<DraftStamp> {
    Ok(DraftStamp {
        len: meta.len(),
        modified: meta.modified()?,
    })
}

/// A work item body staged for read-only execution input.
///
/// Lives in its own unpredictable 0700 directory under `parent`, as a 0600
/// file created exclusively, and is deleted on drop.
pub struct PrivateWorkItemFile {
    _dir: TempDir,
    path: PathBuf,
}

impl std::fmt::Debug for PrivateWorkItemFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateWorkItemFile")
            .field("path", &self.path)
            .finish()
    }
}

impl PrivateWorkItemFile {
    pub fn create(parent: &Path, content: &str) -> Result<Self, DataError> {
        use std::io::Write;
        if content.len() as u64 > MAX_WORK_ITEM_BYTES {
            return Err(DataError::InvalidPath {
                path: parent.to_path_buf(),
                reason: format!("work item exceeds the {MAX_WORK_ITEM_BYTES}-byte limit"),
            });
        }
        crate::data::fs::private_files::create_private_dir(parent)?;
        let dir = tempfile::Builder::new()
            .prefix("work-item-")
            .tempdir_in(parent)
            .map_err(|e| DataError::io(parent, e))?;
        // The temporary directory honours the umask; make it owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(|e| DataError::io(dir.path(), e))?;
        }
        let path = dir.path().join("work-item.md");
        let mut file = crate::data::fs::private_files::create_private_file_new(&path)?;
        file.write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| DataError::io(&path, e))?;
        Ok(Self { _dir: dir, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use nix::errno::Errno;
    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::{mkdirat, Mode};
    use std::ffi::OsStr;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    /// The `work-items` directory held open, plus its identity for the
    /// "path still names this directory" check.
    pub struct Anchor {
        dir: File,
        dev: u64,
        ino: u64,
        path: PathBuf,
    }

    fn dir_flags() -> OFlag {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    }

    fn unsafe_dir(path: &Path, source: Errno) -> DataError {
        match source {
            Errno::ELOOP | Errno::ENOTDIR => DataError::InvalidPath {
                path: path.to_path_buf(),
                reason: "must be a real directory, not a symlink".into(),
            },
            e => DataError::io(path, e.into()),
        }
    }

    impl Anchor {
        pub fn create(repo_dir: &Path, work_items: &Path) -> Result<Self, DataError> {
            // The repo context directory is trusted; make it if the repo has
            // none yet, owner-only, without touching an existing one's mode.
            crate::data::fs::path_guard::ensure_not_symlinked(repo_dir)?;
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.recursive(true);
            builder
                .create(repo_dir)
                .map_err(|e| DataError::io(repo_dir, e))?;
            crate::data::fs::path_guard::ensure_not_symlinked(repo_dir)?;
            let mut dir: File = openat(nix::fcntl::AT_FDCWD, repo_dir, dir_flags(), Mode::empty())
                .map_err(|e| unsafe_dir(repo_dir, e))?
                .into();
            let mut path = repo_dir.to_path_buf();
            for component in ["scratch", "work-items"] {
                path.push(component);
                match mkdirat(&dir, Path::new(component), Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(e) => return Err(DataError::io(&path, e.into())),
                }
                dir = openat(&dir, Path::new(component), dir_flags(), Mode::empty())
                    .map_err(|e| unsafe_dir(&path, e))?
                    .into();
            }
            debug_assert_eq!(path, work_items);
            let meta = dir.metadata().map_err(|e| DataError::io(&path, e))?;
            if meta.uid() != nix::unistd::geteuid().as_raw() {
                return Err(DataError::InvalidPath {
                    path,
                    reason: "is owned by another user".into(),
                });
            }
            // Drafts hold private issue text, and an editor's rename-save can
            // leave one world-readable: keep the drafts directory owner-only
            // even when an older awman created it with a wider mode.
            if meta.mode() & 0o077 != 0 {
                nix::sys::stat::fchmod(&dir, Mode::from_bits_truncate(0o700))
                    .map_err(|e| DataError::io(&path, e.into()))?;
            }
            Ok(Self {
                dir,
                dev: meta.dev(),
                ino: meta.ino(),
                path,
            })
        }
    }

    /// The path must still name the directory we hold; otherwise an ancestor
    /// was renamed or replaced.
    pub fn verify_anchor(anchor: &Anchor, host: &Path) -> Result<(), DraftReadError> {
        match std::fs::symlink_metadata(&anchor.path) {
            Ok(m) if m.is_dir() && m.dev() == anchor.dev && m.ino() == anchor.ino => Ok(()),
            _ => Err(DraftReadError::UnsafeAncestor {
                path: host.to_path_buf(),
            }),
        }
    }

    pub fn create_file(
        anchor: &Anchor,
        name: &OsStr,
        seed: &[u8],
        host: &Path,
    ) -> Result<(), DataError> {
        let fd = openat(
            &anchor.dir,
            Path::new(name),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| DataError::io(host, e.into()))?;
        let mut file = File::from(fd);
        file.write_all(seed)
            .and_then(|()| file.sync_all())
            .map_err(|e| DataError::io(host, e))
    }

    pub fn open_file(anchor: &Anchor, name: &OsStr, host: &Path) -> Result<File, DraftReadError> {
        openat(
            &anchor.dir,
            Path::new(name),
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|e| match e {
            Errno::ENOENT => DraftReadError::Missing,
            Errno::ELOOP => DraftReadError::Symlink {
                path: host.to_path_buf(),
            },
            e => DraftReadError::Io {
                path: host.to_path_buf(),
                source: e.into(),
            },
        })
    }

    pub fn remove_file(anchor: &Anchor, name: &OsStr, host: &Path) -> Result<(), DataError> {
        match nix::unistd::unlinkat(
            &anchor.dir,
            Path::new(name),
            nix::unistd::UnlinkatFlags::NoRemoveDir,
        ) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(e) => Err(DataError::io(host, e.into())),
        }
    }

    pub fn link_count(meta: &std::fs::Metadata) -> u64 {
        meta.nlink()
    }
}

/// Best-effort equivalent for platforms without descriptor-relative opens
/// (Windows): reparse points are refused at every component and on the file.
#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::ffi::OsStr;
    use std::io::Write;

    pub struct Anchor {
        dir: PathBuf,
    }

    fn is_link(meta: &std::fs::Metadata) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x400 != 0 || meta.file_type().is_symlink()
        }
        #[cfg(not(windows))]
        {
            meta.file_type().is_symlink()
        }
    }

    fn check_dir(path: &Path) -> Result<(), DataError> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| DataError::io(path, e))?;
        if is_link(&meta) || !meta.is_dir() {
            return Err(DataError::InvalidPath {
                path: path.to_path_buf(),
                reason: "must be a real directory, not a symlink or reparse point".into(),
            });
        }
        Ok(())
    }

    impl Anchor {
        pub fn create(repo_dir: &Path, work_items: &Path) -> Result<Self, DataError> {
            std::fs::create_dir_all(repo_dir).map_err(|e| DataError::io(repo_dir, e))?;
            check_dir(repo_dir)?;
            let mut path = repo_dir.to_path_buf();
            for component in ["scratch", "work-items"] {
                path.push(component);
                match std::fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(DataError::io(&path, e)),
                }
                check_dir(&path)?;
            }
            Ok(Self {
                dir: work_items.to_path_buf(),
            })
        }
    }

    pub fn verify_anchor(anchor: &Anchor, host: &Path) -> Result<(), DraftReadError> {
        let mut path = PathBuf::new();
        let mut checking = false;
        for component in anchor.dir.components() {
            path.push(component);
            checking |= component.as_os_str() == "scratch";
            if checking && check_dir(&path).is_err() {
                return Err(DraftReadError::UnsafeAncestor {
                    path: host.to_path_buf(),
                });
            }
        }
        Ok(())
    }

    pub fn create_file(
        anchor: &Anchor,
        name: &OsStr,
        seed: &[u8],
        host: &Path,
    ) -> Result<(), DataError> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(anchor.dir.join(name))
            .map_err(|e| DataError::io(host, e))?;
        file.write_all(seed)
            .and_then(|()| file.sync_all())
            .map_err(|e| DataError::io(host, e))
    }

    pub fn open_file(anchor: &Anchor, name: &OsStr, host: &Path) -> Result<File, DraftReadError> {
        let path = anchor.dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if is_link(&m) => {
                return Err(DraftReadError::Symlink {
                    path: host.to_path_buf(),
                })
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DraftReadError::Missing)
            }
            Err(e) => {
                return Err(DraftReadError::Io {
                    path: host.to_path_buf(),
                    source: e,
                })
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        options.open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DraftReadError::Missing,
            _ => DraftReadError::Io {
                path: host.to_path_buf(),
                source: e,
            },
        })
    }

    pub fn remove_file(anchor: &Anchor, name: &OsStr, host: &Path) -> Result<(), DataError> {
        match std::fs::remove_file(anchor.dir.join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(host, e)),
        }
    }

    pub fn link_count(_meta: &std::fs::Metadata) -> u64 {
        1
    }
}
