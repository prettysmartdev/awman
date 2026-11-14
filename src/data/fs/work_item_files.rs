//! Local work item files (Layer 0): `NNNN-*.md` lookup, safe reads, creation.
//!
//! Reading a work item as *input* is confined to the checkout: an explicit
//! path or a configured directory that resolves outside `git_root` is refused,
//! and the opened file is checked against the path that was validated, so a
//! symlink swapped in between cannot lead outside.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWorkItemFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug)]
pub enum LocalWorkItemFileError {
    NotFound {
        path: PathBuf,
    },
    OutsideCheckout {
        path: PathBuf,
    },
    NotRegular {
        path: PathBuf,
    },
    TooLarge {
        limit: u64,
    },
    InvalidUtf8 {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for LocalWorkItemFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { path } => write!(f, "work item file not found: {}", path.display()),
            Self::OutsideCheckout { path } => write!(
                f,
                "work item {} is outside the repository checkout; refusing to read it",
                path.display()
            ),
            Self::NotRegular { path } => {
                write!(f, "work item {} is not a regular file", path.display())
            }
            Self::TooLarge { limit } => {
                write!(f, "work item is larger than the {limit}-byte limit")
            }
            Self::InvalidUtf8 { path } => {
                write!(f, "work item {} is not valid UTF-8", path.display())
            }
            Self::Io { path, source } => {
                write!(f, "cannot read work item {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LocalWorkItemFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_error(path: &Path, source: std::io::Error) -> LocalWorkItemFileError {
    if source.kind() == std::io::ErrorKind::NotFound {
        LocalWorkItemFileError::NotFound {
            path: path.to_path_buf(),
        }
    } else {
        LocalWorkItemFileError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

fn confined(
    git_root: &Path,
    candidate: &Path,
) -> Result<(PathBuf, PathBuf), LocalWorkItemFileError> {
    let root = std::fs::canonicalize(git_root).map_err(|e| io_error(git_root, e))?;
    let canonical = std::fs::canonicalize(candidate).map_err(|e| io_error(candidate, e))?;
    if !canonical.starts_with(&root) {
        return Err(LocalWorkItemFileError::OutsideCheckout {
            path: candidate.to_path_buf(),
        });
    }
    Ok((root, canonical))
}

/// Every `format!("{number:04}-") + *.md` file directly in `directory`,
/// sorted by path. A missing directory yields none; the directory must lie
/// inside the checkout; other IO errors (including per-entry ones) propagate.
pub fn matching_work_item_files(
    git_root: &Path,
    directory: &Path,
    number: u32,
) -> Result<Vec<PathBuf>, LocalWorkItemFileError> {
    let canonical_dir = match std::fs::canonicalize(directory) {
        Ok(_) => confined(git_root, directory)?.1,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(directory, e)),
    };
    let prefix = format!("{number:04}-");
    let mut matches = Vec::new();
    let entries = std::fs::read_dir(&canonical_dir).map_err(|e| io_error(directory, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_error(directory, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&prefix) && name.ends_with(".md") {
            // Keep the configured spelling of the directory in the result.
            matches.push(directory.join(name));
        }
    }
    matches.sort();
    Ok(matches)
}

/// Read a local work item. A relative `path` resolves from `working_dir`. The
/// file must be inside the checkout once symlinks are resolved, a regular
/// file, within the size cap and strict UTF-8.
pub fn read_local_work_item(
    git_root: &Path,
    working_dir: &Path,
    path: &Path,
) -> Result<LocalWorkItemFile, LocalWorkItemFileError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };
    let (_, canonical) = confined(git_root, &joined)?;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The canonical path has no symlinks left; refuse one swapped in
        // since, and never block on a FIFO.
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    }
    let file = options.open(&canonical).map_err(|e| io_error(&joined, e))?;
    let meta = file.metadata().map_err(|e| io_error(&joined, e))?;
    if !meta.is_file() {
        return Err(LocalWorkItemFileError::NotRegular { path: joined });
    }
    // Re-check containment after the open: a directory swapped for a symlink
    // meanwhile would resolve elsewhere than the descriptor we hold.
    let (_, again) = confined(git_root, &joined)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let now = std::fs::metadata(&again).map_err(|e| io_error(&joined, e))?;
        if again != canonical || now.ino() != meta.ino() || now.dev() != meta.dev() {
            return Err(LocalWorkItemFileError::OutsideCheckout { path: joined });
        }
    }
    if meta.len() > MAX_WORK_ITEM_BYTES {
        return Err(LocalWorkItemFileError::TooLarge {
            limit: MAX_WORK_ITEM_BYTES,
        });
    }
    let mut bytes = Vec::new();
    file.take(MAX_WORK_ITEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(&joined, e))?;
    if bytes.len() as u64 > MAX_WORK_ITEM_BYTES {
        return Err(LocalWorkItemFileError::TooLarge {
            limit: MAX_WORK_ITEM_BYTES,
        });
    }
    let content = String::from_utf8(bytes).map_err(|_| LocalWorkItemFileError::InvalidUtf8 {
        path: joined.clone(),
    })?;
    Ok(LocalWorkItemFile {
        path: canonical,
        content,
    })
}

/// Read the work item template (`workItems.template`). It comes from repo
/// config and its text can be published to a remote tracker by `new spec`, so
/// it is read like any local work item: confined to the checkout once
/// symlinks are resolved, a regular file (never a FIFO), within the size cap
/// and strict UTF-8.
pub fn read_work_item_template(git_root: &Path, path: &Path) -> Result<String, DataError> {
    read_local_work_item(git_root, git_root, path)
        .map(|file| file.content)
        .map_err(|e| DataError::InvalidPath {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
}

/// One more than the highest `NNNN-*` number in `directory` (1 when it is
/// missing or empty). Directory errors propagate rather than restarting at 1.
pub fn next_work_item_number(directory: &Path) -> Result<u32, DataError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(e) => return Err(DataError::io(directory, e)),
    };
    let mut max = 0u32;
    for entry in entries {
        let entry = entry.map_err(|e| DataError::io(directory, e))?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        let bytes = s.as_bytes();
        if bytes.len() >= 5 && bytes[4] == b'-' && bytes[..4].iter().all(u8::is_ascii_digit) {
            if let Ok(n) = s[..4].parse::<u32>() {
                max = max.max(n);
            }
        }
    }
    max.checked_add(1).ok_or_else(|| DataError::InvalidPath {
        path: directory.to_path_buf(),
        reason: "work item numbers are exhausted".into(),
    })
}

/// Create `<directory>/<NNNN>-<slug>.md` (and the directory) with `body`,
/// failing rather than overwriting an existing file. `git_root` is the
/// checkout the directory is configured relative to; creation keeps the
/// configured location even when it is outside it.
pub fn write_new_local_work_item(
    git_root: &Path,
    directory: &Path,
    number: u32,
    slug: &str,
    body: &str,
) -> Result<PathBuf, DataError> {
    use std::io::Write;
    let _ = git_root;
    if slug.is_empty() || slug.contains(['/', '\\']) || slug == "." || slug == ".." {
        return Err(DataError::InvalidPath {
            path: directory.to_path_buf(),
            reason: format!("invalid work item slug {slug:?}"),
        });
    }
    std::fs::create_dir_all(directory).map_err(|e| DataError::io(directory, e))?;
    let path = directory.join(format!("{number:04}-{slug}.md"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| DataError::io(&path, e))?;
    file.write_all(body.as_bytes())
        .map_err(|e| DataError::io(&path, e))?;
    Ok(path)
}
