//! Windows: every child runs in its own job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The child is created suspended,
//! assigned to the job, and only then resumed, so no descendant can start
//! outside the job (design 16.7). Killing = terminating the job.
//!
//! This is the one module of the crate that may use `unsafe`: the job-object
//! API has no safe wrapper in the locked dependency set. Each block says why
//! it is sound.
//!
//! NOT VERIFIED in the WI 0144 foundation container (Linux only); see the
//! step handoff. CI's windows cells compile and run it.

#![allow(unsafe_code)]

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenThread, ResumeThread, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

/// The job object of one supervised child.
#[derive(Debug)]
pub(crate) struct Group {
    job: HANDLE,
    pid: u32,
}

// SAFETY: a job-object HANDLE is a kernel handle value usable from any
// thread; the struct owns it exclusively and closes it once in Drop.
unsafe impl Send for Group {}
// SAFETY: every method only passes the handle to thread-safe kernel calls.
unsafe impl Sync for Group {}

pub(crate) fn prepare(cmd: &mut Command) {
    cmd.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
}

/// Windows standard library handles are created non-inheritable; nothing to do.
pub(crate) fn before_spawn() {}

pub(crate) fn after_spawn(child: &std::process::Child) -> std::io::Result<Group> {
    // SAFETY: null attributes and name are allowed; the result is checked.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let group = Group {
        job,
        pid: child.id(),
    };
    // SAFETY: `info` is a properly sized, zero-initialised plain C struct.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: valid job handle, pointer and size of the matching struct.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both handles are valid and owned for the duration of the call.
    let ok = unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    resume_threads(child.id())?;
    Ok(group)
}

/// Resume every thread of the (suspended) process `pid`.
fn resume_threads(pid: u32) -> std::io::Result<()> {
    // SAFETY: snapshot of all threads; the handle is checked and closed.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: zero-initialised C struct with its size field set as required.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0;
    // SAFETY: valid snapshot handle and entry pointer.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: thread ID from the snapshot; the handle is checked and closed.
            unsafe {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    ResumeThread(thread);
                    CloseHandle(thread);
                    resumed += 1;
                }
            }
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: snapshot handle owned here.
    unsafe { CloseHandle(snapshot) };
    if resumed == 0 {
        return Err(std::io::Error::other(
            "could not resume the suspended child",
        ));
    }
    Ok(())
}

impl Group {
    pub(crate) fn id(&self) -> i64 {
        i64::from(self.pid)
    }

    pub(crate) fn kill(&self) {
        // SAFETY: valid job handle owned by self.
        unsafe { TerminateJobObject(self.job, 1) };
    }

    pub(crate) fn terminate(&self) {
        self.kill();
    }

    pub(crate) fn interrupt(&self) {
        // SAFETY: plain call with a process-group ID (the child was created
        // with CREATE_NEW_PROCESS_GROUP, so its pid is the group ID).
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid) };
    }

    pub(crate) fn has_members(&self) -> bool {
        // SAFETY: zero-initialised C struct, valid handle, correct size.
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let ok = QueryInformationJobObject(
                self.job,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            );
            ok != 0 && info.ActiveProcesses > 0
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: closing the owned handle kills any survivor (KILL_ON_JOB_CLOSE).
        unsafe { CloseHandle(self.job) };
    }
}

/// `ERROR_SHARING_VIOLATION`: the executable is still open for writing.
pub(crate) fn is_text_file_busy(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(32)
}

/// Job objects contain every supervised descendant before it starts; the
/// group's accounting probe above is the Windows escape check.
pub(crate) fn escaped_processes(_root: &std::path::Path) -> Vec<crate::Leak> {
    Vec::new()
}
