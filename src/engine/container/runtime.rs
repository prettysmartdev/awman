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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::config::env::test_isolation_active;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};
use crate::data::oci_identity::ImageIdentity;
use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::{
    AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ImageAcquisition,
    ImageImportRequest, ImportedImage, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::container::apple::AppleBackend;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::background::BackgroundContainer;
use crate::engine::container::docker::DockerBackend;
use crate::engine::container::options::{OverlaySpec, ResolvedContainerOptions};
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
    arbitrary_env_vars: true,
    arbitrary_host_mounts: true,
    cpu_limits: true,
    per_resource_stats: true,
    persistent_lifecycle: false,
    kit_declarative: false,
    dind: DindSupport::OnRequest,
    host_paths_visible: true,
    session_label_supported: true,
    has_image_store: true,
    image_acquisition: ImageAcquisition::Build,
    fractional_cpu: true,
};

/// Everything the builtin microVM backend needs, resolved from global config,
/// repo config and the environment by the caller (Layer 2, or
/// `agent_runtime::detect`) before `ContainerRuntime::builtin` is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinRuntimeSettings {
    /// Private state root (catalog, caches, control sockets).
    pub state_dir: PathBuf,
    /// Whole vCPUs per agent VM.
    pub vcpus: u8,
    /// Guest memory per agent VM, in MiB.
    pub memory_mib: u32,
    /// Default image source for images that are not cached.
    pub image_source: Option<ImageSourceSpec>,
    /// Per-image source overrides keyed by awman image tag.
    pub images: BTreeMap<String, ImageSourceSpec>,
    /// Registry host settings keyed by `host[:port]`.
    pub registries: BTreeMap<String, RegistryHostConfig>,
    /// The ambient `MSB_*` overrides present in the environment. Any entry
    /// makes the backend refuse to start (`AmbientRuntimeOverride`).
    pub ambient_overrides: Vec<&'static str>,
    /// Running under test isolation without `AWMAN_TEST_BUILTIN`: the real
    /// runtime must not be touched.
    pub test_isolation: bool,
}

impl BuiltinRuntimeSettings {
    /// Resolve from the effective config (repo `builtin` block merged over the
    /// global one) and its environment snapshot. Invalid values — zero
    /// resources, a meaningless image source — are a configuration error.
    pub fn resolve(config: &EffectiveConfig) -> Result<Self, EngineError> {
        let builtin = config.builtin_runtime();
        builtin.validate().map_err(EngineError::Config)?;
        let env = config.env();
        Ok(Self {
            state_dir: config.builtin_state_dir()?,
            vcpus: builtin.vcpus_or_default(),
            memory_mib: builtin.memory_mib_or_default(),
            image_source: builtin.image_source,
            images: builtin.images,
            registries: builtin.registries,
            ambient_overrides: env.ambient_msb_overrides(),
            test_isolation: test_isolation_active() && !env.test_builtin(),
        })
    }
}

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

    /// Construct with the builtin microVM backend. Errors — never panics —
    /// when awman was built without the runtime, on an unsupported target,
    /// when an ambient `MSB_*` override is set, or when the state directory
    /// cannot be secured.
    pub fn builtin(settings: BuiltinRuntimeSettings) -> Result<Self, EngineError> {
        Ok(Self {
            backend: crate::engine::container::builtin::open(settings)?,
        })
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

    /// The host CLI the chosen backend drives, if any.
    pub fn host_cli(&self) -> Option<&'static str> {
        self.backend.host_cli()
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
        tag: &str,
        dockerfile: &Path,
        context: &Path,
        no_cache: bool,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        self.backend
            .build_image(tag, dockerfile, context, no_cache, on_line)
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

    /// Argv (after the host CLI name) for `exec -it` into a running
    /// container. `None` when the backend drives no host CLI.
    pub fn exec_args(
        &self,
        container_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Option<Vec<String>> {
        self.backend
            .exec_args(container_id, working_dir, entrypoint, env_vars)
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
    /// later `exec` and `kill` calls flow through the same trait.
    pub fn start_background(
        &self,
        image: &str,
        workdir: &Path,
        env: &HashMap<String, String>,
        overlays: &[OverlaySpec],
    ) -> Result<BackgroundContainer, EngineError> {
        let container_id = self
            .backend
            .start_background(image, workdir, env, overlays)?;
        let workdir_str = workdir.display().to_string();
        Ok(BackgroundContainer::new(
            container_id,
            Arc::clone(&self.backend),
            workdir_str,
        ))
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

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        ContainerRuntime::stop(self, handle)
    }

    fn exec_args(
        &self,
        agent_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Option<Vec<String>> {
        ContainerRuntime::exec_args(self, agent_id, working_dir, entrypoint, env_vars)
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        ContainerRuntime::attach(self, handle)
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        ContainerRuntime::list_running_with_name_prefix(self, prefix)
    }

    fn host_cli(&self) -> Option<&'static str> {
        ContainerRuntime::host_cli(self)
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
        tag: &str,
        dockerfile: &Path,
        context: &Path,
        no_cache: bool,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        ContainerRuntime::build_image(self, tag, dockerfile, context, no_cache, on_line)
    }

    fn import_image(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.backend.import_image(request, sink)
    }

    fn image_identity(&self, tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        self.backend.image_identity(tag)
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
    fn cli_backends_state_their_host_cli_and_reattach_model() {
        let docker = ContainerRuntime::docker();
        let apple = ContainerRuntime::apple();
        assert_eq!(docker.host_cli(), Some("docker"));
        assert_eq!(apple.host_cli(), Some("container"));
        assert!(<ContainerRuntime as AgentRuntimeEngine>::reattach_after_owner_exit(&docker));
        assert!(!<ContainerRuntime as AgentRuntimeEngine>::reattach_after_owner_exit(&apple));
        for rt in [&docker, &apple] {
            let args = rt
                .exec_args("ctr", "/w", &["sh"], &[])
                .expect("CLI backends have exec argv");
            assert_eq!(args, vec!["exec", "-it", "-w", "/w", "ctr", "sh"]);
        }
    }

    #[test]
    fn builtin_settings_apply_defaults_and_reject_invalid_config() {
        use crate::data::config::builtin_runtime::{
            BuiltinRuntimeConfig, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS,
        };
        use crate::data::config::env::{EnvSnapshot, AWMAN_BUILTIN_STATE_DIR, MSB_PATH};
        use crate::data::config::{FlagConfig, GlobalConfig, RepoConfig};

        let env = EnvSnapshot::with_overrides([
            (AWMAN_BUILTIN_STATE_DIR, "/s"),
            (MSB_PATH, "/elsewhere/msb"),
        ]);
        let effective = |builtin: BuiltinRuntimeConfig| {
            EffectiveConfig::new(
                FlagConfig::default(),
                env.clone(),
                RepoConfig::default(),
                GlobalConfig {
                    builtin: Some(builtin),
                    ..Default::default()
                },
            )
        };
        let settings =
            BuiltinRuntimeSettings::resolve(&effective(BuiltinRuntimeConfig::default())).unwrap();
        assert_eq!(settings.state_dir, PathBuf::from("/s"));
        assert_eq!(settings.vcpus, DEFAULT_VCPUS);
        assert_eq!(settings.memory_mib, DEFAULT_MEMORY_MIB);
        assert_eq!(settings.ambient_overrides, vec![MSB_PATH]);
        assert!(settings.test_isolation, "unit tests are always isolated");

        let invalid = effective(BuiltinRuntimeConfig {
            vcpus: Some(0),
            ..Default::default()
        });
        assert!(matches!(
            BuiltinRuntimeSettings::resolve(&invalid),
            Err(EngineError::Config(_))
        ));

        // The ambient override is refused before anything else.
        match ContainerRuntime::builtin(settings) {
            Err(EngineError::AmbientRuntimeOverride { variable }) => assert_eq!(variable, MSB_PATH),
            Err(e) => panic!("expected AmbientRuntimeOverride, got {e:?}"),
            Ok(_) => panic!("expected an error"),
        }
    }
}
