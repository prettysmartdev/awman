//! Windows private files: created with an explicit owner-only descriptor and
//! validated on the opened handle, never through a link, junction or other
//! reparse point.
//!
//! The library stays free of unsafe code. Security descriptors and handle
//! inspection come from the binary's native provider (`awman-host-shims`),
//! installed with [`install_windows_native`]. Without it every private store
//! is refused before anything is written.
//!
//! While a path is opened or created, every ancestor directory is held open
//! without delete sharing, so none of them can be renamed, removed or
//! replaced between the check and the use. Write sharing stays allowed so
//! other processes can keep creating and renaming entries inside; instead,
//! each opened component's resolved path must sit directly under the
//! previously pinned directory, which detects a directory that was emptied
//! and turned into a junction while the path was being walked.
//! CreateFile sharing and FILE_FLAG_OPEN_REPARSE_POINT:
//! https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-createfilew

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::OnceLock;

use super::{io_err, refused};
use crate::data::error::DataError;

const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const SHARE_READ_WRITE: u32 = 0x1 | 0x2;

/// The native operations behind Windows private files. Plain `fn` pointers
/// over std types, so a provider built against another copy of this library
/// (as in this crate's own unit tests) installs cleanly.
#[derive(Clone, Copy)]
pub struct WindowsNative {
    /// Create one directory (the parent exists) owned by this user with a
    /// protected owner-only DACL. `AlreadyExists` if anything is there.
    pub create_dir: fn(&Path) -> io::Result<()>,
    /// Create a file exclusively, never through a link, with the same
    /// descriptor, open for read, write and DELETE.
    pub create_file: fn(&Path) -> io::Result<File>,
    /// Inspect an open handle: `Ok(None)` when private, `Ok(Some(reason))`
    /// for the first problem (owner, DACL, reparse point, hard links).
    pub verify: fn(&File) -> io::Result<Option<String>>,
    /// The normalized path the system resolves for an open handle.
    pub final_path: fn(&File) -> io::Result<PathBuf>,
    /// Rename an open file to a leaf name in its own directory, replacing
    /// the existing entry rather than writing through it.
    pub rename: fn(&File, &OsStr) -> io::Result<()>,
}

static NATIVE: OnceLock<WindowsNative> = OnceLock::new();

/// Install the native provider. The first installation wins.
pub fn install_windows_native(native: WindowsNative) {
    let _ = NATIVE.set(native);
}

fn native() -> Result<WindowsNative, DataError> {
    if let Some(native) = NATIVE.get() {
        return Ok(*native);
    }
    // The library's own unit tests link the provider as a dev-dependency.
    #[cfg(test)]
    return Ok(*NATIVE.get_or_init(|| WindowsNative {
        create_dir: awman_host_shims::private_create_dir,
        create_file: awman_host_shims::private_create_file,
        verify: awman_host_shims::private_verify,
        final_path: awman_host_shims::private_final_path,
        rename: awman_host_shims::publish_private_file,
    }));
    #[cfg(not(test))]
    return Err(DataError::Other(
        "private storage cannot be secured: the native Windows file-security provider \
         is not installed in this program"
            .into(),
    ));
}

/// `path` made absolute, on a local drive, with only plain components.
fn absolute(path: &Path) -> Result<PathBuf, DataError> {
    let abs = std::path::absolute(path).map_err(|e| io_err(path, e))?;
    for component in abs.components() {
        match component {
            Component::Prefix(prefix) => {
                if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
                    return Err(refused(
                        path,
                        "is not on a local drive, so its privacy cannot be established",
                    ));
                }
            }
            Component::RootDir => {}
            Component::Normal(name) if !name.to_string_lossy().contains(':') => {}
            _ => return Err(refused(path, "is not a plain local path")),
        }
    }
    Ok(abs)
}

fn absent(path: &Path) -> DataError {
    io_err(path, io::Error::from(io::ErrorKind::NotFound))
}

fn is_absent(error: &DataError) -> bool {
    matches!(error, DataError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
}

const SWAPPED: &str = "changed while it was being checked (a parent directory was replaced)";

/// Pinned directory handles, root first, with the resolved path of the last.
/// Dropping it releases the pins.
struct Pins {
    native: WindowsNative,
    handles: Vec<File>,
    last: Option<PathBuf>,
}

impl Pins {
    fn new(native: WindowsNative) -> Self {
        Pins {
            native,
            handles: Vec::new(),
            last: None,
        }
    }

    /// Require `object` (opened by `path`) to resolve directly under the last
    /// pinned directory.
    fn check_parent(&self, path: &Path, object: &File) -> Result<PathBuf, DataError> {
        let resolved = (self.native.final_path)(object).map_err(|e| io_err(path, e))?;
        if let Some(last) = &self.last {
            if resolved.parent() != Some(last.as_path()) {
                return Err(refused(path, SWAPPED));
            }
        }
        Ok(resolved)
    }

    /// Open and pin the directory at `path`, the next component below the
    /// last pin; `Ok(None)` when it is absent. A junction, link or other
    /// reparse point, or a non-directory, is refused.
    fn pin(&mut self, path: &Path) -> Result<Option<&File>, DataError> {
        let dir = match OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ_WRITE)
            .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
            .open(path)
        {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(path, e)),
        };
        let meta = dir.metadata().map_err(|e| io_err(path, e))?;
        if meta.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(refused(
                path,
                "is a junction, link or other reparse point (these are refused)",
            ));
        }
        if !meta.is_dir() {
            return Err(refused(path, "is not a plain directory"));
        }
        let resolved = self.check_parent(path, &dir)?;
        self.last = Some(resolved);
        self.handles.push(dir);
        Ok(self.handles.last())
    }
}

/// Pin every ancestor of `path`, root first. A missing ancestor is a
/// `NotFound` I/O error.
fn pin_ancestors(native: WindowsNative, path: &Path) -> Result<(PathBuf, Pins), DataError> {
    let abs = absolute(path)?;
    let parent = abs
        .parent()
        .ok_or_else(|| refused(path, "has no parent directory"))?;
    let mut current = PathBuf::new();
    let mut pins = Pins::new(native);
    for component in parent.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        if pins.pin(&current)?.is_none() {
            return Err(absent(&current));
        }
    }
    Ok((abs, pins))
}

fn check(native: &WindowsNative, path: &Path, object: &File) -> Result<(), DataError> {
    match (native.verify)(object) {
        Ok(None) => Ok(()),
        Ok(Some(reason)) => Err(refused(path, &reason)),
        Err(e) => Err(refused(
            path,
            &format!("its owner and access list cannot be checked ({e})"),
        )),
    }
}

/// The opened leaf must be the private object directly under the pins.
fn check_leaf(pins: &Pins, path: &Path, object: &File) -> Result<(), DataError> {
    pins.check_parent(path, object)?;
    check(&pins.native, path, object)
}

pub(super) fn create_private_dir(dir: &Path) -> Result<(), DataError> {
    let native = native()?;
    let abs = absolute(dir)?;
    let components: Vec<_> = abs.components().collect();
    let mut current = PathBuf::new();
    let mut pins = Pins::new(native);
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let mut created = false;
        if pins.pin(&current)?.is_none() {
            match (native.create_dir)(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(&current, e)),
            }
            if pins.pin(&current)?.is_none() {
                return Err(absent(&current));
            }
            created = true;
        }
        // As on Unix, an existing ancestor is only required not to be a
        // link; the directory itself, and anything created here (or raced
        // into place by someone else), must be private.
        if created || index + 1 == components.len() {
            let handle = pins.handles.last().expect("just pinned");
            check(&native, &current, handle)?;
        }
    }
    Ok(())
}

pub(super) fn create_private_dir_new(dir: &Path) -> Result<(), DataError> {
    let native = native()?;
    let (abs, mut pins) = pin_ancestors(native, dir)?;
    (native.create_dir)(&abs).map_err(|e| io_err(dir, e))?;
    let handle = pins.pin(&abs)?.ok_or_else(|| absent(dir))?;
    check(&native, dir, handle)
}

pub(super) fn create_private_file_new(path: &Path) -> Result<File, DataError> {
    let native = native()?;
    let (abs, pins) = pin_ancestors(native, path)?;
    let file = (native.create_file)(&abs).map_err(|e| io_err(path, e))?;
    check_leaf(&pins, path, &file)?;
    Ok(file)
}

pub(super) fn open_private(
    path: &Path,
    create: bool,
    write: bool,
) -> Result<Option<File>, DataError> {
    let native = native()?;
    let (abs, pins) = match pin_ancestors(native, path) {
        Ok(pinned) => pinned,
        Err(e) if !create && is_absent(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    // A record replaced by a concurrent atomic write leaves this handle on the
    // unlinked old file (no links, no resolvable parent); open the new entry
    // again. A persistent problem is still refused after the last attempt.
    let mut attempts = 0;
    loop {
        attempts += 1;
        let Some(file) = open_or_create(&native, path, &abs, create, write)? else {
            return Ok(None);
        };
        let meta = file.metadata().map_err(|e| io_err(path, e))?;
        if !meta.is_file() {
            return Err(refused(path, "is not a regular file"));
        }
        match check_leaf(&pins, path, &file) {
            Ok(()) => return Ok(Some(file)),
            Err(_) if attempts < 3 => continue,
            Err(e) => return Err(e),
        }
    }
}

fn open_or_create(
    native: &WindowsNative,
    path: &Path,
    abs: &Path,
    create: bool,
    write: bool,
) -> Result<Option<File>, DataError> {
    let mut raced = 0;
    loop {
        let opened = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(abs);
        match opened {
            Ok(file) => return Ok(Some(file)),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match (native.create_file)(abs) {
                Ok(file) => return Ok(Some(file)),
                // Created by someone else between the two attempts: open theirs.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && raced < 3 => raced += 1,
                Err(e) => return Err(io_err(path, e)),
            },
            Err(e) => {
                return Err(refused(
                    path,
                    &format!("cannot be opened safely (links are refused): {e}"),
                ))
            }
        }
    }
}

pub(super) fn write_atomic(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    check_last: &mut dyn FnMut() -> Result<(), DataError>,
) -> Result<(), DataError> {
    let native = native()?;
    // Pins the record's directory too, so the rename lands where it was checked.
    let (abs, pins) = pin_ancestors(native, path)?;
    let leaf = abs
        .file_name()
        .ok_or_else(|| refused(path, "has no file name"))?
        .to_os_string();
    let tmp_abs = abs.with_file_name(
        tmp.file_name()
            .ok_or_else(|| refused(tmp, "has no file name"))?,
    );
    let mut file = (native.create_file)(&tmp_abs).map_err(|e| io_err(tmp, e))?;
    let result = (|| {
        check_leaf(&pins, tmp, &file)?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| io_err(tmp, e))?;
        check_last()?;
        // Replaces the directory entry: an existing record, even a hard link
        // or a link planted in its place, is never written through.
        (native.rename)(&file, &leaf).map_err(|e| io_err(path, e))
    })();
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_abs);
    }
    result
}

/// Cache-style open: a reparse point in place of the record is a miss.
pub(super) fn open_or_miss(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        // Backup semantics let a directory or junction open, to be reported as a miss.
        .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
        .open(path)?;
    let meta = file.metadata()?;
    if meta.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 || !meta.is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}
