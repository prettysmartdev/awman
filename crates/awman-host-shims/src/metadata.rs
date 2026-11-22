//! Native in-place rename; callers retain their parent-directory guards.
//! Used by Git metadata copies and private-file replacement.
#![allow(unsafe_code)]
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use windows_sys::Wdk::Storage::FileSystem::{
    FileRenameInformation, FileRenameInformationEx, NtSetInformationFile, FILE_RENAME_INFORMATION,
    FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
};
use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Rename within the file's existing parent. A leaf name and null RootDirectory
/// avoid the writable parent reopen performed by pathname-based MoveFileEx.
/// The caller can keep denying write/delete sharing on its directory guards;
/// allowing write sharing would permit junction mutation before publication.
/// The file must have DELETE access. Other directories cannot be selected here.
/// https://learn.microsoft.com/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
pub fn publish(file: &File, leaf: &OsStr) -> io::Result<()> {
    rename(file, leaf, false)
}

/// [`publish`] with POSIX semantics where the file system supports them, as
/// `std::fs::rename` does: an existing target that readers still hold open
/// (with delete sharing) is replaced instead of failing with access denied.
/// Falls back to [`publish`] when the extended class is unsupported.
pub fn publish_replacing(file: &File, leaf: &OsStr) -> io::Result<()> {
    match rename(file, leaf, true) {
        // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER.
        Err(e) if matches!(e.raw_os_error(), Some(1 | 50 | 87)) => publish(file, leaf),
        other => other,
    }
}

fn rename(file: &File, leaf: &OsStr, posix: bool) -> io::Result<()> {
    let name: Vec<u16> = leaf.encode_wide().collect();
    if name.is_empty()
        || name.len() > u16::MAX as usize / 2
        || leaf == "."
        || leaf == ".."
        || name
            .iter()
            .any(|c| [0, b'/' as u16, b'\\' as u16, b':' as u16].contains(c))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid metadata leaf name",
        ));
    }
    let size = std::mem::size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
    // usize storage provides HANDLE alignment and trailing UTF-16 name space.
    let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: info is aligned, zeroed and large enough for the header and name;
    // GitMetadataCopy and private_files supply a synchronous DELETE-capable
    // file and keep it and all buffers live for the call. This callback is
    // private to those provider registrations, not an API accepting
    // arbitrary/overlapped handles. The Ex class shares the same layout with
    // the union read as flags.
    let status = unsafe {
        if posix {
            (*info).Anonymous.Flags = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
        } else {
            (*info).Anonymous.ReplaceIfExists = true;
        }
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
            if posix {
                FileRenameInformationEx
            } else {
                FileRenameInformation
            },
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
