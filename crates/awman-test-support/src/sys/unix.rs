//! Unix: every child leads its own process group; killing the group kills
//! the tree. Descriptors other than 0–2 are marked close-on-exec in the
//! parent before each spawn, so a child never inherits a stray descriptor
//! (fork-inherited locks, design 8.8: e6a2d429, e6322996, f5b724fc).

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The process group of one supervised child (its pgid equals its pid).
#[derive(Debug)]
pub(crate) struct Group {
    pgid: i32,
}

/// Configure `cmd` so the child leads a new process group.
pub(crate) fn prepare(cmd: &mut Command) {
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    prevent_group_escape(cmd);
}

/// Descendants inherit this kernel filter across fork/exec. They may create
/// children, but cannot start a new session. Group creation remains available
/// for the runner contract; the root ownership probe below catches such escapes.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn prevent_group_escape(cmd: &mut Command) {
    // SAFETY: pre_exec uses only stack data and async-signal-safe prctl calls.
    // The kernel copies the filter before the stack storage goes out of scope.
    unsafe {
        cmd.pre_exec(|| {
            let mut filter = [
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 1,
                    jf: 0,
                    k: libc::SYS_setsid as u32,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x7fff0000,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x00050000 | libc::EPERM as u32,
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_SECCOMP, 2, &program) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Run before every spawn: mark every open descriptor above 2 close-on-exec.
pub(crate) fn before_spawn() {
    mark_inherited_descriptors_cloexec();
}

/// Called right after a successful spawn.
pub(crate) fn after_spawn(child: &std::process::Child) -> std::io::Result<Group> {
    Ok(Group {
        pgid: child.id() as i32,
    })
}

impl Group {
    /// The process-group ID (for evidence).
    pub(crate) fn id(&self) -> i64 {
        i64::from(self.pgid)
    }

    /// SIGKILL to every member. Errors (group already gone) are ignored.
    pub(crate) fn kill(&self) {
        let _ = killpg(Pid::from_raw(self.pgid), Signal::SIGKILL);
    }

    /// SIGTERM to every member.
    pub(crate) fn terminate(&self) {
        let _ = killpg(Pid::from_raw(self.pgid), Signal::SIGTERM);
    }

    /// SIGINT to every member (what a terminal's Ctrl-C does).
    pub(crate) fn interrupt(&self) {
        let _ = killpg(Pid::from_raw(self.pgid), Signal::SIGINT);
    }

    /// Whether any member is still alive. Call only after the leader was
    /// reaped and before the group was observed empty (identity is not
    /// reused while any member lives).
    pub(crate) fn has_members(&self) -> bool {
        killpg(Pid::from_raw(self.pgid), None).is_ok()
    }
}

/// Whether `err` is "text file busy" (an executable still open for writing
/// in some forked-but-not-yet-exec'd process).
pub(crate) fn is_text_file_busy(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(libc::ETXTBSY)
}

fn open_descriptors() -> Vec<i32> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (3..256).collect();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
        .filter(|fd: &i32| *fd > 2)
        .collect()
}

#[allow(unsafe_code)]
fn mark_inherited_descriptors_cloexec() {
    for fd in open_descriptors() {
        // SAFETY: F_GETFD/F_SETFD only read and set the close-on-exec flag of
        // a descriptor number; they do not touch memory. A number that is not
        // (or no longer) an open descriptor makes fcntl fail with EBADF,
        // which is ignored. No Rust object's invariants depend on this flag.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Linux fallback ownership probe: exact inherited private roots, never argv substrings.
/// This complements group cleanup; a delegated cgroup is still required for hostile
/// programs that deliberately erase their environment (pending native containment).
#[cfg(target_os = "linux")]
pub(crate) fn escaped_processes(root: &std::path::Path) -> Vec<crate::Leak> {
    let mut leaks = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return leaks;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if pid == std::process::id() as i32 {
            continue;
        }
        let path = entry.path();
        let identity = || -> Option<String> {
            let stat = std::fs::read_to_string(path.join("stat")).ok()?;
            let fields = stat
                .rsplit_once(") ")?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            if fields.first() == Some(&"Z") {
                return None;
            }
            Some(fields.get(19)?.to_string())
        };
        let Some(start) = identity() else { continue };
        let Ok(env) = std::fs::read(path.join("environ")) else {
            continue;
        };
        let owned = env.split(|b| *b == 0).any(|e| {
            [
                b"HOME=".as_slice(),
                b"TMPDIR=".as_slice(),
                b"AWMAN_HOME=".as_slice(),
            ]
            .iter()
            .any(|key| {
                e.strip_prefix(*key)
                    .and_then(|value| std::str::from_utf8(value).ok())
                    .is_some_and(|value| std::path::Path::new(value).starts_with(root))
            })
        });
        if owned && identity().as_deref() == Some(start.as_str()) {
            let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
            leaks.push(crate::Leak::new(
                "process",
                format!("pid {pid} escaped its group with a private sandbox root"),
            ));
        }
    }
    leaks
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn escaped_processes(_root: &std::path::Path) -> Vec<crate::Leak> {
    Vec::new()
}
