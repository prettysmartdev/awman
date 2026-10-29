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
//!   Platforms without descriptor-relative traversal refuse access.
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

/// Largest file `GET /v1/commands/{id}/outputs/{path}` serves. A bigger file
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

/// The body of `GET /v1/commands/{id}/outputs`.
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

    #[cfg(any(not(target_os = "linux"), test))]
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
    #[cfg(test)]
    mod race_tests {
        use super::*;
        #[test]
        fn private_link_pinning_rejects_links_and_fifos_before_a_data_open() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("outputs");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("file"), "safe").unwrap();
            std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
            nix::unistd::mkfifo(&root.join("fifo"), Mode::S_IRWXU).unwrap();
            let dir = open(&root, dir_flags(), Mode::empty()).unwrap();
            assert!(
                open_linked_regular_at(&dir, std::ffi::OsStr::new("link"), tmp.path()).is_err()
            );
            assert!(
                open_linked_regular_at(&dir, std::ffi::OsStr::new("fifo"), tmp.path()).is_err()
            );
            let mut file =
                open_linked_regular_at(&dir, std::ffi::OsStr::new("file"), tmp.path()).unwrap();
            std::fs::remove_file(root.join("file")).unwrap();
            std::os::unix::fs::symlink("/dev/null", root.join("file")).unwrap();
            let mut content = String::new();
            std::io::Read::read_to_string(&mut file, &mut content).unwrap();
            assert_eq!(content, "safe");
        }
        #[test]
        fn descriptor_walk_stays_inside_after_a_directory_is_swapped_for_a_link() {
            let tmp = tempfile::tempdir().unwrap();
            let inside = tmp.path().join("inside");
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&inside).unwrap();
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(inside.join("safe"), "safe").unwrap();
            std::fs::write(outside.join("host-secret"), "secret").unwrap();
            let held = open(&inside, dir_flags(), Mode::empty()).unwrap();
            std::fs::rename(&inside, tmp.path().join("held")).unwrap();
            std::os::unix::fs::symlink(&outside, &inside).unwrap();
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let mut names = Vec::new();
            walk_dir(
                held,
                PathBuf::new(),
                &mut crate::data::fs::command_outputs::WalkBudget::new(&cancelled),
                &mut |path, _| {
                    names.push(path.to_path_buf());
                    Ok(())
                },
                tmp.path(),
            )
            .unwrap();
            assert_eq!(names, [PathBuf::from("safe")]);
            assert!(open_beneath(&inside, &["host-secret"]).is_err());
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_name_swap_cannot_turn_a_pinned_regular_file_into_a_device_open() {
            let tmp = tempfile::tempdir().unwrap();
            let file = tmp.path().join("file");
            std::fs::write(&file, "safe").unwrap();
            let pinned = open(
                &file,
                OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            std::fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink("/dev/null", &file).unwrap();
            let mut input = reopen_regular(pinned).unwrap();
            let mut body = String::new();
            std::io::Read::read_to_string(&mut input, &mut body).unwrap();
            assert_eq!(body, "safe");
            let device = open(
                Path::new("/dev/null"),
                OFlag::O_PATH | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            assert!(reopen_regular(device).is_err());
        }
    }
}

// Refuse output access on platforms without a descriptor-relative no-follow
// implementation. A pathname canonicalization is not proof about an open handle.
#[cfg(not(unix))]
mod platform {
    use std::{io, path::Path};
    pub(super) fn open_beneath(_: &Path, _: &[&str]) -> io::Result<std::fs::File> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(super) fn walk(
        _: &Path,
        _: &mut super::WalkBudget<'_>,
        _: &mut impl FnMut(&Path, std::fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_regular_files_recursively_and_ignores_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("answer.json"), b"{}").unwrap();
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("docs/notes.md"), b"12345").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("passwd")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("docs"), tmp.path().join("docs-link")).unwrap();
        let summary = summarize_output_dir(tmp.path());
        assert_eq!(
            summary,
            OutputsSummary {
                files: 2,
                bytes: 7,
                incomplete: false
            }
        );
    }

    #[test]
    fn hostile_paths_are_rejected_before_any_open() {
        assert_eq!(validate_output_path(""), Err(OutputPathError::Empty));
        assert_eq!(
            validate_output_path("/etc/passwd"),
            Err(OutputPathError::Absolute)
        );
        assert_eq!(
            validate_output_path("a//b"),
            Err(OutputPathError::EmptySegment)
        );
        assert_eq!(
            validate_output_path("a/"),
            Err(OutputPathError::EmptySegment)
        );
        assert_eq!(
            validate_output_path("../x"),
            Err(OutputPathError::DotSegment)
        );
        assert_eq!(
            validate_output_path("a/../../x"),
            Err(OutputPathError::DotSegment)
        );
        assert_eq!(
            validate_output_path("./x"),
            Err(OutputPathError::DotSegment)
        );
        for bad in ["a\0b", "a\\..\\b", "C:x"] {
            assert_eq!(
                validate_output_path(bad),
                Err(OutputPathError::ForbiddenCharacter)
            );
        }
        assert_eq!(
            validate_output_path("tests/report.json").unwrap(),
            ["tests", "report.json"]
        );
        assert_eq!(validate_output_path("..hidden").unwrap(), ["..hidden"]);
    }

    #[test]
    fn listing_has_hash_size_and_mtime_of_regular_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("tests")).unwrap();
        std::fs::write(tmp.path().join("tests/report.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"abc").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("passwd")).unwrap();
            std::os::unix::fs::symlink("/etc", tmp.path().join("etc")).unwrap();
            nix::unistd::mkfifo(&tmp.path().join("fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        }
        let listing =
            list_output_dir(tmp.path(), &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let paths: Vec<&str> = listing.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "tests/report.json"]);
        assert_eq!(
            listing.total,
            OutputsSummary {
                files: 2,
                bytes: 5,
                incomplete: false
            }
        );
        assert_eq!(listing.total, summarize_output_dir(tmp.path()));
        assert_eq!(
            listing.files[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(listing.files[0].modified_at.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_fifos_are_never_opened() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("real/ok.txt"), b"ok").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("passwd")).unwrap();
        std::os::unix::fs::symlink("/etc", tmp.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
        nix::unistd::mkfifo(&tmp.path().join("fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        for path in [
            "passwd",
            "etc/passwd",
            "link/ok.txt",
            "fifo",
            "real",
            "missing",
        ] {
            assert!(
                open_output_file(tmp.path(), path).unwrap().is_none(),
                "{path} must not open"
            );
        }
        let opened = open_output_file(tmp.path(), "real/ok.txt")
            .unwrap()
            .unwrap();
        assert_eq!(opened.size, 2);
        // The root itself being a link is refused too.
        let link_root = tmp.path().join("root-link");
        std::os::unix::fs::symlink(tmp.path().join("real"), &link_root).unwrap();
        assert!(open_output_file(&link_root, "ok.txt").unwrap().is_none());
    }

    #[test]
    fn prepare_clears_only_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("output");
        prepare_output_dir(&dir, false).unwrap();
        std::fs::write(dir.join("kept.json"), b"{}").unwrap();
        prepare_output_dir(&dir, false).unwrap();
        assert!(dir.join("kept.json").exists());
        prepare_output_dir(&dir, true).unwrap();
        assert!(dir.is_dir());
        assert!(!dir.join("kept.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_replaces_a_planted_link_and_never_clears_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("precious"), b"x").unwrap();
        let dir = tmp.path().join("output");
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        prepare_output_dir(&dir, true).unwrap();
        assert!(!std::fs::symlink_metadata(&dir)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.join("precious").exists());
    }

    #[test]
    fn content_type_comes_from_the_extension() {
        assert_eq!(content_type_for("a/report.JSON"), "application/json");
        assert_eq!(content_type_for("notes.md"), "text/markdown; charset=utf-8");
        assert_eq!(content_type_for("blob"), "application/octet-stream");
        assert_eq!(
            content_type_for("dir.json/blob"),
            "application/octet-stream"
        );
        assert_eq!(content_type_for("x.unknown"), "application/octet-stream");
    }

    #[test]
    fn overlay_mounts_exactly_the_output_dir_read_write() {
        let spec = output_dir_overlay(Path::new("/srv/api/sessions/s/commands/c/output"));
        assert_eq!(spec.host, "/srv/api/sessions/s/commands/c/output");
        assert_eq!(spec.container, OUTPUT_MOUNT_PATH);
        assert_eq!(
            spec.permission,
            crate::data::config::overlays::OverlayPermission::ReadWrite
        );
    }

    #[test]
    fn missing_directory_is_an_empty_summary() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            summarize_output_dir(&tmp.path().join("missing")),
            OutputsSummary::default()
        );
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn deep_and_unaddressable_names_are_counted_in_the_physical_total() {
        let tmp = tempfile::tempdir().unwrap();
        let relative = format!("{}answer.json", "d/".repeat(70));
        let path = tmp.path().join(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
        let summary = summarize_output_dir(tmp.path());
        assert_eq!(summary.files, 1);
        assert_eq!(summary.bytes, 2);
        assert!(!summary.incomplete);
        assert!(open_output_file(tmp.path(), &relative).unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            std::fs::write(
                tmp.path().join(std::ffi::OsString::from_vec(vec![0xff])),
                "bad",
            )
            .unwrap();
            let summary = summarize_output_dir(tmp.path());
            assert_eq!(summary.files, 2);
            assert_eq!(summary.bytes, 5);
            let listing = list_output_dir(tmp.path(), &AtomicBool::new(false)).unwrap();
            assert_eq!(listing.total, summary);
            assert_eq!(listing.files.len(), 1);
        }
    }

    #[test]
    fn hashing_has_a_finite_snapshot_and_cooperatively_cancels() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("growing");
        std::fs::write(&path, "old").unwrap();
        let mut input = std::fs::File::open(&path).unwrap();
        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut append, b"new").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let hash = sha256_hex(&mut input, 3, &AtomicBool::new(false), deadline).unwrap();
        use sha2::{Digest, Sha256};
        assert_eq!(
            hash,
            Sha256::digest(b"old")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert!(list_output_dir(tmp.path(), &AtomicBool::new(true)).is_err());
        let mut input = std::fs::File::open(&path).unwrap();
        assert!(sha256_hex(&mut input, 6, &AtomicBool::new(true), deadline).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_device_node_is_rejected_without_a_data_open() {
        assert!(open_output_file(Path::new("/dev"), "null")
            .unwrap()
            .is_none());
    }

    #[cfg(not(unix))]
    #[test]
    fn platforms_without_a_safe_walk_refuse_output_access() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), "data").unwrap();
        assert!(open_output_file(tmp.path(), "file").unwrap().is_none());
        assert!(list_output_dir(tmp.path(), &AtomicBool::new(false)).is_err());
    }
}
