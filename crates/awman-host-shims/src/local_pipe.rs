//! Minimal native ACL boundary for the owner-only daemon named pipe.
#![allow(unsafe_code)]
use std::path::Path;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::{
    Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
};

pub fn create(endpoint: &Path, first: bool) -> std::io::Result<NamedPipeServer> {
    // Protected DACL: only the object's owner has generic-all access. Reject
    // remote pipe clients as well; no local bearer key is generated or stored.
    let sddl: Vec<u16> = "D:P(A;;GA;;;OW)\0".encode_utf16().collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: sddl is NUL-terminated and descriptor is a valid output pointer.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: the descriptor and attributes remain alive throughout creation.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                endpoint,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    };
    // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW allocates with LocalAlloc.
    unsafe {
        LocalFree(descriptor.cast());
    }
    result
}
