//! No-follow regular-file traversal shared by outputs and context.
use crate::data::context::{ContextFileMeta, ContextRelPath};
use std::io::Read;
use std::path::{Path, PathBuf};
pub(crate) struct WalkBudget<'a> {
    cancelled: &'a std::sync::atomic::AtomicBool,
    pub(crate) deadline: std::time::Instant,
    entries: usize,
    max_entries: usize,
}
impl<'a> WalkBudget<'a> {
    pub(crate) fn new(cancelled: &'a std::sync::atomic::AtomicBool) -> Self {
        Self {
            cancelled,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
            entries: 0,
            max_entries: 100_000,
        }
    }
    fn entry(&mut self) -> std::io::Result<()> {
        self.entries += 1;
        if self.entries > self.max_entries
            || self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
            || std::time::Instant::now() > self.deadline
        {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }
}

pub(crate) fn sha256_hex(
    file: &mut std::fs::File,
    size: u64,
    cancelled: &std::sync::atomic::AtomicBool,
    deadline: std::time::Instant,
) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut remaining = size;
    let mut buffer = vec![0u8; 64 * 1024];
    while remaining > 0 {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed)
            || std::time::Instant::now() > deadline
        {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        let limit = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(unix)]
pub(crate) mod platform {
    use std::io;
    #[cfg(target_os = "linux")]
    use std::os::fd::AsRawFd;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use nix::fcntl::{open, openat, AtFlags, OFlag};
    #[cfg(target_os = "linux")]
    use nix::sys::stat::fstat;
    use nix::sys::stat::{fstatat, Mode, SFlag};

    fn dir_flags() -> OFlag {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    }

    fn open_dir_at(dir: &OwnedFd, name: &std::ffi::OsStr) -> io::Result<OwnedFd> {
        Ok(openat(dir, name, dir_flags(), Mode::empty())?)
    }

    /// Recheck root ancestors through held descriptors after resolution.
    pub(crate) fn context_root(root: &Path, create: bool) -> io::Result<OwnedFd> {
        use std::path::Component;
        let start = if root.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        };
        let mut dir = open(start, dir_flags(), Mode::empty())?;
        for part in root.components() {
            match part {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => {
                    if create {
                        match nix::sys::stat::mkdirat(&dir, name, Mode::from_bits_truncate(0o700)) {
                            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                    dir = open_dir_at(&dir, name)?;
                }
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            }
        }
        Ok(dir)
    }

    pub(crate) fn open_context(root: &Path, segments: &[&str]) -> io::Result<std::fs::File> {
        let (last, parents) = segments.split_last().ok_or(io::ErrorKind::NotFound)?;
        let mut dir = context_root(root, false)?;
        for segment in parents {
            dir = open_dir_at(&dir, std::ffi::OsStr::new(segment))?;
        }
        open_file_at(
            &dir,
            std::ffi::OsStr::new(last),
            root.parent().ok_or(io::ErrorKind::NotFound)?,
        )
    }

    pub(crate) fn walk_context(
        root: &Path,
        budget: &mut super::WalkBudget<'_>,
        visitor: &mut impl FnMut(&Path, std::fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let dir = match context_root(root, false) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        walk_dir(
            dir,
            PathBuf::new(),
            budget,
            visitor,
            root.parent().ok_or(io::ErrorKind::NotFound)?,
        )
    }

    /// Linux O_PATH pins the inode without invoking a device's open handler.
    /// Reopening the pinned regular inode through procfs cannot follow a name
    /// exchanged by the agent. O_NOFOLLOW on the initial handle rejects links.
    #[cfg(target_os = "linux")]
    fn open_file_at(
        dir: &OwnedFd,
        name: &std::ffi::OsStr,
        _staging: &Path,
    ) -> io::Result<std::fs::File> {
        let pinned = openat(
            dir,
            name,
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        reopen_regular(pinned)
    }

    #[cfg(target_os = "linux")]
    fn reopen_regular(pinned: OwnedFd) -> io::Result<std::fs::File> {
        let stat = fstat(&pinned)?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
            return Err(io::ErrorKind::NotFound.into());
        }
        std::fs::File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
    }

    #[cfg(not(target_os = "linux"))]
    fn open_file_at(
        dir: &OwnedFd,
        name: &std::ffi::OsStr,
        staging: &Path,
    ) -> io::Result<std::fs::File> {
        open_linked_regular_at(dir, name, staging)
    }

    #[cfg(not(target_os = "linux"))]
    fn open_linked_regular_at(
        dir: &OwnedFd,
        name: &std::ffi::OsStr,
        staging: &Path,
    ) -> io::Result<std::fs::File> {
        // Pin the named inode in a private, unmounted sibling directory on
        // the same filesystem. linkat does not open a device or follow links.
        // The agent cannot exchange the private name between stat and open.
        let lease = tempfile::Builder::new()
            .prefix(".output-read-")
            .tempdir_in(staging)?;
        let pinned_dir = open(lease.path(), dir_flags(), Mode::empty())?;
        nix::unistd::linkat(dir, name, &pinned_dir, "file", AtFlags::empty())?;
        let stat = fstatat(&pinned_dir, "file", AtFlags::AT_SYMLINK_NOFOLLOW)?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(std::fs::File::from(openat(
            &pinned_dir,
            "file",
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?))
    }

    pub(crate) fn open_beneath(root: &Path, segments: &[&str]) -> io::Result<std::fs::File> {
        let (last, parents) = segments.split_last().ok_or(io::ErrorKind::NotFound)?;
        let mut dir = open(root, dir_flags(), Mode::empty())?;
        for segment in parents {
            dir = open_dir_at(&dir, std::ffi::OsStr::new(segment))?;
        }
        open_file_at(
            &dir,
            std::ffi::OsStr::new(last),
            root.parent().ok_or(io::ErrorKind::NotFound)?,
        )
    }

    pub(crate) fn walk(
        root: &Path,
        budget: &mut super::WalkBudget<'_>,
        visitor: &mut impl FnMut(&Path, std::fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let root_path_staging = root.parent().ok_or(io::ErrorKind::NotFound)?;
        let root = match open(root, dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(
                nix::errno::Errno::ENOENT | nix::errno::Errno::ENOTDIR | nix::errno::Errno::ELOOP,
            ) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        walk_dir(root, PathBuf::new(), budget, visitor, root_path_staging)
    }

    fn walk_dir(
        dir: OwnedFd,
        prefix: PathBuf,
        budget: &mut super::WalkBudget<'_>,
        visitor: &mut impl FnMut(&Path, std::fs::File) -> io::Result<()>,
        staging: &Path,
    ) -> io::Result<()> {
        let entries = nix::dir::Dir::from_fd(nix::unistd::dup(&dir)?)?.into_iter();
        let mut stack = vec![(dir, entries, prefix)];
        while let Some((dir, entries, prefix)) = stack.last_mut() {
            let Some(entry) = entries.next() else {
                stack.pop();
                continue;
            };
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            budget.entry()?;
            let name = std::ffi::OsStr::from_bytes(name);
            let relative = prefix.join(name);
            let stat = match fstatat(&*dir, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(nix::errno::Errno::ENOENT) => continue,
                Err(error) => return Err(error.into()),
            };
            let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
            if kind == SFlag::S_IFDIR {
                // Never enumerate by pathname: the held descriptor remains
                // confined if the entry is replaced with an outside link.
                match open_dir_at(dir, name) {
                    Ok(child) => {
                        let entries =
                            nix::dir::Dir::from_fd(nix::unistd::dup(&child)?)?.into_iter();
                        stack.push((child, entries, relative));
                    }
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                        ) => {}
                    Err(error) => return Err(error),
                }
            } else if kind == SFlag::S_IFREG {
                match open_file_at(dir, name, staging) {
                    Ok(file) => visitor(&relative, file)?,
                    Err(error)
                        if error.kind() == io::ErrorKind::NotFound
                            || matches!(
                                error.raw_os_error(),
                                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                            ) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}

// Platforms without descriptor-relative no-follow traversal (Windows) use the
// canonical-prefix check WI 0137 §3 specifies for "elsewhere".
#[cfg(not(unix))]
pub(crate) mod platform {
    pub(crate) use super::portable::{open_beneath, walk};
    pub(crate) use super::portable::{open_beneath as open_context, walk as walk_context};
}

/// The canonical-prefix fallback (WI 0137 §3) for platforms without
/// `openat(O_NOFOLLOW)`. Pathname-based, so it is checked on both sides of
/// the open:
///
/// - the root is canonicalised once; every component beneath it is looked
///   up with `symlink_metadata` and must be a plain directory (or, last, a
///   plain regular file) — a symlink or junction anywhere is refused;
/// - Windows device names (`NUL`, `COM1`, …) and names Windows would alias
///   by stripping a trailing dot or space are refused before any lookup;
/// - after opening, the name is canonicalised again and must still lie
///   beneath the canonical root, every component is re-checked, and the
///   handle's own metadata must be a regular file agreeing with the name's.
///
/// Compiled on every platform under test so the Unix suite exercises it.
#[cfg(not(unix))]
mod portable {
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    /// `candidate` is strictly beneath `root` (both canonical), compared by
    /// path component so `/out-evil` is not beneath `/out`.
    pub(super) fn is_beneath(root: &Path, candidate: &Path) -> bool {
        candidate != root && candidate.starts_with(root)
    }

    /// A name Windows would open as a device, or alias to another name.
    pub(super) fn is_windows_unsafe_name(name: &OsStr) -> bool {
        let name = name.to_string_lossy();
        if name.ends_with('.') || name.ends_with(' ') {
            return true;
        }
        let stem = name.split('.').next().unwrap_or("").trim_end();
        let upper = stem.to_ascii_uppercase();
        let numbered = |prefix: &str| {
            upper.strip_prefix(prefix).is_some_and(|rest| {
                let mut chars = rest.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '¹' | '²' | '³'), None)
                )
            })
        };
        matches!(
            upper.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || numbered("COM")
            || numbered("LPT")
    }

    fn not_found() -> io::Error {
        io::ErrorKind::NotFound.into()
    }

    /// Join `segments` to `root`, refusing any component that is a link, a
    /// non-directory parent, or a final entry that is not a regular file.
    fn checked_path(root: &Path, segments: &[&OsStr]) -> io::Result<PathBuf> {
        let (_, parents) = segments.split_last().ok_or_else(not_found)?;
        let mut path = root.to_path_buf();
        for (i, segment) in segments.iter().enumerate() {
            if is_windows_unsafe_name(segment) {
                return Err(not_found());
            }
            path.push(segment);
            let kind = fs::symlink_metadata(&path)?.file_type();
            let ok = if i < parents.len() {
                kind.is_dir()
            } else {
                kind.is_file()
            };
            if kind.is_symlink() || !ok {
                return Err(not_found());
            }
        }
        Ok(path)
    }

    fn open_checked(root: &Path, segments: &[&OsStr]) -> io::Result<fs::File> {
        let path = checked_path(root, segments)?;
        let file = fs::File::open(&path)?;
        let resolved = fs::canonicalize(&path)?;
        if !is_beneath(root, &resolved) || checked_path(root, segments)? != path {
            return Err(not_found());
        }
        let handle = file.metadata()?;
        let named = fs::symlink_metadata(&path)?;
        if !handle.is_file()
            || handle.len() != named.len()
            || handle.modified().ok() != named.modified().ok()
        {
            return Err(not_found());
        }
        Ok(file)
    }

    /// The root itself must be a real directory, not a link to one.
    fn canonical_root(root: &Path) -> io::Result<Option<PathBuf>> {
        match fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() => fs::canonicalize(root).map(Some),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open_beneath(root: &Path, segments: &[&str]) -> io::Result<fs::File> {
        let root = canonical_root(root)?.ok_or_else(not_found)?;
        let segments: Vec<&OsStr> = segments.iter().map(OsStr::new).collect();
        open_checked(&root, &segments)
    }

    pub(crate) fn walk(
        root: &Path,
        budget: &mut super::WalkBudget<'_>,
        visitor: &mut impl FnMut(&Path, fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some(root) = canonical_root(root)? else {
            return Ok(());
        };
        let mut stack: Vec<Vec<std::ffi::OsString>> = vec![Vec::new()];
        while let Some(prefix) = stack.pop() {
            let mut dir = root.clone();
            dir.extend(&prefix);
            let resolved = fs::canonicalize(&dir)?;
            if resolved != root && !is_beneath(&root, &resolved) {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                budget.entry()?;
                let kind = match fs::symlink_metadata(entry.path()) {
                    Ok(meta) => meta.file_type(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                let mut segments = prefix.clone();
                segments.push(entry.file_name());
                if kind.is_symlink() || is_windows_unsafe_name(&entry.file_name()) {
                    continue;
                }
                if kind.is_dir() {
                    stack.push(segments);
                } else if kind.is_file() {
                    let names: Vec<&OsStr> = segments.iter().map(|s| s.as_os_str()).collect();
                    match open_checked(&root, &names) {
                        Ok(file) => {
                            // `/`-separated on every platform, as the listing
                            // and `validate_output_path` expect.
                            let relative = PathBuf::from(segments.join(OsStr::new("/")));
                            visitor(&relative, file)?;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct SafeTree {
    root: PathBuf,
}
#[derive(Debug, Clone, Copy)]
pub struct TreeBudget {
    pub max_entries: usize,
    pub max_hash_bytes: u64,
    pub deadline: std::time::Duration,
}
impl Default for TreeBudget {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_hash_bytes: 256 << 20,
            deadline: std::time::Duration::from_secs(2),
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum SafeTreeError {
    #[error("not found")]
    NotFound,
    #[error("too large")]
    TooLarge { size_bytes: u64 },
    #[error("listing could not complete")]
    Incomplete,
    #[error("path component is not a directory")]
    NotADirectory,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
pub struct TreeListing {
    pub files: Vec<ContextFileMeta>,
    pub skipped: bool,
}
pub struct OpenedFile {
    pub file: std::fs::File,
    pub size_bytes: u64,
    pub modified: Option<std::time::SystemTime>,
}
impl SafeTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn list(
        &self,
        limits: TreeBudget,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<ContextFileMeta>, SafeTreeError> {
        Ok(self.list_report(limits, cancelled)?.files)
    }
    pub fn list_report(
        &self,
        limits: TreeBudget,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<TreeListing, SafeTreeError> {
        self.scan(limits, cancelled, true)
    }
    /// Sizes and types only: mutation caps must not hash an oversized target.
    pub(crate) fn inventory(&self) -> Result<Vec<ContextFileMeta>, SafeTreeError> {
        let listing = self.scan(
            TreeBudget::default(),
            &std::sync::atomic::AtomicBool::new(false),
            false,
        )?;
        if listing.skipped {
            return Err(SafeTreeError::Incomplete);
        }
        Ok(listing.files)
    }
    fn scan(
        &self,
        limits: TreeBudget,
        cancelled: &std::sync::atomic::AtomicBool,
        hash: bool,
    ) -> Result<TreeListing, SafeTreeError> {
        let mut skipped = false;
        let mut budget = WalkBudget::new(cancelled);
        budget.max_entries = limits.max_entries;
        budget.deadline = std::time::Instant::now() + limits.deadline;
        let deadline = budget.deadline;
        let mut hash_left = limits.max_hash_bytes;
        let mut files = Vec::new();
        platform::walk_context(&self.root, &mut budget, &mut |path, mut file| {
            let Some(path) = path.to_str().and_then(|p| ContextRelPath::parse(p).ok()) else {
                skipped = true;
                return Ok(());
            };
            let before = file.metadata()?;
            if hash && before.len() > hash_left {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let sha256 = if hash {
                hash_left -= before.len();
                sha256_hex(&mut file, before.len(), cancelled, deadline)?
            } else {
                String::new()
            };
            let after = file.metadata()?;
            if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            files.push(ContextFileMeta {
                path: path.as_str().to_owned(),
                size_bytes: before.len(),
                sha256,
                modified_at: before.modified().ok().map(Into::into),
            });
            Ok(())
        })
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                SafeTreeError::Incomplete
            } else {
                SafeTreeError::Io(e)
            }
        })?;
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(SafeTreeError::Incomplete);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(TreeListing { files, skipped })
    }
    pub fn open(&self, path: &ContextRelPath) -> Result<OpenedFile, SafeTreeError> {
        let segments: Vec<_> = path.segments().collect();
        let file =
            platform::open_context(&self.root, &segments).map_err(|_| SafeTreeError::NotFound)?;
        let meta = file.metadata().map_err(|_| SafeTreeError::NotFound)?;
        if !meta.is_file() {
            return Err(SafeTreeError::NotFound);
        }
        Ok(OpenedFile {
            file,
            size_bytes: meta.len(),
            modified: meta.modified().ok(),
        })
    }
    pub fn read(
        &self,
        path: &ContextRelPath,
        max_bytes: u64,
    ) -> Result<(Vec<u8>, ContextFileMeta), SafeTreeError> {
        use sha2::{Digest, Sha256};
        let mut opened = self.open(path)?;
        if opened.size_bytes > max_bytes {
            return Err(SafeTreeError::TooLarge {
                size_bytes: opened.size_bytes,
            });
        }
        let mut bytes = Vec::new();
        opened
            .file
            .by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(SafeTreeError::TooLarge {
                size_bytes: bytes.len() as u64,
            });
        }
        let after = opened.file.metadata()?;
        if after.len() != opened.size_bytes
            || after.modified().ok() != opened.modified
            || bytes.len() as u64 != after.len()
        {
            return Err(SafeTreeError::NotFound);
        }
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok((
            bytes,
            ContextFileMeta {
                path: path.as_str().to_owned(),
                size_bytes: after.len(),
                sha256,
                modified_at: opened.modified.map(Into::into),
            },
        ))
    }
    pub(crate) fn replace_atomic(
        &self,
        path: &ContextRelPath,
        staged: &Path,
    ) -> Result<(), SafeTreeError> {
        mutation::replace(&self.root, path, staged)
    }
    pub(crate) fn remove(&self, path: &ContextRelPath) -> Result<(), SafeTreeError> {
        mutation::remove(&self.root, path)
    }
    pub(crate) fn ensure_root(&self) -> Result<(), SafeTreeError> {
        #[cfg(unix)]
        {
            platform::context_root(&self.root, true)?;
        }
        if !cfg!(unix) {
            return Err(std::io::Error::from(std::io::ErrorKind::Unsupported).into());
        }
        Ok(())
    }
}

#[cfg(unix)]
mod mutation {
    use super::*;
    use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
    use nix::sys::stat::{fstatat, mkdirat, Mode, SFlag};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::os::fd::OwnedFd;
    fn flags() -> OFlag {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    }
    fn parent(
        root: &Path,
        path: &ContextRelPath,
        create: bool,
    ) -> Result<(OwnedFd, String), SafeTreeError> {
        let mut names: Vec<_> = path.segments().collect();
        let last = names.pop().ok_or(SafeTreeError::NotFound)?.to_owned();
        let mut dir = platform::context_root(root, false)?;
        for name in names {
            if create {
                match mkdirat(&dir, name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                    Err(e) => return Err(map(e)),
                }
            }
            dir = openat(&dir, name, flags(), Mode::empty()).map_err(map)?;
        }
        // A final link/non-regular inode must not be adopted or deleted.
        match fstatat(&dir, last.as_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat)
                if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG =>
            {
                return Err(SafeTreeError::NotADirectory)
            }
            Ok(_) | Err(nix::errno::Errno::ENOENT) => {}
            Err(e) => return Err(map(e)),
        }
        Ok((dir, last))
    }
    fn map(e: nix::errno::Errno) -> SafeTreeError {
        match e {
            nix::errno::Errno::ENOENT => SafeTreeError::NotFound,
            nix::errno::Errno::ENOTDIR | nix::errno::Errno::ELOOP => SafeTreeError::NotADirectory,
            e => SafeTreeError::Io(e.into()),
        }
    }
    pub(super) fn replace(
        root: &Path,
        path: &ContextRelPath,
        staged: &Path,
    ) -> Result<(), SafeTreeError> {
        let (dir, name) = parent(root, path, true)?;
        let staging = open(
            staged.parent().ok_or(SafeTreeError::NotFound)?,
            flags(),
            Mode::empty(),
        )
        .map_err(map)?;
        renameat(
            &staging,
            staged.file_name().ok_or(SafeTreeError::NotFound)?,
            &dir,
            name.as_str(),
        )
        .map_err(map)?;
        std::fs::File::from(dir).sync_all()?;
        Ok(())
    }
    pub(super) fn remove(root: &Path, path: &ContextRelPath) -> Result<(), SafeTreeError> {
        let (dir, name) = parent(root, path, false)?;
        unlinkat(&dir, name.as_str(), UnlinkatFlags::NoRemoveDir).map_err(map)?;
        std::fs::File::from(dir).sync_all()?;
        Ok(())
    }
}
#[cfg(not(unix))]
mod mutation {
    use super::*;
    // A pathname precheck cannot fence parent replacement on Windows. Until
    // handle-relative mutations are implemented, refuse every context write.
    pub(super) fn replace(_: &Path, _: &ContextRelPath, _: &Path) -> Result<(), SafeTreeError> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported).into())
    }
    pub(super) fn remove(_: &Path, _: &ContextRelPath) -> Result<(), SafeTreeError> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported).into())
    }
}

#[cfg(all(test, windows))]
mod context_mutation_tests {
    use super::*;
    #[test]
    fn windows_mutations_fail_closed_before_any_path_lookup() {
        let path = ContextRelPath::parse("sub/note").unwrap();
        for error in [
            mutation::replace(
                Path::new("missing-root"),
                &path,
                Path::new("missing-staged"),
            )
            .unwrap_err(),
            mutation::remove(Path::new("missing-root"), &path).unwrap_err(),
        ] {
            assert!(
                matches!(error, SafeTreeError::Io(e) if e.kind() == std::io::ErrorKind::Unsupported)
            );
        }
    }
}
