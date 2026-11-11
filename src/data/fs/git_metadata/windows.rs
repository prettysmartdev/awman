//! Pin directory handles against replacement while copying inert metadata.
//! CreateFile sharing and OPEN_REPARSE_POINT semantics:
//! https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Wdk::Storage::FileSystem::{
    FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
};
use windows_sys::Win32::Foundation::{RtlNtStatusToDosError, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const REPARSE_POINT: u32 = 0x0000_0400;
const SHARE_READ: u32 = 1;

// Close the non-delete-sharing file before removing its temporary name on an
// error. NamedTempFile normally drops its path first (its files share delete).
struct PendingFile {
    file: File,
    path: tempfile::TempPath,
}

fn context(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

/// Rename within the temporary file's already-pinned parent. A leaf name with
/// a null RootDirectory requests an in-place NT rename, avoiding the writable
/// parent reopen performed by pathname-based MoveFileEx/persist. Keep denying
/// write/delete sharing on the ancestors: allowing write sharing would allow
/// an empty directory to be changed into a junction before publication.
/// https://learn.microsoft.com/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
fn publish(file: &File, destination: &Path) -> io::Result<()> {
    let name: Vec<u16> = destination.file_name().unwrap().encode_wide().collect();
    let size = std::mem::size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
    // usize storage provides the alignment required by the HANDLE field, and
    // includes trailing space for the variable-length UTF-16 filename.
    let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: info is aligned, zeroed and large enough for the header and name;
    // file is a synchronous handle opened with DELETE access, and all borrowed
    // buffers and the handle remain live for the synchronous NT call.
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            info.cast(),
            size as u32,
            FileRenameInformation,
        )
    };
    if status < 0 {
        // SAFETY: this pure conversion accepts every NTSTATUS value.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
}

fn pinned(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        // No write/delete sharing: the checked object cannot become a junction,
        // be renamed, or be replaced while a descendant is opened by pathname.
        .share_mode(SHARE_READ)
        .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
        .open(path)
        .map_err(|e| context("pin metadata", path, e))?;
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
            }
            _ => return Err(io::Error::other("non-normal metadata path")),
        }
        let handle = match pinned(&current) {
            Ok(handle) => handle,
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(context("create metadata ancestor", &current, e)),
                }
                pinned(&current)?
            }
            Err(e) => return Err(e),
        };
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
            Err(e) => return Err(context("create metadata directory", &destination, e)),
        }
        let output = pinned(&destination)?;
        if !output.metadata()?.is_dir() {
            return Err(io::Error::other("metadata destination is not a directory"));
        }
        for entry in fs::read_dir(&source).map_err(|e| context("list metadata", &source, e))? {
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
        let temporary = tempfile::Builder::new()
            .make_in(destination.parent().unwrap(), |path| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                    // Only our handle may write or rename this new entry.
                    .share_mode(SHARE_READ)
                    .open(path)
            })
            .map_err(|e| context("create metadata temporary file", &destination, e))?;
        let (file, path) = temporary.into_parts();
        let mut output = PendingFile { file, path };
        io::copy(&mut input, &mut output.file)
            .map_err(|e| context("copy metadata bytes", &destination, e))?;
        output
            .file
            .sync_all()
            .map_err(|e| context("sync metadata", &destination, e))?;
        publish(&output.file, &destination)
            .map_err(|e| context("publish metadata", &destination, e))?;
        // The handle now names destination; the temporary pathname is gone.
        output.path.disable_cleanup(true);
    } else {
        return Err(io::Error::other("unsafe Git metadata"));
    }
    Ok(())
}
