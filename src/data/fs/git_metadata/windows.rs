//! Pin directory handles against replacement while copying inert metadata.
//! CreateFile sharing and OPEN_REPARSE_POINT semantics:
//! https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf, Prefix};

const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const REPARSE_POINT: u32 = 0x0000_0400;
const SHARE_READ: u32 = 1;

fn pinned(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        // No write/delete sharing: the checked object cannot become a junction,
        // be renamed, or be replaced while a descendant is opened by pathname.
        .share_mode(SHARE_READ)
        .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
        .open(path)?;
    if file.metadata()?.file_attributes() & REPARSE_POINT != 0 {
        return Err(io::Error::other("Git metadata traverses a reparse point"));
    }
    Ok(file)
}

fn parents(path: &Path, create: bool) -> io::Result<(PathBuf, Vec<File>)> {
    let path = std::path::absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing metadata parent"))?;
    let mut current = PathBuf::new();
    let mut handles = Vec::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => {
                if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
                    return Err(io::Error::other("Git metadata must be on a local disk"));
                }
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(name) => {
                if name.to_string_lossy().contains(':') {
                    return Err(io::Error::other("metadata stream path refused"));
                }
                current.push(name);
                if create {
                    match fs::create_dir(&current) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            _ => return Err(io::Error::other("non-normal metadata path")),
        }
        let handle = pinned(&current)?;
        if !handle.metadata()?.is_dir() {
            return Err(io::Error::other("metadata ancestor is not a directory"));
        }
        handles.push(handle);
    }
    if path
        .file_name()
        .is_none_or(|name| name.to_string_lossy().contains(':'))
    {
        return Err(io::Error::other("invalid metadata filename"));
    }
    Ok((path, handles))
}

pub(super) fn copy(source: &Path, destination: &Path) -> io::Result<()> {
    let (source, _source_parents) = parents(source, false)?;
    let (destination, _destination_parents) = parents(destination, true)?;
    let mut input = pinned(&source)?;
    let metadata = input.metadata()?;
    if metadata.is_dir() {
        match fs::create_dir(&destination) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let output = pinned(&destination)?;
        if !output.metadata()?.is_dir() {
            return Err(io::Error::other("metadata destination is not a directory"));
        }
        for entry in fs::read_dir(&source)? {
            let entry = entry?;
            copy(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        match pinned(&destination) {
            Ok(existing) if !existing.metadata()?.is_file() => {
                return Err(io::Error::other("unsafe metadata destination"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Replace the directory entry rather than truncating an existing inode:
        // even a concurrently substituted hard link cannot modify another file.
        let mut output = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
        io::copy(&mut input, output.as_file_mut())?;
        output.as_file().sync_all()?;
        output.persist(&destination).map_err(|e| e.error)?;
    } else {
        return Err(io::Error::other("unsafe Git metadata"));
    }
    Ok(())
}
