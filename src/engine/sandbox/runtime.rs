//! `SandboxRuntime` — the sandbox-class `AgentRuntimeEngine` impl.
//!
//! Holds an `Arc<dyn SandboxBackend>`. The concrete driver is invisible
//! outside this module. Platform guards live in the constructors: a user on
//! an unsupported platform gets `BackendUnsupportedOnPlatform` from the
//! constructor and never reaches the backend.

use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::agent_runtime::{
    AgentExecution, AgentFrontend, AgentHandle, AgentHandlePreview, AgentInstance,
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::SandboxBackend;
use crate::engine::sandbox::dsbx::DSbxBackend;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Capabilities shared by sandbox-class runtimes: kit-declarative,
/// persistent, workspace-only mounts, private DinD per VM.
static SANDBOX_CAPABILITIES: Capabilities = Capabilities {
    arbitrary_env_vars: false,
    arbitrary_host_mounts: false,
    cpu_limits: false,
    per_resource_stats: false,
    persistent_lifecycle: true,
    kit_declarative: true,
    dind: DindSupport::Always,
    host_paths_visible: false,
    session_label_supported: false,
};

pub struct SandboxRuntime {
    backend: Arc<dyn SandboxBackend>,
}

impl SandboxRuntime {
    /// Construct with the Docker Sandbox (`sbx`) backend.
    ///
    /// Platform guards: Docker Sandboxes are not available on Linux, and not
    /// on Intel Macs. Erroring here (rather than from the first backend
    /// call) gives the user an actionable platform message up front.
    pub fn dsbx() -> Result<Self, EngineError> {
        if cfg!(target_os = "linux") {
            return Err(EngineError::BackendUnsupportedOnPlatform {
                backend: "docker-sbx-experimental".into(),
                platform: "linux".into(),
            });
        }
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            return Err(EngineError::BackendUnsupportedOnPlatform {
                backend: "docker-sbx-experimental".into(),
                platform: "macos (x86_64)".into(),
            });
        }
        Ok(Self {
            backend: Arc::new(DSbxBackend::new()),
        })
    }
}

impl AgentRuntimeEngine for SandboxRuntime {
    fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    fn display_name(&self) -> &'static str {
        match self.backend.name() {
            "docker-sbx-experimental" => "Docker Sandboxes (experimental)",
            _ => "Sandbox",
        }
    }

    fn capabilities(&self) -> &Capabilities {
        &SANDBOX_CAPABILITIES
    }

    fn is_available(&self) -> bool {
        use std::process::Stdio;
        let child = std::process::Command::new(self.backend.cli_binary())
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => crate::engine::container::runtime::wait_with_timeout(
                child,
                std::time::Duration::from_secs(10),
            )
            .map(|s| s.success())
            .unwrap_or(false),
            Err(_) => false,
        }
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        match options {
            ResolvedAgentOptions::Sandbox(opts) => Ok(Box::new(SandboxAgentInstance {
                backend: Arc::clone(&self.backend),
                options: opts,
            })),
            other => Err(EngineError::OptionVariantMismatch {
                runtime: self.runtime_name().to_string(),
                got: other.paradigm(),
            }),
        }
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        // Sandboxes have no session label; attribution is by name (WI 0090).
        self.backend.list_running()
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running()
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.backend.stats(handle)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }

    fn exec_args(
        &self,
        _agent_id: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        // Exec/re-attach argv shape is defined by WI 0090. Unreachable while
        // the backend is stubbed: no sandbox can be running.
        Vec::new()
    }

    fn cli_binary(&self) -> &'static str {
        self.backend.cli_binary()
    }
}

/// Configured-but-not-running sandbox agent — the sandbox tier's half of the
/// two-step build/run pattern.
struct SandboxAgentInstance {
    backend: Arc<dyn SandboxBackend>,
    options: ResolvedSandboxOptions,
}

impl AgentInstance for SandboxAgentInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        let name = self
            .options
            .sandbox_name
            .clone()
            .unwrap_or_else(|| self.options.agent_id.clone());
        AgentHandlePreview {
            id: name.clone(),
            name,
            // Sandboxes boot a kit/template rather than a local image; the
            // kit selector is the closest analogue.
            image: self.options.agent_id.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        _frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        // Stubbed: `start_sandbox` returns NotImplemented until WI 0090.
        let _id = self.backend.start_sandbox(&self.options)?;
        Err(EngineError::NotImplemented(
            "SandboxAgentInstance::run_with_frontend is stubbed; see work-item 0090 for the implementation",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
    use crate::engine::container::options::ResolvedContainerOptions;
    use crate::engine::error::EngineError;

    // ─── Platform guards ──────────────────────────────────────────────────────

    #[test]
    fn dsbx_errors_on_linux() {
        if cfg!(target_os = "linux") {
            match SandboxRuntime::dsbx() {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                    assert_eq!(backend, "docker-sbx-experimental");
                    assert_eq!(platform, "linux");
                }
                Err(e) => panic!("expected BackendUnsupportedOnPlatform on linux, got: {e:?}"),
                Ok(_) => panic!("dsbx() must fail on linux"),
            }
        }
    }

    #[test]
    fn dsbx_errors_on_x86_64_macos() {
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            match SandboxRuntime::dsbx() {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                    assert_eq!(backend, "docker-sbx-experimental");
                    assert!(
                        platform.contains("macos"),
                        "platform should mention macos, got: {platform}"
                    );
                    assert!(
                        platform.contains("x86_64"),
                        "platform should mention x86_64, got: {platform}"
                    );
                }
                Err(e) => {
                    panic!("expected BackendUnsupportedOnPlatform on x86_64 macos, got: {e:?}")
                }
                Ok(_) => panic!("dsbx() must fail on x86_64 macos"),
            }
        }
    }

    // ─── Option-variant mismatch via SandboxRuntime ───────────────────────────

    /// `SandboxRuntime::build` must reject container-paradigm options with a
    /// clear `OptionVariantMismatch` error on platforms where dsbx is
    /// supported. Skipped via early-return on unsupported platforms.
    #[test]
    fn sandbox_runtime_via_trait_rejects_container_options() {
        let rt = match SandboxRuntime::dsbx() {
            Ok(rt) => rt,
            Err(_) => return, // unsupported platform — platform guard test covers this
        };
        let opts = ResolvedAgentOptions::Container(ResolvedContainerOptions::resolve([]).unwrap());
        match <SandboxRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker-sbx-experimental");
                assert_eq!(got, "container");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ─── runtime_name and display_name ────────────────────────────────────────

    #[test]
    fn dsbx_runtime_name_and_display_name() {
        // dsbx() errors on unsupported platforms; the guard tests above
        // cover that path.
        if let Ok(rt) = SandboxRuntime::dsbx() {
            assert_eq!(rt.runtime_name(), "docker-sbx-experimental");
            assert!(
                rt.display_name().contains("experimental"),
                "display_name should mention experimental, got: {}",
                rt.display_name()
            );
        }
    }
}
