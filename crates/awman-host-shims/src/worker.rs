//! Private worker dispatch, before the binary's async/frontend startup.
#[cfg(awman_builtin)]
use crate::{kernel, version};

/// Return true only for the reserved, first-argument internal routes.
pub fn dispatch_worker(
    environment: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    let arguments: Vec<std::ffi::OsString> = arguments.into_iter().skip(1).collect();
    let worker = is_worker_invocation(&arguments);
    let info = arguments.first().map(std::ffi::OsString::as_os_str)
        == Some(std::ffi::OsStr::new("__awman-builtin-info"));
    let kernel_check = arguments.first().map(std::ffi::OsString::as_os_str)
        == Some(std::ffi::OsStr::new("__awman-builtin-kernel-check"));
    if !worker && !info && !kernel_check {
        return false;
    }
    // The binary supplies OS strings so non-UTF-8 overrides cannot disappear
    // through the data layer's String-based environment snapshot. Never print
    // values. Build-time payload variables do not affect runtime resolution.
    for (key, _) in environment {
        if awman::data::config::env::AMBIENT_MSB_OVERRIDES
            .iter()
            .any(|name| key == std::ffi::OsStr::new(name))
        {
            eprintln!(
                "awman: ambient runtime overrides are forbidden for the builtin runtime worker"
            );
            std::process::exit(64);
        }
    }
    #[cfg(awman_builtin)]
    {
        version::retain();
        if kernel_check {
            if let Err(reason) = kernel::smoke_check() {
                eprintln!("awman: {reason}");
                std::process::exit(70);
            }
        }
        if info || kernel_check {
            println!(
                "{}",
                serde_json::json!({
                    "msb_version": awman::data::builtin_runtime::MSB_VERSION,
                    "worker_protocol": awman::data::builtin_runtime::WORKER_PROTOCOL,
                    "upgradable_from": awman::data::builtin_runtime::UPGRADABLE_FROM,
                    "target_os": std::env::consts::OS, "target_arch": std::env::consts::ARCH,
                    "libkrun_commit": awman::data::builtin_runtime::LIBKRUN_COMMIT,
                    "kernel_sha256": kernel::KERNEL_SHA256,
                    "embedded_kernel": true, "embedded_guest_agent": true,
                    "host_helpers": false, "reattach_after_owner_exit": false,
                    "checkpoint_restore": false
                })
            );
            return true;
        }
        enter(arguments);
    }
    #[cfg(not(awman_builtin))]
    {
        eprintln!("awman: this build has no builtin runtime worker");
        std::process::exit(70);
    }
}

#[cfg(awman_builtin)]
fn enter(arguments: Vec<std::ffi::OsString>) -> ! {
    use clap::Parser;
    #[derive(Parser)]
    #[command(disable_help_flag = true, disable_version_flag = true)]
    struct Worker {
        #[command(flatten)]
        args: microsandbox_cli::machine_cmd::MachineArgs,
    }
    // Parse errors are deliberately generic: private invocations must not echo argv.
    let parsed = Worker::try_parse_from(arguments);
    let Ok(parsed) = parsed else {
        eprintln!("awman: invalid private worker invocation (inherited descriptors required)");
        std::process::exit(64);
    };
    let args = parsed.args;
    let descriptors = WorkerDescriptors {
        config: args.config_fd,
        parent_watch: args.parent_watch_fd,
        lifecycle_lock: args.lifecycle_lock_fd,
        startup: args.startup_fd,
    };
    if !descriptors.valid()
        || args.config_file.is_some()
        || args.restore
        || !descriptors.sdk_slots()
        || !descriptors.open_with_expected_roles()
    {
        eprintln!(
            "awman: invalid private worker descriptors (open inherited config, pipes and lock required)"
        );
        std::process::exit(64);
    }
    if let Err(error) = kernel::prepare() {
        eprintln!("awman: {error}");
        std::process::exit(70);
    }
    // The SDK adopts descriptors exactly once, installs watchdog/startup/lock
    // ownership, enters the VMM and exits the process on VM termination.
    microsandbox_cli::machine_cmd::run(args)
}

/// The worker route is taken only for the SDK's private argv shape: first
/// argument `machine` plus an inherited `--config-fd`. A human typing
/// `awman machine --help` falls through to clap's normal error instead.
fn is_worker_invocation(arguments: &[std::ffi::OsString]) -> bool {
    arguments.first().map(std::ffi::OsString::as_os_str) == Some(std::ffi::OsStr::new("machine"))
        && arguments.iter().skip(1).any(|argument| {
            argument
                .to_str()
                .is_some_and(|a| a == "--config-fd" || a.starts_with("--config-fd="))
        })
}

/// Inherited descriptors named on the private worker argv.
///
/// awman always spawns attached (`msb_driver.rs`), and the pinned SDK then passes
/// `--config-fd`, `--parent-watch-fd` and (launch contract patch 18)
/// `--lifecycle-lock-fd`. `--startup-fd` exists only for detached spawns
/// (`microsandbox/lib/runtime/spawn.rs`), so it is optional and checked only
/// when present.
#[cfg_attr(not(awman_builtin), allow(dead_code))]
struct WorkerDescriptors {
    config: Option<i32>,
    parent_watch: Option<i32>,
    lifecycle_lock: Option<i32>,
    startup: Option<i32>,
}

#[cfg_attr(not(awman_builtin), allow(dead_code))]
impl WorkerDescriptors {
    fn valid(&self) -> bool {
        let required = [self.config, self.parent_watch, self.lifecycle_lock];
        if required.iter().any(Option::is_none) {
            return false;
        }
        let mut fds: Vec<i32> = required
            .into_iter()
            .chain([self.startup])
            .flatten()
            .collect();
        fds.sort_unstable();
        fds.iter().all(|fd| *fd >= 3) && fds.windows(2).all(|pair| pair[0] != pair[1])
    }

    /// The pinned SDK maps these descriptors to fixed slots before exec. Checking
    /// the slots prevents a caller from naming a different open host handle.
    fn sdk_slots(&self) -> bool {
        self.config == Some(96)
            && self.parent_watch == Some(97)
            && self.lifecycle_lock == Some(99)
            && self.startup.is_none_or(|fd| fd == 98)
    }

    #[cfg(unix)]
    fn open_with_expected_roles(&self) -> bool {
        self.config
            .is_some_and(|fd| descriptor_role(fd, FdRole::Config))
            && self
                .parent_watch
                .is_some_and(|fd| descriptor_role(fd, FdRole::PipeRead))
            && self
                .lifecycle_lock
                .is_some_and(|fd| descriptor_role(fd, FdRole::Lock))
            && self
                .startup
                .is_none_or(|fd| descriptor_role(fd, FdRole::PipeWrite))
    }
}

#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
#[derive(Clone, Copy)]
enum FdRole {
    Config,
    PipeRead,
    PipeWrite,
    Lock,
}

/// Probe raw inherited numbers without taking ownership. A safe `BorrowedFd`
/// cannot be made from an unverified number: its constructor requires an open
/// fd, precisely what this probe must establish. These libc calls neither
/// close nor transfer the descriptor. No descriptor number enters a diagnostic.
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
#[allow(unsafe_code)]
fn descriptor_role(fd: i32, role: FdRole) -> bool {
    // SAFETY: F_GETFD only probes the integer descriptor; invalid numbers return an error.
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return false;
    }
    // SAFETY: F_GETFL reads descriptor flags without transferring ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return false;
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable storage for one libc::stat; invalid fds fail.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: fstat returned success and initialized the complete stat struct.
    let stat = unsafe { stat.assume_init() };
    if stat.st_uid != nix::unistd::geteuid().as_raw() {
        return false;
    }
    let kind = stat.st_mode & libc::S_IFMT;
    let access = flags & libc::O_ACCMODE;
    match role {
        FdRole::Config => kind == libc::S_IFREG && stat.st_nlink == 0 && access != libc::O_WRONLY,
        FdRole::PipeRead => kind == libc::S_IFIFO && access == libc::O_RDONLY,
        FdRole::PipeWrite => kind == libc::S_IFIFO && access == libc::O_WRONLY,
        // The SDK opens the lifecycle lock with the process umask (0644 by
        // default). Only this user may be able to write it; read bits grant
        // nothing, since `flock` needs no write access.
        FdRole::Lock => {
            kind == libc::S_IFREG
                && stat.st_nlink == 1
                && stat.st_mode & 0o022 == 0
                && access == libc::O_RDWR
        }
    }
}
