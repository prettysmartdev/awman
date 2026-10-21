//! Transport boundary: runtime policy and frontend I/O never depend on SDK types.
use crate::data::session::AgentHandle;
use crate::engine::container::builtin::network::NetworkPlan;
use crate::engine::{agent_runtime::execution::AgentStats, error::EngineError};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
pub struct SandboxSpec {
    pub name: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub mounts: Vec<MountSpec>,
    pub mount_owner: Option<(u32, u32)>,
    pub vcpus: u8,
    pub memory_mib: u32,
    /// Size of the writable root disk (everything outside the mounts).
    pub root_disk_mib: u32,
    /// How much the guest may add to each mount beyond its existing contents.
    pub mount_quota_mib: u32,
    /// Compiled network policy. The driver applies all of it or refuses to
    /// create the VM; it never falls back to the SDK's defaults.
    pub network: NetworkPlan,
}
impl SandboxSpec {
    /// Refusals every driver applies before creating a VM. Each mount must be
    /// a regular file or directory: a Unix socket (such as the Docker
    /// socket), FIFO or device node shared into a guest is not a working or
    /// sanctioned bridge, so it is refused rather than passed through.
    pub fn check(&self) -> Result<(), EngineError> {
        if self.vcpus == 0 {
            return Err(EngineError::Config(
                "builtin VM needs at least one vCPU".into(),
            ));
        }
        if self.memory_mib < crate::data::config::builtin_runtime::MIN_MEMORY_MIB {
            return Err(EngineError::Config(format!(
                "builtin VM memory {} MiB is below the {} MiB minimum",
                self.memory_mib,
                crate::data::config::builtin_runtime::MIN_MEMORY_MIB
            )));
        }
        if self.root_disk_mib < crate::data::config::builtin_runtime::MIN_ROOT_DISK_MIB {
            return Err(EngineError::Config(format!(
                "builtin VM root disk {} MiB is below the {} MiB minimum",
                self.root_disk_mib,
                crate::data::config::builtin_runtime::MIN_ROOT_DISK_MIB
            )));
        }
        if self.mount_quota_mib == 0 {
            return Err(EngineError::Config(
                "builtin mount quota must be at least 1 MiB".into(),
            ));
        }
        for mount in &self.mounts {
            let metadata =
                std::fs::metadata(&mount.host).map_err(|e| EngineError::io(&mount.host, e))?;
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(EngineError::Config(format!(
                    "cannot share {} into the guest at {}: only regular files and directories can \
                     be mounted; sockets such as the Docker socket are not bridged",
                    mount.host.display(),
                    mount.guest
                )));
            }
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct MountSpec {
    pub host: PathBuf,
    pub guest: String,
    pub read_only: bool,
}
#[derive(Clone, Default)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    pub env: Vec<(String, String)>,
    pub tty: Option<(u16, u16)>,
    pub stdin: bool,
}
#[derive(Clone)]
pub struct SandboxSummary {
    pub handle: AgentHandle,
    pub labels: BTreeMap<String, String>,
    pub stopped: bool,
}
#[derive(Clone, Default)]
pub struct SandboxImageConfig {
    pub home: Option<String>,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub argv: Vec<String>,
    /// Effective `/etc/passwd` and `/etc/group` of the image rootfs (whiteouts
    /// applied), read only when `user` names an account.
    pub accounts: ImageAccounts,
}
#[derive(Clone, Default)]
pub struct ImageAccounts {
    pub passwd: Option<String>,
    pub group: Option<String>,
}
#[derive(Clone)]
pub enum ExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited(i32),
    Failed,
}
pub enum Control {
    Stdin(Vec<u8>),
    Eof,
    Resize(u16, u16),
    Interrupt,
    Kill,
}
pub trait ExecControl: Send + Sync {
    fn send(&self, command: Control) -> Result<(), EngineError>;
}
pub struct ExecSession {
    pub events: tokio::sync::broadcast::Receiver<ExecEvent>,
    pub broadcast: tokio::sync::broadcast::Sender<ExecEvent>,
    pub control: Arc<dyn ExecControl>,
}
/// Whether this host exposes the hypervisor the guest needs. Linux requires
/// read/write access to `/dev/kvm`; on macOS the worker's startup handshake
/// reports entitlement/HVF failures, because signing cannot be probed here.
pub fn host_hypervisor() -> Result<(), EngineError> {
    #[cfg(target_os = "linux")]
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|e| EngineError::BuiltinRuntimeUnavailable {
            reason: format!(
                "cannot access /dev/kvm: {e}; enable KVM and grant this user read/write access"
            ),
        })?;
    Ok(())
}

pub trait SandboxDriver: Send + Sync {
    /// Hypervisor probe. Drivers that never boot a guest (test doubles)
    /// override this; the native driver keeps the host check.
    fn hypervisor_available(&self) -> Result<(), EngineError> {
        host_hypervisor()
    }
    fn create(&self, spec: SandboxSpec) -> Result<(), EngineError>;
    fn get(&self, id: &str) -> Result<SandboxSummary, EngineError>;
    fn list(&self) -> Result<Vec<SandboxSummary>, EngineError>;
    fn exec(&self, id: &str, request: ExecRequest) -> Result<ExecSession, EngineError>;
    fn stop_owned(&self, id: &str, owner: &str, grace: Duration) -> Result<(), EngineError> {
        self.finish_owned(id, owner, grace, true)
    }
    fn finish_owned(
        &self,
        id: &str,
        owner: &str,
        grace: Duration,
        remove: bool,
    ) -> Result<(), EngineError>;
    fn remove_stopped(&self, id: &str) -> Result<(), EngineError>;
    fn stats(&self, id: &str) -> Result<AgentStats, EngineError>;
    fn image_config(&self, tag: &str) -> Result<Option<SandboxImageConfig>, EngineError>;
    fn import_archive(&self, path: PathBuf, tag: String) -> Result<(), EngineError>;
    fn remove_image(&self, tag: &str) -> Result<(), EngineError>;
}

#[cfg(all(test, unix))]
mod spec_tests {
    use super::*;

    fn spec(mounts: Vec<MountSpec>) -> SandboxSpec {
        SandboxSpec {
            name: "awman-spec".into(),
            image: "awman-x:latest".into(),
            labels: BTreeMap::new(),
            mounts,
            mount_owner: None,
            vcpus: 1,
            memory_mib: 256,
            root_disk_mib: 512,
            mount_quota_mib: 64,
            network: NetworkPlan::default(),
        }
    }

    #[test]
    fn docker_socket_is_not_a_file_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let error = spec(vec![MountSpec {
            host: socket,
            guest: "/var/run/docker.sock".into(),
            read_only: false,
        }])
        .check()
        .unwrap_err();
        assert!(
            matches!(&error, EngineError::Config(m) if m.contains("not bridged")),
            "{error:?}"
        );
        // A symlink to the socket is resolved and refused the same way.
        let link = dir.path().join("link.sock");
        std::os::unix::fs::symlink(dir.path().join("docker.sock"), &link).unwrap();
        assert!(spec(vec![MountSpec {
            host: link,
            guest: "/var/run/docker.sock".into(),
            read_only: true,
        }])
        .check()
        .is_err());
        // Files and directories are accepted.
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        spec(vec![
            MountSpec {
                host: dir.path().into(),
                guest: "/workspace".into(),
                read_only: false,
            },
            MountSpec {
                host: file,
                guest: "/workspace/f".into(),
                read_only: true,
            },
        ])
        .check()
        .unwrap();
    }

    #[test]
    fn spec_resources_below_the_floor_are_refused() {
        let mut zero_cpu = spec(Vec::new());
        zero_cpu.vcpus = 0;
        assert!(zero_cpu.check().is_err());
        let mut small = spec(Vec::new());
        small.memory_mib = 127;
        assert!(small.check().is_err());
        let mut tiny_disk = spec(Vec::new());
        tiny_disk.root_disk_mib = 255;
        assert!(tiny_disk.check().is_err());
        let mut no_quota = spec(Vec::new());
        no_quota.mount_quota_mib = 0;
        assert!(no_quota.check().is_err());
        assert!(spec(Vec::new()).check().is_ok());
    }
}

/// The home directory `/etc/passwd` gives the image's effective user, the
/// fallback Docker uses when the image config sets no `HOME`. `user` is the
/// image `USER` (`name`, `uid`, `name:group` or `uid:gid`); unset or empty
/// means root. Only an absolute path is returned.
pub(super) fn passwd_home(passwd: &str, user: Option<&str>) -> Option<String> {
    let user = user
        .filter(|u| !u.is_empty())
        .map(|u| u.split(':').next().unwrap_or(u))
        .unwrap_or("0");
    let by_uid = user.parse::<u32>().ok();
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            return None;
        }
        let matches = match by_uid {
            Some(uid) => fields[2].parse::<u32>().ok() == Some(uid),
            None => fields[0] == user,
        };
        (matches && fields[5].starts_with('/')).then(|| fields[5].to_string())
    })
}

#[cfg(test)]
mod passwd_home_tests {
    use super::passwd_home;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nprobe:x:1234:1234::/home/probe:/bin/sh\nnohome:x:5:5:::/bin/sh\n";

    #[test]
    fn unset_user_is_root() {
        assert_eq!(passwd_home(PASSWD, None).as_deref(), Some("/root"));
        assert_eq!(passwd_home(PASSWD, Some("")).as_deref(), Some("/root"));
    }

    #[test]
    fn named_and_numeric_users_with_or_without_group() {
        assert_eq!(
            passwd_home(PASSWD, Some("probe")).as_deref(),
            Some("/home/probe")
        );
        assert_eq!(
            passwd_home(PASSWD, Some("1234")).as_deref(),
            Some("/home/probe")
        );
        assert_eq!(
            passwd_home(PASSWD, Some("probe:staff")).as_deref(),
            Some("/home/probe")
        );
        assert_eq!(
            passwd_home(PASSWD, Some("1234:1234")).as_deref(),
            Some("/home/probe")
        );
    }

    #[test]
    fn unknown_users_and_relative_or_empty_homes_give_nothing() {
        assert_eq!(passwd_home(PASSWD, Some("ghost")), None);
        assert_eq!(passwd_home(PASSWD, Some("99")), None);
        assert_eq!(passwd_home(PASSWD, Some("nohome")), None);
        assert_eq!(passwd_home("garbage", None), None);
    }
}
