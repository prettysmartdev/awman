//! `engine::agent_runtime` — the Layer 1 `AgentRuntimeEngine` trait family.
//!
//! Abstracts over the two paradigms of agent-isolation runtime awman
//! supports: **container-class** (`ContainerRuntime` — Docker, Apple
//! Containers) and **sandbox-class** (`SandboxRuntime` — microVM-per-session
//! runtimes such as Docker Sandboxes).
//!
//! Layer 2 sees only `Arc<dyn AgentRuntimeEngine>` and this module's types.
//! Paradigm-specific operations (image builds, background containers) stay
//! as inherent methods on the concrete runtimes and are reached through the
//! typed handles `agent_runtime::detect()` hands back.

use std::sync::Arc;

use crate::data::config::global::GlobalConfig;
use crate::data::session::Session;
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::sandbox::options::ResolvedSandboxOptions;
use crate::engine::sandbox::SandboxRuntime;

pub mod background;
pub mod capabilities;
pub mod execution;
pub mod frontend;

pub use background::{AgentExec, ExecOutput};
pub use capabilities::{Capabilities, DindSupport};
pub use execution::{
    AgentExecution, AgentExitInfo, AgentHandle, AgentHandlePreview, AgentInstance, AgentStats,
    CancelHandle, StuckEvent,
};
pub use frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};

/// Common option carrier between Layer 2 and the runtime tier. Layer 2
/// constructs whichever variant matches the runtime paradigm it's targeting
/// (branching on `AgentRuntimeEngine::capabilities()`).
#[derive(Debug, Clone)]
pub enum ResolvedAgentOptions {
    Container(ResolvedContainerOptions),
    Sandbox(ResolvedSandboxOptions),
}

impl ResolvedAgentOptions {
    /// Name of the carried paradigm — used in `OptionVariantMismatch` errors.
    pub fn paradigm(&self) -> &'static str {
        match self {
            ResolvedAgentOptions::Container(_) => "container",
            ResolvedAgentOptions::Sandbox(_) => "sandbox",
        }
    }

    /// Resolve a container option list into the `Container` variant.
    /// Conflicting options surface as `EngineError::ConflictingOptions`,
    /// exactly as the pre-refactor `ContainerRuntime::build` reported them.
    pub fn container(
        options: impl IntoIterator<Item = crate::engine::container::options::ContainerOption>,
    ) -> Result<Self, EngineError> {
        let resolved = ResolvedContainerOptions::resolve(options)?;
        Ok(ResolvedAgentOptions::Container(resolved))
    }
}

/// What every agent runtime must support. The cross-paradigm trait surface
/// Layer 2 programs against; paradigm-specific decisions branch on
/// `capabilities()` or `runtime_name()`, never on concrete types.
pub trait AgentRuntimeEngine: Send + Sync {
    /// Stable machine name for this runtime (e.g. "docker", "apple-containers",
    /// "docker-sbx-experimental"). Used for log lines and config round-trips.
    fn runtime_name(&self) -> &'static str;

    /// User-facing display name (e.g. "Docker", "Apple Containers",
    /// "Docker Sandboxes (experimental)").
    fn display_name(&self) -> &'static str;

    /// Static description of what this runtime can do. Layer 2 reads this
    /// to decide how to map cross-paradigm options before calling build().
    fn capabilities(&self) -> &Capabilities;

    /// Probe whether the underlying tooling is reachable. Times out on its own.
    fn is_available(&self) -> bool;

    /// Construct a configured `AgentInstance` from typed options — the first
    /// half of the two-step build/run pattern (no spawn happens here). The
    /// runtime rejects options whose paradigm doesn't fit with
    /// `EngineError::OptionVariantMismatch`.
    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError>;

    /// Enumerate handles for running agents created by this runtime.
    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError>;

    /// Same as list_running but session-less, for stats polling loops.
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError>;

    /// Per-handle resource stats. Returns zeros when the runtime can't
    /// provide per-resource metrics (sandbox-class runtimes today).
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;

    /// Stop a running agent. Semantics vary per runtime:
    ///   - container: stop + rm
    ///   - sandbox:   stop (preserve persistent volume)
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Build argv for an exec/re-attach against an existing agent.
    fn exec_args(
        &self,
        agent_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String>;

    /// Name of the CLI binary this runtime drives ("docker", "container", "sbx").
    fn cli_binary(&self) -> &'static str;
}

/// The concrete runtime `detect()` chose, exposing both the cross-paradigm
/// trait handle and the typed paradigm-specific handle. `Engines` populates
/// its `container_runtime` / `sandbox_runtime` fields from this — both
/// handles point at the same underlying object.
pub enum DetectedRuntime {
    Container(Arc<ContainerRuntime>),
    Sandbox(Arc<SandboxRuntime>),
}

impl DetectedRuntime {
    /// Cross-paradigm trait-object handle to the detected runtime.
    pub fn engine(&self) -> Arc<dyn AgentRuntimeEngine> {
        match self {
            DetectedRuntime::Container(rt) => rt.clone(),
            DetectedRuntime::Sandbox(rt) => rt.clone(),
        }
    }

    /// Typed handle, set when the detected runtime is container-class.
    pub fn container_runtime(&self) -> Option<Arc<ContainerRuntime>> {
        match self {
            DetectedRuntime::Container(rt) => Some(rt.clone()),
            DetectedRuntime::Sandbox(_) => None,
        }
    }

    /// Typed handle, set when the detected runtime is sandbox-class.
    pub fn sandbox_runtime(&self) -> Option<Arc<SandboxRuntime>> {
        match self {
            DetectedRuntime::Container(_) => None,
            DetectedRuntime::Sandbox(rt) => Some(rt.clone()),
        }
    }
}

/// Factory: pick the right runtime based on `GlobalConfig::runtime`.
///
/// - `None` / `"docker"` → `ContainerRuntime` with the Docker backend
/// - `"apple-containers"` → `ContainerRuntime` with the Apple backend
///   (macOS only)
/// - `"docker-sbx-experimental"` → `SandboxRuntime` with the (stubbed)
///   Docker Sandbox backend (macOS arm64 / Windows only; see WI 0090)
/// - anything else → warn and fall back to Docker
pub fn detect(global_config: &GlobalConfig) -> Result<DetectedRuntime, EngineError> {
    let runtime_name = global_config
        .runtime
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    match runtime_name {
        Some("docker") | None => Ok(DetectedRuntime::Container(Arc::new(
            ContainerRuntime::docker(),
        ))),
        Some("apple-containers") => {
            if cfg!(target_os = "macos") {
                Ok(DetectedRuntime::Container(Arc::new(
                    ContainerRuntime::apple(),
                )))
            } else {
                Err(EngineError::BackendUnsupportedOnPlatform {
                    backend: "apple-containers".into(),
                    platform: std::env::consts::OS.into(),
                })
            }
        }
        Some("docker-sbx-experimental") => {
            Ok(DetectedRuntime::Sandbox(Arc::new(SandboxRuntime::dsbx()?)))
        }
        Some(other) => {
            tracing::warn!(runtime = other, "unknown runtime, falling back to Docker");
            Ok(DetectedRuntime::Container(Arc::new(
                ContainerRuntime::docker(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{ContainerOption, ImageRef};

    fn docker_cfg() -> GlobalConfig {
        GlobalConfig {
            runtime: Some("docker".into()),
            ..Default::default()
        }
    }

    // ─── Detection ────────────────────────────────────────────────────────────

    #[test]
    fn detect_none_runtime_picks_docker() {
        let cfg = GlobalConfig::default();
        let rt = detect(&cfg).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
        assert!(rt.container_runtime().is_some());
        assert!(rt.sandbox_runtime().is_none());
    }

    #[test]
    fn detect_explicit_docker_string_picks_docker() {
        let rt = detect(&docker_cfg()).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
        assert!(rt.container_runtime().is_some());
    }

    #[test]
    fn detect_empty_runtime_string_falls_back_to_docker() {
        let cfg = GlobalConfig {
            runtime: Some("  ".into()),
            ..Default::default()
        };
        let rt = detect(&cfg).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
    }

    #[test]
    fn detect_apple_on_non_mac_errors() {
        let cfg = GlobalConfig {
            runtime: Some("apple-containers".into()),
            ..Default::default()
        };
        let res = detect(&cfg);
        if cfg!(target_os = "macos") {
            assert!(res.is_ok());
            assert_eq!(res.unwrap().engine().runtime_name(), "apple-containers");
        } else {
            match res {
                Err(EngineError::BackendUnsupportedOnPlatform { .. }) => {}
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
                Ok(_) => panic!("expected error on non-macOS"),
            }
        }
    }

    #[test]
    fn detect_dsbx_routes_to_sandbox_or_errors_on_unsupported_platform() {
        let cfg = GlobalConfig {
            runtime: Some("docker-sbx-experimental".into()),
            ..Default::default()
        };
        let res = detect(&cfg);
        // dsbx is only supported on macOS arm64 and Windows.
        if cfg!(target_os = "linux")
            || cfg!(all(target_os = "macos", target_arch = "x86_64"))
        {
            match res {
                Err(EngineError::BackendUnsupportedOnPlatform { .. }) => {}
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
                Ok(_) => panic!("expected platform error for dsbx on this OS/arch"),
            }
        } else {
            let rt = res.expect("dsbx should succeed on this platform");
            assert_eq!(rt.engine().runtime_name(), "docker-sbx-experimental");
            assert!(rt.sandbox_runtime().is_some());
            assert!(rt.container_runtime().is_none());
        }
    }

    #[test]
    fn detect_unknown_runtime_falls_back_to_docker() {
        let cfg = GlobalConfig {
            runtime: Some("blarg".into()),
            ..Default::default()
        };
        // Unknown runtime should fall back to Docker with a warning, not error.
        let rt = detect(&cfg).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
    }

    // ─── Option-variant mismatch via ContainerRuntime ─────────────────────────

    #[test]
    fn container_engine_rejects_sandbox_options() {
        use crate::engine::sandbox::options::ResolvedSandboxOptions;
        let rt = detect(&docker_cfg()).unwrap();
        let engine = rt.engine();
        let opts = ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::default());
        match engine.build(opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker");
                assert_eq!(got, "sandbox");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ─── Docker integration (env-gated) ───────────────────────────────────────

    /// End-to-end test: runs a real Docker container through
    /// `Box<dyn AgentRuntimeEngine>` and asserts the exit code matches the
    /// pre-refactor path.
    ///
    /// Gate: set `AWMAN_DOCKER_INTEGRATION=1` and ensure `docker` is on PATH
    /// and the daemon is reachable. Skipped silently otherwise.
    #[tokio::test]
    async fn docker_integration_runs_container_through_trait() {
        // Skip if the gate env var is absent.
        if std::env::var("AWMAN_DOCKER_INTEGRATION").as_deref() != Ok("1") {
            return;
        }

        use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
        use crate::engine::container::ContainerRuntime;
        use crate::engine::message::{UserMessage, UserMessageSink};

        // Minimal no-op frontend for test purposes.
        struct NullFrontend;
        impl UserMessageSink for NullFrontend {
            fn write_message(&mut self, _msg: UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        #[async_trait::async_trait]
        impl AgentFrontend for NullFrontend {
            fn report_status(&mut self, _: AgentStatus) {}
            fn report_progress(&mut self, _: AgentProgress) {}
            fn take_io(&mut self) -> AgentIo {
                let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
                let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
                let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
                AgentIo {
                    stdout,
                    stderr,
                    stdin_tx,
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }
            }
        }

        // Build through the trait surface (Box<dyn AgentRuntimeEngine>).
        let engine: Arc<dyn AgentRuntimeEngine> =
            Arc::new(ContainerRuntime::docker());
        let opts = ResolvedAgentOptions::container([
            ContainerOption::Image(ImageRef::new("busybox:latest")),
            ContainerOption::Entrypoint(
                crate::engine::container::options::Entrypoint::new(["true"]),
            ),
        ])
        .expect("resolve container options");

        let instance = engine.build(opts).expect("build agent instance");
        let mut execution = instance
            .run_with_frontend(Box::new(NullFrontend))
            .expect("run_with_frontend");

        let exit_info = execution.wait().await.expect("wait for container");
        assert_eq!(exit_info.exit_code, 0, "expected clean exit from `true`");
    }
}
