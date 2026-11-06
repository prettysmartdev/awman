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

/// After `connect`: is the pipe's caller running as this process's user?
/// GetNamedPipeClientProcessId → OpenProcess → OpenProcessToken → TokenUser
/// SID, compared with this process's token user SID. Any failure is an error,
/// which the caller treats as "not the owner" (fail closed).
pub fn verify_client(server: &NamedPipeServer) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut pid: u32 = 0;
    // SAFETY: the handle is a live pipe server owned by `server`; pid is a valid out pointer.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: plain FFI call; a null result is checked below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let client = OwnedHandle(process);
    let client_user = token_user(client.0);
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    let own_user = token_user(unsafe { GetCurrentProcess() });
    let (client_user, own_user) = (client_user?, own_user?);
    // SAFETY: both buffers start with a TOKEN_USER whose SID points into the buffer.
    Ok(unsafe { EqualSid(sid_of(&client_user), sid_of(&own_user)) } != 0)
}

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
};

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The TOKEN_USER of `process`, in a u64-aligned buffer.
fn token_user(process: HANDLE) -> std::io::Result<Vec<u64>> {
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: process is a valid process handle; token is a valid out pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut needed: u32 = 0;
    // SAFETY: size query with a null buffer; failure with a length is expected.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: buffer holds at least `needed` bytes and is suitably aligned for TOKEN_USER.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(buffer)
}

fn sid_of(buffer: &[u64]) -> *mut core::ffi::c_void {
    let user = buffer.as_ptr().cast::<TOKEN_USER>();
    // SAFETY: only called on buffers filled by GetTokenInformation(TokenUser), u64-aligned.
    unsafe { (*user).User.Sid }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn this_process_is_its_own_owner() {
        let name = format!(r"\\.\pipe\awman-verify-{}", std::process::id());
        let server = super::create(std::path::Path::new(&name), true).unwrap();
        let _client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .unwrap();
        server.connect().await.unwrap();
        assert!(super::verify_client(&server).unwrap());
    }
}
