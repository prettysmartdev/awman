//! Native boundary for awman's owner-only stores on Windows.
//!
//! The library keeps the policy (which paths are pinned, what is refused and
//! how it is reported) in safe code. This module does only what needs Win32
//! calls: creating a directory or file with an explicit owner-only security
//! descriptor, and inspecting an already opened handle (object identity,
//! owner and DACL). Nothing here changes an existing object's ACL.
//!
//! Trust model for an existing object, checked on the handle, not the path:
//! - it is not a reparse point (no symbolic link, junction or mount point);
//! - a non-directory has exactly one link;
//! - its owner is this token's user or its default owner (an elevated
//!   administrator's default owner is BUILTIN\Administrators);
//! - its DACL exists and every allow entry names this user, the default owner,
//!   LocalSystem, BUILTIN\Administrators or OWNER RIGHTS, or is an
//!   inherit-only CREATOR OWNER template. Deny entries only narrow access.
//!   Any other allow entry or entry type is a refusal.
//!
//! Security descriptors and ACL layout:
//! https://learn.microsoft.com/windows/win32/secauthz/security-descriptor-string-format
//! https://learn.microsoft.com/windows/win32/api/aclapi/nf-aclapi-getsecurityinfo
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, GetAce, GetAclInformation, GetTokenInformation, TokenOwner, TokenUser,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_OWNER,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// ACCESS_DENIED, ACCESS_DENIED_OBJECT, ACCESS_DENIED_CALLBACK and
/// ACCESS_DENIED_CALLBACK_OBJECT: entries that can only remove access.
const DENY_ACE_TYPES: [u8; 4] = [1, 6, 10, 12];
const INHERIT_ONLY_ACE: u8 = 0x08;
const LOCAL_SYSTEM: &str = "S-1-5-18";
const BUILTIN_ADMINISTRATORS: &str = "S-1-5-32-544";
const OWNER_RIGHTS: &str = "S-1-3-4";
const CREATOR_OWNER: &str = "S-1-3-0";

/// Create the directory `path` (its parent must exist) owned by this user,
/// with a protected DACL granting only this user, inherited by its children.
/// An existing entry is `AlreadyExists`; it is never adopted or changed.
pub fn create_dir(path: &Path) -> io::Result<()> {
    let name = wide(path)?;
    let descriptor = owner_only_descriptor(true)?;
    let attributes = descriptor.attributes();
    // SAFETY: name is NUL-terminated; attributes points at a live descriptor.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create the file `path` exclusively (never through a link), owned by this
/// user with a protected owner-only DACL, open for read, write and DELETE (the
/// last for [`crate::publish_private_file`]). Sharing matches std's default so
/// other awman processes can open a lock file.
pub fn create_file(path: &Path) -> io::Result<File> {
    let name = wide(path)?;
    let descriptor = owner_only_descriptor(false)?;
    let attributes = descriptor.attributes();
    // SAFETY: name is NUL-terminated; attributes points at a live descriptor;
    // the template handle may be null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a valid handle that nothing else owns.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// Inspect the object behind `file` (opened with READ_CONTROL, e.g. any read
/// access). `Ok(None)` when it is private by the trust model above;
/// `Ok(Some(reason))` names the first problem; `Err` means the facts could not
/// be read, which callers must also treat as a refusal.
pub fn verify(file: &File) -> io::Result<Option<String>> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION; handle is live.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(Some(
            "is a link, junction or other reparse point (these are refused)".into(),
        ));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.nNumberOfLinks != 1 {
        return Ok(Some("has other hard links".into()));
    }

    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: all out pointers are valid; owner and dacl point into the
    // returned descriptor, which LocalDescriptor frees after their last use.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = LocalDescriptor(descriptor);
    let me = identity()?;
    if owner.is_null() {
        return Ok(Some("has no owner".into()));
    }
    let owner = sid_string(owner)?;
    if owner != me.user && owner != me.default_owner {
        return Ok(Some(format!("is owned by another account ({owner})")));
    }
    if dacl.is_null() {
        return Ok(Some(
            "has no access control list, so every account can access it".into(),
        ));
    }
    // SAFETY: zeroed is a valid ACL_SIZE_INFORMATION; dacl is a live ACL.
    let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    for index in 0..size.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: index < AceCount of a live ACL; ace is a valid out pointer.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { std::ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
        if DENY_ACE_TYPES.contains(&header.AceType) {
            continue;
        }
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return Ok(Some(format!(
                "has an access entry of unsupported type {}",
                header.AceType
            )));
        }
        let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: the header says this is an ACCESS_ALLOWED_ACE; its SID
        // starts at SidStart inside the ACE.
        let (mask, sid) = unsafe {
            (
                std::ptr::read_unaligned(std::ptr::addr_of!((*allowed).Mask)),
                std::ptr::addr_of!((*allowed).SidStart) as PSID,
            )
        };
        if mask == 0 {
            continue;
        }
        let trustee = sid_string(sid)?;
        let trusted = trustee == me.user
            || trustee == me.default_owner
            || [LOCAL_SYSTEM, BUILTIN_ADMINISTRATORS, OWNER_RIGHTS].contains(&trustee.as_str())
            || (header.AceFlags & INHERIT_ONLY_ACE != 0 && trustee == CREATOR_OWNER);
        if !trusted {
            return Ok(Some(format!(
                "grants access to another account ({trustee})"
            )));
        }
    }
    Ok(None)
}

/// This process's token user and default owner, as SID strings.
struct Identity {
    user: String,
    default_owner: String,
}

fn identity() -> io::Result<&'static Identity> {
    static IDENTITY: OnceLock<Identity> = OnceLock::new();
    if let Some(identity) = IDENTITY.get() {
        return Ok(identity);
    }
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess is a pseudo-handle; token is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let user = token_information(token.0, TokenUser)?;
    let owner = token_information(token.0, TokenOwner)?;
    // SAFETY: both buffers were filled by GetTokenInformation for their class
    // and are u64-aligned; the SIDs point into the buffers.
    let identity = unsafe {
        Identity {
            user: sid_string((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid)?,
            default_owner: sid_string((*owner.as_ptr().cast::<TOKEN_OWNER>()).Owner)?,
        }
    };
    Ok(IDENTITY.get_or_init(|| identity))
}

fn token_information(token: HANDLE, class: i32) -> io::Result<Vec<u64>> {
    let mut needed: u32 = 0;
    // SAFETY: size query with a null buffer; failure with a length is expected.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: buffer holds at least `needed` bytes and is 8-byte aligned.
    if unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: sid is a valid SID; text is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the function returned a NUL-terminated LocalAlloc'd string,
    // read once here and then freed.
    unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(value)
    }
}

/// A security descriptor allocated by the system with LocalAlloc.
struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl LocalDescriptor {
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the system with LocalAlloc; freed once.
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

/// Owner = this user; protected DACL (nothing inherited from the parent) with
/// one full-control entry for this user, inherited by children of a directory.
fn owner_only_descriptor(directory: bool) -> io::Result<LocalDescriptor> {
    let user = &identity()?.user;
    let inherit = if directory { "OICI" } else { "" };
    let sddl: Vec<u16> = format!("O:{user}D:P(A;{inherit};FA;;;{user})\0")
        .encode_utf16()
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: sddl is NUL-terminated and descriptor is a valid out pointer.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalDescriptor(descriptor))
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.is_empty() || name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid private path",
        ));
    }
    name.push(0);
    Ok(name)
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The normalized `\\?\` path of the object behind `file` as the system
/// resolves it now. Callers compare a child's parent with the pinned parent's
/// path to detect a directory that was swapped for a junction mid-walk.
pub fn final_path(file: &File) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let handle = file.as_raw_handle() as HANDLE;
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: buffer is valid for `len` UTF-16 units; flags 0 request the
        // normalized name with a DOS volume prefix.
        let len = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        } as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < buffer.len() {
            return Ok(std::ffi::OsString::from_wide(&buffer[..len]).into());
        }
        // Too small: `len` is the size needed including the terminator.
        buffer.resize(len + 1, 0);
    }
}
