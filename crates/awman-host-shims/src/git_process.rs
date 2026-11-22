//! Contain Git and its helpers in a kill-on-close Windows job.
#![allow(unsafe_code)]
use std::io;
use std::os::windows::{io::AsRawHandle, process::CommandExt};
use std::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
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
    OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

struct Job(HANDLE);
// SAFETY: exclusively owned kernel handle; Windows job operations are thread-safe.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the job handle remains owned here until the final close.
        unsafe {
            TerminateJobObject(self.0, 1);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                if QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                ) == 0
                    || info.ActiveProcesses == 0
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            CloseHandle(self.0);
        }
    }
}

pub fn spawn(command: &mut Command) -> io::Result<(Child, Box<dyn Send + Sync>)> {
    // SAFETY: unnamed job, no inherited handle; every return retains RAII ownership.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = Job(handle);
    // SAFETY: correctly sized native struct with all reserved fields zeroed.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: valid handle and pointer for the supplied size and class.
    if unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Contain the child before it can start a helper. Never attach after running.
    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let setup = (|| {
        // SAFETY: both handles are live and owned for the entire call.
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        resume(child.id())
    })();
    if let Err(error) = setup {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, Box::new(job)))
}

fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: snapshot handle is checked and closed on every path below.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        let mut resumed = false;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    resumed |= ResumeThread(thread) != u32::MAX;
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if resumed {
            Ok(())
        } else {
            Err(io::Error::other("could not resume contained Git process"))
        }
    }
}
