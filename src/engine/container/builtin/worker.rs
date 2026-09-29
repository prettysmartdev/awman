//! Included only by the binary, before its async/frontend startup.
#[cfg(awman_builtin)]
#[path = "embedded/kernel.rs"]
mod kernel;
#[cfg(awman_builtin)]
#[path = "embedded/version.rs"]
mod version;

/// Return true only for the reserved, first-argument internal routes.
pub fn dispatch(
    environment: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> bool {
    let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let worker = is_worker_invocation(&arguments);
    let info = arguments.first().map(std::ffi::OsString::as_os_str)
        == Some(std::ffi::OsStr::new("__awman-builtin-info"));
    if !worker && !info {
        return false;
    }
    // The binary supplies OS strings so non-UTF-8 overrides cannot disappear
    // through the data layer's String-based environment snapshot. Never print
    // values. Build-time payload variables do not affect runtime resolution.
    for (key, _) in environment {
        if matches!(
            key.to_str(),
            Some(
                "MSB_PATH"
                    | "MSB_LIBKRUNFW_PATH"
                    | "MSB_AGENTD_PATH"
                    | "MSB_HOME"
                    | "MSB_BACKEND"
                    | "MSB_CONFIG_PATH"
                    | "MSB_PROFILE"
                    | "MSB_CACHE_DIR"
                    | "MSB_SANDBOXES_DIR"
                    | "MSB_VOLUMES_DIR"
                    | "MSB_SNAPSHOTS_DIR"
                    | "MSB_LOGS_DIR"
                    | "MSB_SECRETS_DIR"
            )
        ) {
            eprintln!(
                "awman: ambient Microsandbox overrides are forbidden for the private embedded worker"
            );
            std::process::exit(64);
        }
    }
    #[cfg(awman_builtin)]
    {
        version::retain();
        if info {
            println!(
                "{}",
                serde_json::json!({
                    "msb_version": "0.7.2", "worker_protocol": "0.7.2/18/awman-1",
                    "target_os": std::env::consts::OS, "target_arch": std::env::consts::ARCH,
                    "libkrun_commit": "2bd0f84ad0956f3032e0490d3b8512b6851eca12",
                    "kernel_sha256": kernel::KERNEL_SHA256,
                    "embedded_kernel": true, "embedded_guest_agent": true,
                    "host_helpers": false, "reattach_after_owner_exit": false,
                    "checkpoint_restore": false
                })
            );
            return true;
        }
        enter();
    }
    #[cfg(not(awman_builtin))]
    {
        eprintln!("awman: this build has no builtin runtime worker");
        std::process::exit(70);
    }
}

#[cfg(awman_builtin)]
fn enter() -> ! {
    use clap::Parser;
    #[derive(Parser)]
    #[command(disable_help_flag = true, disable_version_flag = true)]
    struct Worker {
        #[command(flatten)]
        args: microsandbox_cli::machine_cmd::MachineArgs,
    }
    // Parse errors are deliberately generic: private invocations must not echo argv.
    let parsed = Worker::try_parse_from(std::env::args_os().skip(1));
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
    if let Err(error) = kernel::register() {
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
/// awman always spawns attached (`msb_driver.rs`), and SDK 0.7.2 then passes
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

    /// SDK 0.7.2 maps these descriptors to fixed slots before exec. Checking
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
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return false;
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return false;
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    fn descriptors(
        config: Option<i32>,
        parent_watch: Option<i32>,
        lifecycle_lock: Option<i32>,
        startup: Option<i32>,
    ) -> WorkerDescriptors {
        WorkerDescriptors {
            config,
            parent_watch,
            lifecycle_lock,
            startup,
        }
    }

    /// Regression: the SDK never passes `--startup-fd` for attached spawns,
    /// which is the only mode awman uses. The worker must accept that shape.
    #[test]
    fn attached_sdk_descriptor_shape_is_accepted() {
        assert!(descriptors(Some(3), Some(5), Some(4), None).valid());
    }

    #[test]
    fn detached_shape_with_startup_fd_is_accepted() {
        assert!(descriptors(Some(3), Some(5), Some(4), Some(6)).valid());
    }

    #[test]
    fn missing_required_descriptor_is_rejected() {
        assert!(!descriptors(None, Some(5), Some(4), None).valid());
        assert!(!descriptors(Some(3), None, Some(4), None).valid());
        assert!(!descriptors(Some(3), Some(5), None, None).valid());
    }

    #[test]
    fn stdio_or_duplicate_descriptors_are_rejected() {
        assert!(!descriptors(Some(2), Some(5), Some(4), None).valid());
        assert!(!descriptors(Some(3), Some(3), Some(4), None).valid());
        assert!(!descriptors(Some(3), Some(5), Some(4), Some(5)).valid());
        assert!(!descriptors(Some(3), Some(5), Some(4), Some(0)).valid());
    }

    #[test]
    fn sdk_slots_reject_swapped_or_arbitrary_descriptors() {
        assert!(descriptors(Some(96), Some(97), Some(99), None).sdk_slots());
        assert!(descriptors(Some(96), Some(97), Some(99), Some(98)).sdk_slots());
        assert!(!descriptors(Some(97), Some(96), Some(99), None).sdk_slots());
        assert!(!descriptors(Some(3), Some(4), Some(5), None).sdk_slots());
    }

    #[cfg(unix)]
    #[test]
    fn inherited_fd_probe_checks_open_state_type_and_access() {
        use std::os::fd::AsRawFd;
        let (read, write) = nix::unistd::pipe().unwrap();
        let (watch_read, _watch_write) = nix::unistd::pipe().unwrap();
        let config = tempfile::tempfile().unwrap();
        let lock = tempfile::NamedTempFile::new().unwrap();
        let lock_fd = lock.as_file().as_raw_fd();
        let readonly = std::fs::File::open(lock.path()).unwrap();
        assert!(descriptor_role(config.as_raw_fd(), FdRole::Config));
        assert!(!descriptor_role(lock_fd, FdRole::Config));
        assert!(!descriptor_role(read.as_raw_fd(), FdRole::Config));
        assert!(descriptor_role(read.as_raw_fd(), FdRole::PipeRead));
        assert!(descriptor_role(write.as_raw_fd(), FdRole::PipeWrite));
        assert!(descriptor_role(lock_fd, FdRole::Lock));
        assert!(!descriptor_role(read.as_raw_fd(), FdRole::Lock));
        assert!(!descriptor_role(write.as_raw_fd(), FdRole::PipeRead));
        assert!(!descriptor_role(lock_fd, FdRole::PipeRead));
        assert!(!descriptor_role(readonly.as_raw_fd(), FdRole::Lock));
        // The SDK's real lock mode is accepted; group/other-writable is not.
        use std::os::unix::fs::PermissionsExt;
        for (mode, ok) in [(0o644, true), (0o600, true), (0o664, false), (0o666, false)] {
            std::fs::set_permissions(lock.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(descriptor_role(lock_fd, FdRole::Lock), ok, "mode {mode:o}");
        }
        std::fs::set_permissions(lock.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let valid = descriptors(
            Some(config.as_raw_fd()),
            Some(watch_read.as_raw_fd()),
            Some(lock_fd),
            Some(write.as_raw_fd()),
        );
        assert!(valid.valid() && valid.open_with_expected_roles());
        let wrong = descriptors(
            Some(read.as_raw_fd()),
            Some(watch_read.as_raw_fd()),
            Some(lock_fd),
            None,
        );
        assert!(!wrong.open_with_expected_roles());
        let closed = read.as_raw_fd();
        drop(read);
        assert!(!descriptor_role(closed, FdRole::PipeRead));
        let closed_set = descriptors(Some(config.as_raw_fd()), Some(closed), Some(lock_fd), None);
        assert!(!closed_set.open_with_expected_roles());
    }

    #[test]
    fn worker_route_requires_machine_and_config_fd() {
        // Exact attached argv shape rendered by SDK 0.7.2 `spawn.rs`.
        assert!(is_worker_invocation(&argv(&[
            "machine",
            "--name",
            "awman-x",
            "--sandbox-id",
            "7",
            "--parent-watch-fd",
            "5",
            "--vcpus",
            "2",
            "--memory-mib",
            "2048",
            "--config-fd",
            "3",
            "--lifecycle-lock-fd",
            "4",
        ])));
        assert!(is_worker_invocation(&argv(&["machine", "--config-fd=3"])));
        assert!(!is_worker_invocation(&argv(&["machine", "--help"])));
        assert!(!is_worker_invocation(&argv(&["machine"])));
        assert!(!is_worker_invocation(&argv(&[
            "status",
            "--config-fd",
            "3"
        ])));
    }

    /// Regression for the attached launch: parse the SDK-rendered argv with
    /// the SDK's own `MachineArgs` and run the worker's descriptor check.
    #[cfg(awman_builtin)]
    #[test]
    fn sdk_attached_argv_parses_and_validates() {
        use clap::Parser;
        #[derive(Parser)]
        struct Worker {
            #[command(flatten)]
            args: microsandbox_cli::machine_cmd::MachineArgs,
        }
        let parsed = Worker::try_parse_from([
            "machine",
            "--name",
            "awman-x",
            "--sandbox-id",
            "7",
            "--parent-watch-fd",
            "5",
            "--vcpus",
            "2",
            "--memory-mib",
            "2048",
            "--config-fd",
            "3",
            "--lifecycle-lock-fd",
            "4",
        ])
        .expect("SDK attached argv must parse")
        .args;
        assert!(parsed.startup_fd.is_none());
        assert!(WorkerDescriptors {
            config: parsed.config_fd,
            parent_watch: parsed.parent_watch_fd,
            lifecycle_lock: parsed.lifecycle_lock_fd,
            startup: parsed.startup_fd,
        }
        .valid());
    }
}
