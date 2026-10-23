//! `ContainerRuntime` — the container-class `AgentRuntimeEngine` impl.
//!
//! Holds an `Arc<dyn ContainerBackend>` chosen by the `docker()` / `apple()` /
//! `builtin()` constructors (selection between runtimes happens in
//! `agent_runtime::detect`). The concrete backend is invisible outside this
//! module, and every operation below delegates to one explicit backend
//! operation — nothing here shells out itself.
//!
//! Container-paradigm-specific operations — `build_image`, `image_exists`,
//! `image_home_dir`, `start_background` — are inherent methods; the
//! cross-paradigm subset is also reachable through `AgentRuntimeEngine`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::config::resolved_builtin::BuiltinProcessConfig;
use crate::data::oci_identity::ImageIdentity;
use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::{
    AgentDetail, AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
    ImageImportRequest, ImportedImage, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::container::apple::AppleBackend;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::background::{BackgroundContainer, BackgroundLaunchRequest};
use crate::engine::container::docker::DockerBackend;
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::container::phase_step::PhaseStepContainerSpec;
use crate::engine::error::EngineError;

/// A container image row returned by image-listing queries. Used by
/// `awman clean` to enumerate dangling awman images eligible for removal.
#[derive(Debug, Clone)]
pub struct ContainerImageInfo {
    /// Image ID (short or full).
    pub id: String,
    /// `repository:tag` label, or `<none>:<none>` for untagged images.
    pub repo_tag: String,
    /// Human-readable size string reported by the runtime (e.g. "1.2GB").
    pub size: String,
}

/// Capabilities shared by the CLI-driven container backends (Docker, Apple
/// Containers): image-based, built locally, ephemeral, arbitrary mounts/env,
/// label-based session attribution, fractional CPU limits.
pub(super) static CONTAINER_CAPABILITIES: Capabilities = Capabilities {
    arbitrary_host_mounts: true,
    kit_declarative: false,
    has_image_store: true,
    image_acquisition: ImageAcquisition::Build,
    per_launch_vm_settings: false,
    reports_resource_stats: true,
};

/// The process-wide part of the builtin microVM backend's settings: one
/// builtin runtime per process owns these. Resolved from the effective config
/// the process starts with (`agent_runtime::detect_effective`) before
/// `ContainerRuntime::builtin` is called.
///
/// Everything a launch is sized and confined by (default vCPUs and memory,
/// network policy) is per session instead: it arrives with each launch as
/// [`BuiltinLaunchSettings`](crate::data::config::builtin_launch::BuiltinLaunchSettings),
/// resolved from that session's own config.
/// Image sources, registries and the builder flag are per-session image
/// acquisition inputs and never live here either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinRuntimeSettings {
    /// Private state root (catalog, caches, control sockets).
    pub state_dir: PathBuf,
    /// The ambient `MSB_*` overrides present in the environment. Any entry
    /// makes the backend refuse to start (`AmbientRuntimeOverride`).
    pub ambient_overrides: Vec<&'static str>,
    /// Running under test isolation without `AWMAN_TEST_BUILTIN`: the real
    /// runtime must not be touched.
    pub test_isolation: bool,
}

impl BuiltinRuntimeSettings {
    /// Select the process-owned fields Layer 0 resolved. All merging and
    /// defaults have already happened there.
    pub fn resolve(resolved: &BuiltinProcessConfig) -> Self {
        Self {
            state_dir: resolved.state_dir.clone(),
            ambient_overrides: resolved.ambient_overrides.clone(),
            test_isolation: resolved.test_isolation,
        }
    }
}

/// [`ContainerRuntime::runtime_name`] of the builtin microVM backend.
pub const BUILTIN_RUNTIME_NAME: &str = "builtin";

pub struct ContainerRuntime {
    backend: Arc<dyn ContainerBackend>,
}

impl ContainerRuntime {
    /// Construct with the Docker backend.
    pub fn docker() -> Self {
        Self {
            backend: Arc::new(DockerBackend),
        }
    }

    /// Construct with the Apple Containers backend. The macOS platform guard
    /// lives in `agent_runtime::detect`; constructing this directly on a
    /// non-mac host yields a runtime whose probes simply fail.
    pub fn apple() -> Self {
        Self {
            backend: Arc::new(AppleBackend),
        }
    }

    /// Construct with the builtin microVM backend. Transient startup
    /// unavailability retains the selected runtime and retries on observation.
    /// Unsupported targets, ambient overrides and invalid configuration fail.
    pub fn builtin(settings: BuiltinRuntimeSettings) -> Result<Self, EngineError> {
        Self::open_builtin(Box::new(move || {
            crate::engine::container::builtin::open(settings.clone())
        }))
    }

    pub(super) fn open_builtin(
        open: Box<super::deferred::OpenBackend>,
    ) -> Result<Self, EngineError> {
        let backend = match open() {
            Ok(backend) => backend,
            Err(EngineError::BuiltinRuntimeUnavailable { .. }) => {
                Arc::new(super::deferred::DeferredBackend::new(open))
            }
            Err(e) => return Err(e),
        };
        Ok(Self { backend })
    }

    /// Wrap an already-constructed backend. Test-only: lets hermetic tests run
    /// a real backend over a fake driver.
    #[cfg(test)]
    pub(super) fn with_backend(backend: Arc<dyn ContainerBackend>) -> Self {
        Self { backend }
    }

    /// Static name of the chosen backend (e.g. `"docker"`).
    pub fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    /// User-facing display name for the chosen backend
    /// (e.g. `"Docker"`, `"Apple Containers"`).
    pub fn display_name(&self) -> &'static str {
        self.backend.display_name()
    }

    /// Static description of what the chosen backend can do.
    pub fn capabilities(&self) -> &Capabilities {
        self.backend.capabilities()
    }

    /// One-line notices from opening the backend (an in-place builtin
    /// catalog upgrade), each returned once.
    pub fn take_notices(&self) -> Vec<String> {
        self.backend.take_notices()
    }

    /// Build a fully-configured `AgentInstance` from pre-resolved options.
    pub fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.backend.build(options)
    }

    pub fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running(session)
    }

    /// Build a container image, streaming output line-by-line through
    /// `on_line`. Returns an error when the build fails or the backend
    /// cannot build images.
    pub fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        self.backend.build_image(request, on_line)
    }

    /// Read the image's baked-in `$HOME` from its config. Used by
    /// `AgentEngine::build_options` to mount agent settings overlays at the
    /// path the running container's user actually reads — when the
    /// `Dockerfile.<agent>` has been changed but the image hasn't been
    /// rebuilt, the image's User/HOME is the authority, not the Dockerfile.
    /// Returns `None` when the image is missing or the runtime is
    /// unreachable.
    pub fn image_home_dir(&self, tag: &str) -> Option<String> {
        self.backend.image_home_dir(tag).ok().flatten()
    }

    /// Best-effort check whether an image tag exists locally on the runtime.
    /// Probes time out after 10 seconds to avoid hanging when the daemon is
    /// unresponsive.
    pub fn image_exists(&self, tag: &str) -> bool {
        self.backend.image_exists(tag).unwrap_or(false)
    }

    /// List all running awman containers without requiring a session.
    /// Used by the TUI event loop for stats polling.
    pub fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running_all()
    }

    pub fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.backend.stats(handle)
    }

    pub fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }

    /// List stopped (exited/dead) awman containers eligible for cleanup.
    /// Running and paused containers are never returned. Used by `awman clean`.
    pub fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_stopped()
    }

    /// List dangling awman images (superseded by a newer build of the same
    /// tag). Used by `awman clean`.
    pub fn list_dangling_images(&self) -> Result<Vec<ContainerImageInfo>, EngineError> {
        self.backend.list_dangling_images()
    }

    /// Remove a container by id/name. Returns an error when the runtime refuses
    /// (e.g. the container transitioned back to running between discovery and
    /// deletion). Used by `awman clean` for per-item failure handling.
    pub fn remove_container(&self, id: &str) -> Result<(), EngineError> {
        self.backend.remove_agent(id)
    }

    /// Remove an image by id. Returns an error when the runtime refuses (e.g.
    /// the image is still referenced by a container). Used by `awman clean`.
    pub fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        self.backend.remove_image(id)
    }

    /// Attach to an already-running container this process did not start.
    /// Delegates to the backend; the returned instance runs through the
    /// existing `run_with_frontend` path.
    pub fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.backend.attach(handle)
    }

    /// List running awman containers whose name starts with `prefix`.
    pub fn list_running_with_name_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running_with_name_prefix(prefix)
    }

    /// Build a foreground container for one setup/teardown step: the step's
    /// command is its only process and a frontend's PTY attaches through
    /// `run_with_frontend`, exactly as for an agent. The interactive
    /// counterpart of [`ContainerRuntime::start_background`]; see
    /// [`PhaseStepContainerSpec`] for the container's shape.
    pub fn build_phase_step(
        &self,
        spec: &PhaseStepContainerSpec<'_>,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.build(ResolvedContainerOptions::resolve(spec.options())?)
    }

    /// Start a background container for setup/teardown execution.
    ///
    /// Delegates to the backend's `start_background`. The returned
    /// `BackgroundContainer` retains a shared reference to the backend so
    /// later `exec` and `kill` calls flow through the same trait. The builtin
    /// backend sizes and confines the container with the request's
    /// `builtin_launch`, exactly as it does a foreground launch.
    pub fn launch_background(
        &self,
        request: &BackgroundLaunchRequest<'_>,
    ) -> Result<BackgroundContainer, EngineError> {
        let container_id = self.backend.start_background(request)?;
        let workdir_str = request.workdir.display().to_string();
        Ok(BackgroundContainer::new(
            container_id,
            Arc::clone(&self.backend),
            workdir_str,
        ))
    }

    /// Start an idle container with the launching session's resolved settings.
    pub fn start_background(
        &self,
        request: &BackgroundLaunchRequest<'_>,
    ) -> Result<BackgroundContainer, EngineError> {
        self.launch_background(request)
    }

    /// Best-effort check whether the container runtime is reachable.
    /// Returns `false` when `docker info` (or equivalent) fails or times out.
    pub fn is_available(&self) -> bool {
        self.backend.is_available().is_ok()
    }

    /// Why the runtime is unreachable, or `Ok` when it is reachable. The
    /// detailed form of [`Self::is_available`].
    pub fn availability(&self) -> Result<(), EngineError> {
        self.backend.is_available()
    }
}

impl AgentRuntimeEngine for ContainerRuntime {
    fn runtime_name(&self) -> &'static str {
        ContainerRuntime::runtime_name(self)
    }

    fn display_name(&self) -> &'static str {
        ContainerRuntime::display_name(self)
    }

    fn capabilities(&self) -> &Capabilities {
        ContainerRuntime::capabilities(self)
    }

    fn is_available(&self) -> bool {
        ContainerRuntime::is_available(self)
    }

    fn availability(&self) -> Result<(), EngineError> {
        ContainerRuntime::availability(self)
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        match options {
            ResolvedAgentOptions::Container(opts) => ContainerRuntime::build(self, opts),
            other => Err(EngineError::OptionVariantMismatch {
                runtime: self.runtime_name().to_string(),
                got: other.paradigm(),
            }),
        }
    }

    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running(self, session)
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running_all(self)
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        ContainerRuntime::stats(self, handle)
    }

    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_stopped(self)
    }

    fn inspect(&self, handle: &AgentHandle) -> Result<AgentDetail, EngineError> {
        self.backend.inspect(handle)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        ContainerRuntime::stop(self, handle)
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        ContainerRuntime::attach(self, handle)
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running_with_name_prefix(self, prefix)
    }

    fn reattach_after_owner_exit(&self) -> bool {
        self.backend.reattach_after_owner_exit()
    }

    fn remove_agent(&self, id: &str) -> Result<(), EngineError> {
        self.backend.remove_agent(id)
    }

    fn ready_agent(
        &self,
        _agent: &str,
        _opts: ReadyAgentOptions,
        _sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        // Only defined for kit-declarative runtimes, and only ever called
        // behind `capabilities().kit_declarative` (see `ReadyEngine`). The
        // container tier prepares an agent by building (or importing) its
        // image, which `ReadyEngine` drives through `build_image` because the
        // step in front of it — downloading the per-agent Dockerfile —
        // belongs to `engine::agent::download`, not to a runtime.
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.runtime_name(),
            operation: "kit-declarative agent preparation",
        })
    }

    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        Ok(ContainerRuntime::image_exists(self, tag))
    }

    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError> {
        Ok(ContainerRuntime::image_home_dir(self, tag))
    }

    fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        ContainerRuntime::build_image(self, request, on_line)
    }

    fn import_image(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.backend.import_image(request, sink)
    }

    fn import_image_cancellable(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<ImportedImage, EngineError> {
        self.backend.import_image_cancellable(request, sink, cancel)
    }

    fn image_identity(&self, tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        self.backend.image_identity(tag)
    }

    fn image_matches_source(&self, request: &ImageImportRequest) -> Result<bool, EngineError> {
        self.backend.image_matches_source(request)
    }
}

/// Wait for a child process with a timeout. Kills the process and returns
/// `None` if the deadline elapses. Prevents unit tests and readiness checks
/// from hanging indefinitely when the Docker daemon is unresponsive.
pub(crate) fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::effective::EffectiveConfig;
    use crate::engine::agent_runtime::ResolvedAgentOptions;
    use crate::engine::sandbox::options::ResolvedSandboxOptions;

    #[test]
    fn build_requires_image_option() {
        let rt = ContainerRuntime::docker();
        let resolved = ResolvedContainerOptions::resolve([]).unwrap();
        match rt.build(resolved) {
            Err(EngineError::MissingRequiredOption(opt)) => {
                assert_eq!(opt, "Image");
            }
            Err(e) => panic!("expected MissingRequiredOption, got: {e:?}"),
            Ok(_) => panic!("expected error from missing Image option"),
        }
    }

    /// The `AgentRuntimeEngine` trait impl must reject sandbox-paradigm options
    /// with a clear `OptionVariantMismatch` error — never silently fall back
    /// or panic.
    #[test]
    fn container_runtime_via_trait_rejects_sandbox_options() {
        use crate::engine::agent_runtime::AgentRuntimeEngine;

        let rt = ContainerRuntime::docker();
        let opts = ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::default());
        match <ContainerRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker");
                assert_eq!(got, "sandbox");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn apple_runtime_via_trait_rejects_sandbox_options() {
        use crate::engine::agent_runtime::AgentRuntimeEngine;

        let rt = ContainerRuntime::apple();
        let opts = ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::default());
        match <ContainerRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "apple-containers");
                assert_eq!(got, "sandbox");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn cli_backends_state_their_reattach_model() {
        let docker = ContainerRuntime::docker();
        let apple = ContainerRuntime::apple();
        assert!(<ContainerRuntime as AgentRuntimeEngine>::reattach_after_owner_exit(&docker));
        assert!(!<ContainerRuntime as AgentRuntimeEngine>::reattach_after_owner_exit(&apple));
    }

    #[test]
    fn builtin_process_settings_hold_only_process_wide_values() {
        use crate::data::config::builtin_runtime::BuiltinRuntimeConfig;
        use crate::data::config::env::{EnvSnapshot, AWMAN_BUILTIN_STATE_DIR, MSB_PATH};
        use crate::data::config::{FlagConfig, GlobalConfig, RepoConfig};

        let env = EnvSnapshot::with_overrides([
            (AWMAN_BUILTIN_STATE_DIR, "/s"),
            (MSB_PATH, "/elsewhere/msb"),
        ]);
        let layered = |global: &str, repo: &str| {
            EffectiveConfig::new(
                FlagConfig::default(),
                env.clone(),
                RepoConfig {
                    builtin: Some(serde_json::from_str(repo).unwrap()),
                    ..Default::default()
                },
                GlobalConfig {
                    builtin: Some(serde_json::from_str(global).unwrap()),
                    ..Default::default()
                },
            )
        };
        let resolved = layered("{}", "{}").builtin_process_settings().unwrap();
        let settings = BuiltinRuntimeSettings::resolve(&resolved);
        assert_eq!(
            settings,
            BuiltinRuntimeSettings {
                state_dir: PathBuf::from("/s"),
                ambient_overrides: vec![MSB_PATH],
                test_isolation: true,
            },
            "unit tests are always isolated"
        );

        // Layer 0's complete resolved value validates resources and policy.
        for (global, repo) in [
            (
                r#"{"network":{"mode":"none"}}"#,
                r#"{"network":{"mode":"public"}}"#,
            ),
            (r#"{}"#, r#"{"network":{"hostPorts":[8765]}}"#),
            (r#"{}"#, r#"{"vcpus":0}"#),
            (r#"{}"#, r#"{"memoryMib":64}"#),
        ] {
            assert!(
                layered(global, repo).builtin_settings().is_err(),
                "{repo} must fail during Layer 0 resolution"
            );
        }

        // A meaningless image source is still refused up front.
        let bad_source = EffectiveConfig::new(
            FlagConfig::default(),
            env.clone(),
            RepoConfig::default(),
            GlobalConfig {
                builtin: Some(BuiltinRuntimeConfig {
                    image_source: Some(
                        crate::data::config::image_source::ImageSourceSpec::Archive {
                            path: "".into(),
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert!(matches!(
            bad_source.builtin_settings(),
            Err(crate::data::error::DataError::BuiltinConfig(_))
        ));

        // The ambient override is refused before anything else.
        match ContainerRuntime::builtin(settings) {
            Err(EngineError::AmbientRuntimeOverride { variable }) => assert_eq!(variable, MSB_PATH),
            Err(e) => panic!("expected AmbientRuntimeOverride, got {e:?}"),
            Ok(_) => panic!("expected an error"),
        }
    }
}

#[cfg(test)]
mod inspect_default_tests {
    use super::*;
    use crate::engine::agent_runtime::AgentRuntimeEngine;
    use crate::engine::container::test_support::holding_runtime;

    fn handle() -> AgentHandle {
        AgentHandle {
            id: "id1".into(),
            name: "awman-x".into(),
            image_tag: "img".into(),
            started_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn container_runtime_inspect_detects_missing_agent() {
        let rt = holding_runtime(Default::default());
        let err = AgentRuntimeEngine::inspect(rt.as_ref(), &handle()).unwrap_err();
        assert!(err.to_string().contains("no longer exists"));
    }

    #[test]
    fn container_runtime_list_stopped_forwards_to_the_backend() {
        let rt = holding_runtime(Default::default());
        assert!(AgentRuntimeEngine::list_stopped(rt.as_ref())
            .unwrap()
            .is_empty());
    }
}
