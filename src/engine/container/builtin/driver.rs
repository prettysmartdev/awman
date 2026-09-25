//! Transport boundary: runtime policy and frontend I/O never depend on SDK types.
use crate::data::session::AgentHandle;
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
pub struct ImageConfigSummary {
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
    fn image_config(&self, tag: &str) -> Result<Option<ImageConfigSummary>, EngineError>;
    fn import_archive(&self, path: PathBuf, tag: String) -> Result<(), EngineError>;
    fn remove_image(&self, tag: &str) -> Result<(), EngineError>;
}
