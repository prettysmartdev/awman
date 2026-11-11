//! A command's outputs directory (WI 0137 §3): the host directory mounted
//! read-write at [`OUTPUT_MOUNT_PATH`] into the command's agent containers,
//! where agents leave machine-readable results for API clients.
//!
//! The directory is `ApiPaths::command_output_dir(session, command)`, under
//! the API root and never inside the clone, so it survives a remote session's
//! clone being deleted and is removed with the command's logs.
//!
//! The agent controls everything in this directory, so every reader here
//! treats it as hostile input (`aspec/architecture/security.md`):
//!
//! - a requested path is a list of plain names: `..`, `.`, empty segments,
//!   absolute paths, backslashes, drive prefixes and NUL are rejected before
//!   anything touches the disk ([`validate_output_path`]);
//! - on Unix (Linux and macOS) a file is opened one component at a time with
//!   `openat(O_NOFOLLOW)` from a directory descriptor that is itself opened
//!   with `O_NOFOLLOW`. Each call resolves exactly one name, so no component
//!   can be a symlink, and because the parent descriptor is held there is no
//!   window between the check and the open for a planted link to slip into.
//!   Linux pins a non-I/O O_PATH handle and validates it before reopening
//!   the regular inode. Other Unix platforms pin a hard link in a private
//!   unmounted sibling directory before validating and opening that name.
//!   Other platforms (Windows) use the canonical-prefix fallback the spec
//!   names for "elsewhere": every component is checked with
//!   `symlink_metadata` before and after the open, the opened name must
//!   canonicalise beneath the canonical root, and Windows device names are
//!   refused (see `portable`).
//! - only regular files are listed or served. Symlinks, devices, FIFOs and
//!   sockets are omitted, and every listed fact (size, hash, mtime) comes
//!   from the safely opened descriptor, never from a path lookup.
//!
//! There is no quota on the directory: an agent writing here has the same
//! disk exposure as an agent writing into its workspace. Files are written by
//! the agent and are served as they are, without redaction.

use std::io::Read;
use std::path::Path;

use crate::data::fs::api_db::OutputsSummary;

/// Where the outputs directory is mounted inside an agent container.
pub const OUTPUT_MOUNT_PATH: &str = "/awman/output";

/// Largest file `GET /v1/worker/commands/{id}/outputs/{path}` serves. A bigger file
/// is still listed; fetching it returns 413.
pub const MAX_SERVED_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

/// One regular file in the outputs directory, as `GET …/outputs` lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OutputFile {
    /// Relative to the outputs directory, `/`-separated.
    pub path: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    /// RFC 3339, UTC. Absent when the platform reports no modification time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
}

/// The body of `GET /v1/worker/commands/{id}/outputs`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct OutputListing {
    /// Sorted by `path`.
    pub files: Vec<OutputFile>,
    /// Equals `CommandResult::outputs` for the same directory contents.
    pub total: OutputsSummary,
}

/// Why a requested output path was rejected before any file was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputPathError {
    Empty,
    Absolute,
    EmptySegment,
    DotSegment,
    ForbiddenCharacter,
}

impl std::fmt::Display for OutputPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Empty => "output path is empty",
            Self::Absolute => "output path must be relative",
            Self::EmptySegment => "output path has an empty segment",
            Self::DotSegment => "output path may not contain '.' or '..' segments",
            Self::ForbiddenCharacter => "output path contains a forbidden character",
        };
        f.write_str(text)
    }
}

impl std::error::Error for OutputPathError {}

/// Split a client-supplied output path into its plain-name segments, or
/// reject it. Accepts only `name(/name)*` where no name is `.` or `..` and
/// none contains a NUL, a backslash or a colon (a Windows drive or stream
/// prefix).
pub fn validate_output_path(path: &str) -> Result<Vec<&str>, OutputPathError> {
    if path.is_empty() {
        return Err(OutputPathError::Empty);
    }
    if path.starts_with('/') {
        return Err(OutputPathError::Absolute);
    }
    if path.contains(['\0', '\\', ':']) {
        return Err(OutputPathError::ForbiddenCharacter);
    }
    let segments: Vec<&str> = path.split('/').collect();
    for segment in &segments {
        match *segment {
            "" => return Err(OutputPathError::EmptySegment),
            "." | ".." => return Err(OutputPathError::DotSegment),
            _ => {}
        }
    }
    Ok(segments)
}

/// A regular file opened safely from the outputs directory.
#[derive(Debug)]
pub struct OpenedOutput {
    pub file: std::fs::File,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

/// Open `path` (already validated) beneath `root` without following any
/// symlink. `Ok(None)` when it does not exist or is not a regular file —
/// including a symlink, device or FIFO.
pub fn open_output_file(root: &Path, path: &str) -> Result<Option<OpenedOutput>, OutputPathError> {
    let segments = validate_output_path(path)?;
    let Ok(file) = platform::open_beneath(root, &segments) else {
        return Ok(None);
    };
    let Ok(metadata) = file.metadata() else {
        return Ok(None);
    };
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    Ok(Some(OpenedOutput {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        file,
    }))
}

/// List a bounded, finite snapshot. If it cannot be completed, return an
/// explicit error instead of a partial listing or a hash for a moving target.
pub fn list_output_dir(
    root: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<OutputListing> {
    let mut listing = OutputListing::default();
    let mut budget = WalkBudget::new(cancelled);
    let mut hash_bytes_left = 256 * 1024 * 1024u64;
    let deadline = budget.deadline;
    platform::walk(root, &mut budget, &mut |path, mut file| {
        let before = file.metadata()?;
        listing.total.files += 1;
        listing.total.bytes = listing.total.bytes.saturating_add(before.len());
        let Some(path) = path
            .to_str()
            .filter(|path| validate_output_path(path).is_ok())
        else {
            return Ok(());
        };
        if before.len() > hash_bytes_left {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        hash_bytes_left -= before.len();
        let sha256 = sha256_hex(&mut file, before.len(), cancelled, deadline)?;
        let after = file.metadata()?;
        if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        listing.files.push(OutputFile {
            path: path.to_owned(),
            size: before.len(),
            sha256,
            modified_at: before
                .modified()
                .ok()
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()),
        });
        Ok(())
    })?;
    listing.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(listing)
}

/// Count all regular files, including names that the URL API cannot address.
/// Resource-limited traversal is explicitly marked incomplete.
pub fn summarize_output_dir(root: &Path) -> OutputsSummary {
    let mut summary = OutputsSummary::default();
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let result = platform::walk(root, &mut WalkBudget::new(&cancelled), &mut |_, file| {
        summary.files += 1;
        summary.bytes = summary.bytes.saturating_add(file.metadata()?.len());
        Ok(())
    });
    summary.incomplete = result.is_err();
    summary
}

struct WalkBudget<'a> {
    cancelled: &'a std::sync::atomic::AtomicBool,
    deadline: std::time::Instant,
    entries: usize,
}
impl<'a> WalkBudget<'a> {
    fn new(cancelled: &'a std::sync::atomic::AtomicBool) -> Self {
        Self {
            cancelled,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
            entries: 0,
        }
    }
    fn entry(&mut self) -> std::io::Result<()> {
        self.entries += 1;
        if self.entries > 100_000
            || self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
            || std::time::Instant::now() > self.deadline
        {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }
}

/// Make `dir` ready to mount at [`OUTPUT_MOUNT_PATH`].
///
/// `clear` removes whatever is there first: `exec prompt` reruns from scratch
/// on every attempt, so its outputs start empty. Without `clear` the existing
/// files stay (a resumed workflow keeps the files of the steps it keeps).
/// Either way a non-directory at `dir` — such as a symlink — is removed, so
/// the mount never goes through a link.
pub fn prepare_output_dir(dir: &Path, clear: bool) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        // `remove_dir_all` does not follow symlinks inside the tree.
        Ok(metadata) if metadata.is_dir() && clear => std::fs::remove_dir_all(dir)?,
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => std::fs::remove_file(dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(dir)
}

/// The read-write overlay mounting `dir` at [`OUTPUT_MOUNT_PATH`]. Only this
/// one directory is mounted, never a parent of it.
pub fn output_dir_overlay(dir: &Path) -> crate::data::config::overlays::DirectorySpec {
    crate::data::config::overlays::DirectorySpec {
        host: dir.to_string_lossy().into_owned(),
        container: OUTPUT_MOUNT_PATH.to_string(),
        permission: crate::data::config::overlays::OverlayPermission::ReadWrite,
    }
}

/// `Content-Type` for a served output, from its extension.
pub fn content_type_for(path: &str) -> &'static str {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase());
    match extension.as_deref() {
        Some("json") => "application/json",
        Some("ndjson" | "jsonl") => "application/x-ndjson",
        Some("txt" | "log") => "text/plain; charset=utf-8",
        Some("md" | "markdown") => "text/markdown; charset=utf-8",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("xml") => "application/xml",
        Some("yaml" | "yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

fn sha256_hex(
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
mod platform {
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

    pub(super) fn open_beneath(root: &Path, segments: &[&str]) -> io::Result<std::fs::File> {
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

    pub(super) fn walk(
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
mod platform {
    pub(super) use super::portable::{open_beneath, walk};
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

    pub(super) fn open_beneath(root: &Path, segments: &[&str]) -> io::Result<fs::File> {
        let root = canonical_root(root)?.ok_or_else(not_found)?;
        let segments: Vec<&OsStr> = segments.iter().map(OsStr::new).collect();
        open_checked(&root, &segments)
    }

    pub(super) fn walk(
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
