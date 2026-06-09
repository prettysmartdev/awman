//! Internal `SandboxBackend` trait — NOT pub outside `src/engine/sandbox/`.
//!
//! The sandbox-paradigm analogue of the container tier's `ContainerBackend`.
//! Method shapes are sandbox-appropriate: no `image_home_dir`, no
//! `build_image` (sandboxes pull kit templates from registries instead of
//! building local images). This is the minimum surface WI 0090's
//! `DSbxBackend` implementation must satisfy.
//!
//! Implementations: `dsbx::DSbxBackend` (stubbed in WI 0089).

use std::collections::HashMap;

use crate::engine::agent_runtime::{AgentHandle, AgentStats, ExecOutput};
use crate::engine::error::EngineError;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Identity-only handle to a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxId(pub String);

impl SandboxId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What every sandbox backend must support. The concrete type is hidden
/// behind `Arc<dyn SandboxBackend>` and never escapes this module.
pub(super) trait SandboxBackend: Send + Sync {
    /// Create and start a sandbox from resolved options. Returns the
    /// sandbox's identity. The kit/template is pulled by the underlying
    /// tooling as needed.
    fn start_sandbox(&self, opts: &ResolvedSandboxOptions) -> Result<SandboxId, EngineError>;

    /// Restart a previously-stopped sandbox, preserving its persistent
    /// volume.
    fn restart_sandbox(&self, id: &SandboxId) -> Result<(), EngineError>;

    /// Execute a command inside a running sandbox.
    fn exec_in_sandbox(
        &self,
        id: &SandboxId,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError>;

    /// Stop a running sandbox (preserve the persistent volume).
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Remove a sandbox and its persistent volume.
    fn remove(&self, id: &SandboxId) -> Result<(), EngineError>;

    /// Enumerate handles for running awman sandboxes.
    fn list_running(&self) -> Result<Vec<AgentHandle>, EngineError>;

    /// Per-handle resource stats. Sandbox-class runtimes can't provide
    /// per-resource metrics today; implementations return zeros.
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;

    /// Static name used by `SandboxRuntime::runtime_name`.
    fn name(&self) -> &'static str;

    /// CLI binary for this backend (`sbx`).
    fn cli_binary(&self) -> &'static str;
}
