//! `DSbxBackend` — the Docker Sandboxes driver. STUBBED in WI 0089.
//!
//! The detection wiring is real: `runtime: "docker-sbx-experimental"`
//! routes to `SandboxRuntime` + this backend. Every method returns
//! `EngineError::NotImplemented` naming WI 0090, which replaces each stub
//! with the real `sbx`-driven implementation.

use std::collections::HashMap;

use crate::engine::agent_runtime::{AgentHandle, AgentStats, ExecOutput};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::{SandboxBackend, SandboxId};
use crate::engine::sandbox::options::ResolvedSandboxOptions;

#[derive(Debug, Default)]
pub(in crate::engine::sandbox) struct DSbxBackend;

impl DSbxBackend {
    pub(in crate::engine::sandbox) fn new() -> Self {
        Self
    }
}

impl SandboxBackend for DSbxBackend {
    fn start_sandbox(&self, _opts: &ResolvedSandboxOptions) -> Result<SandboxId, EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::start_sandbox is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn restart_sandbox(&self, _id: &SandboxId) -> Result<(), EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::restart_sandbox is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn exec_in_sandbox(
        &self,
        _id: &SandboxId,
        _command: &str,
        _working_dir: &str,
        _env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::exec_in_sandbox is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn stop(&self, _handle: &AgentHandle) -> Result<(), EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::stop is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn remove(&self, _id: &SandboxId) -> Result<(), EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::remove is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn list_running(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::list_running is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn stats(&self, _handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        Err(EngineError::NotImplemented(
            "DSbxBackend::stats is stubbed; see work-item 0090 for the implementation",
        ))
    }

    fn name(&self) -> &'static str {
        "docker-sbx-experimental"
    }

    fn cli_binary(&self) -> &'static str {
        "sbx"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::AgentHandle;
    use crate::engine::error::EngineError;
    use crate::engine::sandbox::backend::SandboxId;
    use crate::engine::sandbox::options::ResolvedSandboxOptions;

    fn backend() -> DSbxBackend {
        DSbxBackend::new()
    }

    fn dummy_handle() -> AgentHandle {
        AgentHandle {
            id: "test-id".into(),
            image_tag: "test-kit".into(),
            name: "test-sandbox".into(),
            started_at: chrono::Utc::now(),
        }
    }

    fn is_not_implemented_naming_wi_0090(err: &EngineError) -> bool {
        match err {
            EngineError::NotImplemented(msg) => msg.contains("0090"),
            _ => false,
        }
    }

    // ─── Stub: every method returns NotImplemented naming WI 0090 ─────────────

    #[test]
    fn start_sandbox_is_stubbed() {
        let opts = ResolvedSandboxOptions::default();
        let err = backend().start_sandbox(&opts).unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "start_sandbox must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    #[test]
    fn restart_sandbox_is_stubbed() {
        let id = SandboxId::new("test");
        let err = backend().restart_sandbox(&id).unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "restart_sandbox must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    #[test]
    fn exec_in_sandbox_is_stubbed() {
        let id = SandboxId::new("test");
        let err = backend()
            .exec_in_sandbox(&id, "echo hi", "/", None)
            .unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "exec_in_sandbox must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    #[test]
    fn stop_is_stubbed() {
        let handle = dummy_handle();
        let err = backend().stop(&handle).unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "stop must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    #[test]
    fn remove_is_stubbed() {
        let id = SandboxId::new("test");
        let err = backend().remove(&id).unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "remove must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    #[test]
    fn list_running_is_stubbed() {
        let err = backend().list_running().unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "list_running must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    #[test]
    fn stats_is_stubbed() {
        let handle = dummy_handle();
        let err = backend().stats(&handle).unwrap_err();
        assert!(
            is_not_implemented_naming_wi_0090(&err),
            "stats must return NotImplemented naming WI 0090, got: {err:?}"
        );
    }

    // ─── Identity ─────────────────────────────────────────────────────────────

    #[test]
    fn name_is_correct() {
        assert_eq!(backend().name(), "docker-sbx-experimental");
    }

    #[test]
    fn cli_binary_is_sbx() {
        assert_eq!(backend().cli_binary(), "sbx");
    }
}
