//! Native in-place rename; callers retain their parent-directory guards.
#![allow(unsafe_code)]
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use windows_sys::Wdk::Storage::FileSystem::{
    FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
};
use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Rename within the file's existing parent. A leaf name and null RootDirectory
/// avoid the writable parent reopen performed by pathname-based MoveFileEx.
/// The caller can keep denying write/delete sharing on its directory guards;
/// allowing write sharing would permit junction mutation before publication.
/// The file must have DELETE access. Other directories cannot be selected here.
/// https://learn.microsoft.com/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
pub(super) fn publish(file: &File, leaf: &OsStr) -> io::Result<()> {
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
    // GitMetadataCopy supplies a synchronous DELETE-capable file and keeps it
    // and all buffers live for the call. This callback is private to that
    // provider registration, not an API accepting arbitrary/overlapped handles.
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
