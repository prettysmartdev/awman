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

use crate::data::fs::safe_tree::{platform, sha256_hex, WalkBudget};

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
