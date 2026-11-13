//! `engine::agent` — `AgentEngine`. Cross-cutting agent concerns called by
//! `chat`, `exec`, and `ready`.
//!
//! Every agent is described by one `AgentDefinition` (`definition.rs`),
//! resolved through `AgentRegistry` (`registry.rs`). All first-party
//! agent-name branching lives in `first_party.rs`; any other agent is a
//! custom definition. Option builders resolve once and pass the definition
//! down.

use std::{path::Path, sync::Arc};

use crate::data::config::agents::LaunchMode;
use crate::data::config::builtin_launch::BuiltinLaunchSettings;
use crate::data::config::effective::EffectiveConfig;
use crate::data::image_tags::{agent_image_tag, project_image_tag};
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{AgentSetupStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::auth::{AgentCredentials, CredentialDelivery, RefreshableCredentialDelivery};
use crate::engine::container::options::{
    ContainerOption, EnvLiteral, EnvVar, ImageRef, PlanMode, YoloMode,
};
use crate::engine::error::EngineError;
use crate::engine::overlay::{ContextOverlay, DirectorySpec, OverlayEngine, OverlayRequest};
use crate::engine::sandbox::options::SandboxOption;

pub mod custom;
pub mod definition;
pub mod download;
pub mod first_party;
pub mod frontend;
pub mod registry;

pub use definition::*;
pub use frontend::AgentImageFrontend;
pub use registry::{AgentRegistry, ResolvedAgent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerfileSync {
    Unchanged,
    Written,
    UserOwned,
}

/// Reviewable result of comparing the file on disk with the approved recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerfilePreview {
    pub action: DockerfileSync,
    pub previous_hash: Option<String>,
    pub differs_from_render: bool,
    pub warnings: Vec<String>,
    rendered: Option<String>,
}

impl PartialEq<DockerfileSync> for DockerfilePreview {
    fn eq(&self, other: &DockerfileSync) -> bool {
        self.action == *other
    }
}

impl DockerfilePreview {
    pub fn definition_changed(&self) -> bool {
        self.action == DockerfileSync::Written && self.previous_hash.is_some()
    }
}

/// Inspect without writing, so approval UIs can describe the Dockerfile actually used.
pub fn preview_agent_dockerfile(
    def: &ResolvedAgent,
    paths: &RepoDockerfilePaths,
    project_tag: &str,
) -> Result<DockerfilePreview, EngineError> {
    let path = paths.agent_dockerfile(&def.name);
    let existing = match std::fs::read_to_string(&path) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(EngineError::io(&path, e)),
    };
    let mut preview = DockerfilePreview {
        action: DockerfileSync::Written,
        previous_hash: existing.as_deref().and_then(custom::header_hash),
        differs_from_render: false,
        warnings: vec![],
        rendered: None,
    };
    if matches!(def.image, AgentImageSource::Download) {
        if existing.is_some() {
            preview.action = DockerfileSync::Unchanged;
        }
        return Ok(preview);
    }
    let rendered = custom::render_dockerfile(def, project_tag)?;
    preview.warnings = rendered.warnings;
    preview.differs_from_render = existing
        .as_ref()
        .is_some_and(|body| body != &rendered.content);
    if let Some(existing) = existing {
        if !existing.starts_with(custom::DOCKERFILE_HEADER_PREFIX) {
            preview.action = DockerfileSync::UserOwned;
            preview.warnings.push(format!(
                "{}; the existing Dockerfile will be built instead of the definition's recipe",
                user_owned_dockerfile_note(&def.name)
            ));
        } else if let AgentOrigin::Custom { sha256, .. } = &def.origin {
            if preview.previous_hash.as_ref() == Some(sha256) {
                preview.action = DockerfileSync::Unchanged;
                if preview.differs_from_render {
                    preview.warnings.push(format!("{} differs from the approved definition's render; the existing Dockerfile will be used. Remove the header line to take ownership, or delete the file to regenerate it", path.display()));
                }
            }
        }
    }
    preview.rendered = Some(rendered.content);
    Ok(preview)
}

/// Synchronize a generated Dockerfile without overwriting a hand-maintained one.
pub async fn provision_agent_dockerfile(
    def: &ResolvedAgent,
    paths: &RepoDockerfilePaths,
    project_tag: &str,
) -> Result<DockerfilePreview, EngineError> {
    let path = paths.agent_dockerfile(&def.name);
    let preview = preview_agent_dockerfile(def, paths, project_tag)?;
    if preview.action == DockerfileSync::Written {
        if let Some(content) = &preview.rendered {
            download::atomic_write(&path, content.as_bytes())?;
        } else {
            download::download_agent_dockerfile(&def.name, &path, project_tag).await?;
        }
    }
    Ok(preview)
}

pub fn custom_build_failure(def: &AgentDefinition, error: impl std::fmt::Display) -> String {
    match (&def.origin, &def.image) {
        (AgentOrigin::Custom { path, .. }, AgentImageSource::Rendered { spec }) => {
            format!(
                "{error}\nCheck [image] in {}: smoke binary '{}' must be on PATH. Install steps:\n{}",
                path.display(),
                spec.binary,
                spec.install
                    .iter()
                    .map(|s| format!("  {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
        _ => error.to_string(),
    }
}

pub(crate) fn user_owned_dockerfile_note(name: &str) -> String {
    format!(
        "using hand-maintained .awman/Dockerfile.{name}; delete it to regenerate from {name}.toml"
    )
}

fn custom_auth_env_in(
    env: &crate::data::config::env::EnvSnapshot,
    def: &AgentDefinition,
    overlays: &[String],
) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = overlays.iter().map(String::as_str).collect();
    def.auth_env
        .iter()
        .filter(|name| env.get(name).is_some() && seen.insert(name.as_str()))
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoMode {
    #[default]
    Disabled,
    Enabled,
}

/// Options governing how an agent container is invoked.
#[derive(Debug, Default, Clone)]
pub struct AgentRunOptions {
    /// Caller values are used only for explicit env overlay names.
    pub command_secrets: Option<Arc<crate::data::secret::CommandSecrets>>,
    pub yolo: Option<crate::engine::container::options::YoloMode>,
    pub auto: Option<crate::engine::container::options::AutoMode>,
    pub plan: Option<crate::engine::container::options::PlanMode>,
    /// How awman talks to the agent process's stdio: raw PTY/stdio
    /// (`LaunchMode::Stdio`, the default and today's behaviour) or a
    /// newline-delimited JSON-RPC 2.0 ACP channel (`LaunchMode::Acp`). Threaded
    /// through exactly like `plan`/`yolo`/`auto`; `Default` is `Stdio`, so
    /// existing call sites keep raw-stdio behaviour with no change.
    pub launch_mode: LaunchMode,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub initial_prompt: Option<String>,
    pub allow_docker: bool,
    pub non_interactive: bool,
    /// Opt in to translated structured stdout for API launches.
    pub structured_output: bool,
    /// Optional explicit model name; if `None`, the engine emits no model flag.
    pub model: Option<String>,
    /// Optional explicit env-passthrough list (name only). If `None`, falls
    /// through to `EffectiveConfig::env_passthrough`.
    pub env_passthrough: Option<Vec<String>>,
    /// User-supplied directory overlays.
    pub directory_overlays: Vec<DirectorySpec>,
    /// Host-authored outputs and verified Git sidecars, never parsed from
    /// repository/config/workflow overlay expressions.
    pub command_mounts: Vec<DirectorySpec>,
    /// When true, mount all skill directories.
    pub include_all_skills: bool,
    /// Named skills to mount (when `include_all_skills` is false).
    pub named_skills: Vec<String>,
    /// Override the image tag used for the container and for `image_home_dir`
    /// inspection. When `Some`, this tag is used instead of deriving one from
    /// `session.git_root()`. Needed when the session is rooted at a worktree
    /// but the image was built from the original repo root.
    pub image_tag_override: Option<String>,
    /// Combined, pre-rendered system-prompt text (from `ContextPromptBuilder`).
    pub system_prompt: Option<String>,
    /// Resolved context-directory overlays for this run.
    pub context_overlays: Vec<ContextOverlay>,
    /// `--agent-flag` values the command has scoped to this agent. `Some`
    /// replaces the configured `agentFlags` entry; `None` resolves it from the
    /// session (see [`resolve_agent_flags`]).
    pub agent_flags: Option<Vec<String>>,
}

/// Extra user arguments appended to `agent`'s launch command: the run's
/// explicit `--agent-flag` list, else the session's effective `agentFlags`.
impl AgentRunOptions {
    pub fn secret_env(&self) -> Vec<(String, crate::data::secret::SecretString)> {
        self.env_passthrough
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|name| {
                self.command_secrets
                    .as_ref()?
                    .get(name)
                    .map(|value| (name.clone(), value.clone()))
            })
            .collect()
    }
}

fn resolve_agent_flags(session: &Session, agent: &AgentName, run: &AgentRunOptions) -> Vec<String> {
    match &run.agent_flags {
        Some(values) => values.clone(),
        None => session.effective_config().agent_flags(agent.as_str()),
    }
}

/// `run` with the session's effective `yoloDisallowedTools` added to its
/// disallowed-tools list when the run is a `--yolo` launch. Any other run, or
/// an empty configured list, is returned unchanged (borrowed).
///
/// An agent that cannot deliver a disallowed-tools list on `paradigm` is
/// refused with [`EngineError::YoloDisallowedToolsUnsupported`] — the same
/// fail-closed rule as an explicit list, but naming the config key the list
/// came from.
fn with_yolo_disallowed_tools<'a>(
    session: &Session,
    def: &AgentDefinition,
    run: &'a AgentRunOptions,
    paradigm: RunParadigm,
) -> Result<std::borrow::Cow<'a, AgentRunOptions>, EngineError> {
    use std::borrow::Cow;
    if !matches!(run.yolo, Some(YoloMode::Enabled)) {
        return Ok(Cow::Borrowed(run));
    }
    let configured = session.effective_config().yolo_disallowed_tools();
    if configured.is_empty() {
        return Ok(Cow::Borrowed(run));
    }
    def.validate_tool_restrictions(&[], &configured, paradigm)
        .map_err(|_| EngineError::YoloDisallowedToolsUnsupported {
            agent: def.name.clone(),
            tools: configured.clone(),
        })?;
    let mut merged = run.clone();
    for tool in configured {
        if !merged.disallowed_tools.contains(&tool) {
            merged.disallowed_tools.push(tool);
        }
    }
    Ok(Cow::Owned(merged))
}

/// Writable scratch directory of a read-only workspace launch.
pub const READ_ONLY_SCRATCH_PATH: &str = "/awman/scratch";
/// Environment variable naming [`READ_ONLY_SCRATCH_PATH`] inside the container.
pub const READ_ONLY_SCRATCH_ENV: &str = "AWMAN_SCRATCH";

fn read_only_unsupported(runtime: &str) -> EngineError {
    EngineError::Config(format!(
        "the {runtime} runtime cannot mount the workspace read-only; this run requires a read-only workspace"
    ))
}

/// Runs after all overlays are assembled and before any runtime sees them.
fn validate_workspace_mounts(
    options: &mut [ContainerOption],
    workspace: &Path,
    read_only: bool,
    lease_scope: Option<&Path>,
    inline_file: Option<&Path>,
    command_mounts: &[crate::engine::container::options::OverlaySpec],
) -> Result<(), EngineError> {
    use crate::engine::container::options::OverlayPermission;
    let workspace = workspace
        .canonicalize()
        .map_err(|e| EngineError::io(workspace, e))?;
    let inline_file = inline_file.and_then(|path| path.canonicalize().ok());
    let lease_root = lease_scope
        .map(|scope| scope.canonicalize())
        .transpose()
        .map_err(|e| EngineError::Config(format!("lease workspace: {e}")))?;
    for option in options {
        let ContainerOption::Overlay(spec) = option else {
            continue;
        };
        let host = spec
            .host_path
            .canonicalize()
            .map_err(|e| EngineError::io(&spec.host_path, e))?;
        if let Some(clone) = &lease_root {
            if !host.starts_with(clone)
                && inline_file.as_deref() != Some(host.as_path())
                && !command_mounts.iter().any(|allowed| {
                    // Compare resolved paths: on macOS a temporary directory is
                    // reached through a symlink (/var -> /private/var).
                    allowed
                        .host_path
                        .canonicalize()
                        .unwrap_or_else(|_| allowed.host_path.clone())
                        == host
                        && allowed.container_path == spec.container_path
                        && allowed.permission == spec.permission
                })
            {
                return Err(EngineError::Config(
                    "overlay exposes a path outside the lease workspace".into(),
                ));
            }
        }
        let guest = &spec.container_path;
        if guest.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        }) {
            return Err(EngineError::Config(
                "overlay guest path must be normalized".into(),
            ));
        }
        let guest_workspace = Path::new("/workspace");
        if guest_workspace.starts_with(guest) && (guest != guest_workspace || host != workspace) {
            return Err(EngineError::Config(
                "overlay overlaps the workspace mount".into(),
            ));
        }
        if read_only
            && (host.starts_with(&workspace)
                || workspace.starts_with(&host)
                || guest.starts_with(guest_workspace))
        {
            spec.permission = OverlayPermission::ReadOnly;
        }
        if inline_file.as_deref() == Some(host.as_path()) {
            spec.permission = OverlayPermission::ReadOnly;
        }
        spec.host_path = host;
    }
    Ok(())
}

#[derive(Clone)]
pub struct AgentEngine {
    overlay_engine: Arc<OverlayEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    /// Who registers credential-refresh leases for the containers this engine
    /// builds options for (F-38). `None` — the default — disables leases, the
    /// same behaviour the absent process-global monitor used to produce.
    lease_factory: Option<crate::engine::credential_refresh::LeaseFactoryHandle>,
    /// Mount the session workspace read-only (pulled leases with
    /// `exec.read_only`), with a writable scratch directory at
    /// [`READ_ONLY_SCRATCH_PATH`]. The agent home stays writable.
    read_only_workspace: bool,
    lease_workspace_scope: Option<std::path::PathBuf>,
}

impl AgentEngine {
    pub fn new(overlay_engine: Arc<OverlayEngine>, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self {
            overlay_engine,
            runtime,
            lease_factory: None,
            read_only_workspace: false,
            lease_workspace_scope: None,
        }
    }

    /// Mount `/workspace` read-only for every launch this engine prepares.
    /// A runtime that cannot enforce it (see
    /// [`crate::engine::agent_runtime::Capabilities::read_only_workspace`])
    /// refuses the launch rather than mounting the workspace writable.
    pub fn with_read_only_workspace(mut self, read_only: bool) -> Self {
        self.read_only_workspace = read_only;
        self
    }

    /// Scope every final host bind to this lease's clone. Private and sibling
    /// lease state may never be exposed through an extra overlay.
    pub fn with_lease_workspace_scope(mut self, clone: std::path::PathBuf) -> Self {
        self.lease_workspace_scope = Some(clone);
        self
    }

    pub fn is_lease_workspace(&self) -> bool {
        self.lease_workspace_scope.is_some()
    }
    pub fn read_only_workspace(&self) -> bool {
        self.read_only_workspace
    }

    /// The `/workspace` mount, plus the writable scratch directory when the
    /// workspace is read-only.
    fn workspace_mounts(&self, session: &Session) -> Result<Vec<ContainerOption>, EngineError> {
        use crate::engine::container::options::{MountGuard, OverlayPermission, OverlaySpec};
        let permission = if self.read_only_workspace {
            if !self.runtime.capabilities().read_only_workspace() {
                return Err(read_only_unsupported(self.runtime.display_name()));
            }
            OverlayPermission::ReadOnly
        } else {
            OverlayPermission::ReadWrite
        };
        let mut mounts = vec![ContainerOption::Overlay(OverlaySpec {
            host_path: session.git_root().to_path_buf(),
            container_path: std::path::PathBuf::from("/workspace"),
            permission,
        })];
        if self.read_only_workspace {
            let scratch = tempfile::Builder::new()
                .prefix("awman-scratch-")
                .tempdir()
                .map_err(|e| EngineError::Other(format!("create scratch directory: {e}")))?;
            mounts.push(ContainerOption::TemporaryDirectoryMount {
                container_path: std::path::PathBuf::from(READ_ONLY_SCRATCH_PATH),
                env_var: Some(READ_ONLY_SCRATCH_ENV.to_string()),
                guard: MountGuard::new_dir(scratch),
            });
        }
        Ok(mounts)
    }

    /// Attach the credential-refresh lease factory this engine's launches
    /// should register with. The command layer calls this when the effective
    /// config enables `authRefresh`; leaving it off keeps leases disabled.
    pub fn with_lease_factory(
        mut self,
        factory: Option<crate::engine::credential_refresh::LeaseFactoryHandle>,
    ) -> Self {
        self.lease_factory = factory;
        self
    }

    /// The runtime this engine prepares agents on. Callers that only need to
    /// ask whether an image exists take a cheap clone rather than a second
    /// handle of their own (F-40b replaced `container_runtime_arc`, which
    /// forced them to name the container tier).
    pub fn runtime(&self) -> &Arc<dyn AgentRuntimeEngine> {
        &self.runtime
    }

    /// [`Self::ensure_available`] with the session's own layered config and
    /// the runtime's image probe — what every launch command must use. The
    /// builtin runtime's image source and builder setting come from that
    /// config, so passing anything else misreports a configured source.
    pub async fn ensure_available_for_session(
        &self,
        session: &Session,
        agent: &AgentName,
        frontend: &mut dyn AgentImageFrontend,
    ) -> Result<(), EngineError> {
        let runtime = Arc::clone(&self.runtime);
        self.ensure_available(
            session,
            agent,
            &session.effective_config(),
            frontend,
            move |tag: &str| runtime.image_exists(tag).unwrap_or(false),
        )
        .await
    }

    /// Ensure the agent's Dockerfile and image are available locally. Reports
    /// progress via `frontend`. Idempotent: when both Dockerfile and image
    /// exist, no `report_step_status` calls fire.
    ///
    /// `image_exists` is injected so callers in tests can avoid shelling out
    /// to Docker. Production callers pass a closure over the session's
    /// `ContainerRuntime::image_exists`.
    pub async fn ensure_available(
        &self,
        session: &Session,
        agent: &AgentName,
        config: &EffectiveConfig,
        frontend: &mut dyn AgentImageFrontend,
        image_exists: impl Fn(&str) -> bool,
    ) -> Result<(), EngineError> {
        let def = self.resolve_definition(session, agent)?;
        let paths = RepoDockerfilePaths::new(session.git_root());
        let project_tag = project_image_tag(session.git_root());
        let agent_tag = agent_image_tag(session.git_root(), agent.as_str());
        let sync = provision_agent_dockerfile(&def, &paths, &project_tag).await?;
        for warning in &sync.warnings {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: warning.clone(),
            });
        }
        if sync.definition_changed() && def.is_custom() && image_exists(&agent_tag) {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: format!("agent definition '{agent}' changed since its image was built; run 'awman ready --agent {agent} --build'"),
            });
        }
        if self.runtime.capabilities().image_acquisition
            == crate::engine::agent_runtime::ImageAcquisition::Import
        {
            use crate::data::ready_image::ReadyImagePlan;
            use crate::engine::image_prep::{
                AgentImagePreparer, AgentImageRequest, BuildPolicy, ImageBuildContext,
            };
            let tag = agent_image_tag(session.git_root(), agent.as_str());
            let resolved = config.builtin_settings()?;
            let builder_enabled = resolved.builder_enabled;
            let mut preparer = AgentImagePreparer::new(
                resolved,
                self.runtime.clone(),
                crate::data::fs::data_paths::data_home(config.env())?,
                // Default agent = the configured one, as `ready` and preflight
                // use it, so a single-image default source is refused for
                // any other agent here too.
                ImageBuildContext::for_session(session, session.configured_agent().as_str()),
            )
            .with_definition_env(session.env().clone())
            .with_definition(def.clone());
            // With the builder on, a launch whose image is missing prepares
            // it as the building runtimes do: build into the source store
            // only when it lacks the image, then import (API session setup's
            // policy). An installed image keeps `CheckOnly`'s provenance
            // check, and without a usable source `CheckOnly` reports the
            // typed configuration error.

            let missing = builder_enabled && !self.runtime.image_exists(&tag)?;
            let policy = match missing.then(|| preparer.missing_image_plan(&tag, agent.as_str())) {
                Some(ReadyImagePlan::BuildAndImport | ReadyImagePlan::Import) => {
                    BuildPolicy::SessionSetup
                }
                _ => BuildPolicy::CheckOnly,
            };
            let result = preparer
                .prepare(
                    &AgentImageRequest {
                        tag: &tag,
                        agent: agent.as_str(),
                        policy,
                    },
                    frontend,
                )
                .await?;
            return match (result.build, result.import) {
                (StepStatus::Failed(error), _) => Err(EngineError::Other(format!(
                    "failed to build image for agent '{agent}' ({tag}): {error}"
                ))),
                (_, StepStatus::Failed(error)) => Err(EngineError::Other(format!(
                    "failed to import image for agent '{agent}' ({tag}): {error}"
                ))),
                (_, StepStatus::Warn(reason)) => Err(EngineError::Other(format!(
                    "cannot import image for agent '{agent}' ({tag}): {reason}"
                ))),
                _ => Ok(()),
            };
        }

        // Check for the project base image. If absent, fail with a structured
        // error: agent images are layered FROM the project image.
        let project_tag = project_image_tag(session.git_root());
        if !image_exists(&project_tag) {
            return Err(EngineError::AgentRequiresProjectImage { tag: project_tag });
        }

        let paths = RepoDockerfilePaths::new(session.git_root());
        let agent_dockerfile = paths.agent_dockerfile(agent.as_str());
        let agent_tag = agent_image_tag(session.git_root(), agent.as_str());

        // Ensure agent image is built.
        if !image_exists(&agent_tag) {
            frontend.report_step_status(&AgentSetupStep::BuildingImage.into(), StepStatus::Running);
            let _container = frontend.container_frontend();
            let mut sink = |line: &str| {
                frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
            };
            match self.runtime.build_image(
                &crate::engine::container::BuildRequest::runtime(
                    &agent_tag,
                    &agent_dockerfile,
                    session.git_root(),
                    false,
                ),
                &mut sink,
            ) {
                Ok(()) => {
                    frontend.report_step_status(
                        &AgentSetupStep::BuildingImage.into(),
                        StepStatus::Done,
                    );
                }
                Err(EngineError::ImageBuildExitNonzero { exit_code, .. }) => {
                    let error = EngineError::AgentImageBuildFailed {
                        agent: agent.as_str().to_string(),
                        exit_code,
                    };
                    let message = custom_build_failure(&def, &error);
                    frontend.report_step_status(
                        &AgentSetupStep::BuildingImage.into(),
                        StepStatus::Failed(message.clone()),
                    );
                    return Err(if def.is_custom() {
                        EngineError::Other(message)
                    } else {
                        error
                    });
                }
                Err(e) => {
                    let msg = custom_build_failure(&def, &e);
                    frontend.report_step_status(
                        &AgentSetupStep::BuildingImage.into(),
                        StepStatus::Failed(msg.clone()),
                    );
                    return Err(if def.is_custom() {
                        EngineError::Other(msg)
                    } else {
                        e
                    });
                }
            }
        }

        Ok(())
    }

    /// This session's builtin launch settings (default vCPUs and memory,
    /// network policy) when the engine's runtime sizes and confines each
    /// launch from them (`Capabilities::per_launch_vm_settings`), resolved
    /// from the session's own `EffectiveConfig` for every launch; `None` for
    /// every other runtime, which has no use for them and must not fail on a
    /// `builtin` block it never reads.
    ///
    /// `build_options` carries the value as `ContainerOption::BuiltinLaunch`.
    /// Background setup/teardown launches pass it in their
    /// `BackgroundLaunchRequest`.
    pub(crate) fn builtin_launch_settings(
        &self,
        session: &Session,
    ) -> Result<Option<BuiltinLaunchSettings>, EngineError> {
        if !self.runtime.capabilities().per_launch_vm_settings {
            return Ok(None);
        }
        Ok(Some(session.effective_config().builtin_launch_settings()?))
    }

    /// The common launch boundary: trust first, then the runtime paradigm,
    /// before options, kits, Dockerfiles or images can be prepared.
    pub(crate) fn resolve_definition(
        &self,
        session: &Session,
        agent: &AgentName,
    ) -> Result<ResolvedAgent, EngineError> {
        let def = AgentRegistry::resolve_for_session(session, agent).map_err(|error| {
            if matches!(error, EngineError::UnknownAgent { .. }) {
                AgentRegistry::unknown(
                    session.env(),
                    agent.as_str(),
                    Some(session.definition_root()),
                    self.runtime.capabilities().kit_declarative,
                )
            } else {
                error
            }
        })?;
        if self.runtime.capabilities().kit_declarative && def.is_custom() {
            return Err(EngineError::CustomAgentRuntimeUnsupported {
                name: def.name.clone(),
                runtime: self.runtime.runtime_name().to_string(),
            });
        }
        Ok(def)
    }

    /// Discovery alone is insufficient: deleted, unapproved and unsupported agents
    /// must not be offered to a workflow leader. Keep each failure visible by name.
    pub(crate) fn filter_available_agents(
        &self,
        session: &Session,
        discovered: Vec<(String, std::path::PathBuf)>,
    ) -> (Vec<(String, std::path::PathBuf)>, Vec<String>) {
        let mut available = Vec::new();
        let mut warnings = Vec::new();
        for (name, path) in discovered {
            let result = AgentName::new(&name)
                .map_err(EngineError::from)
                .and_then(|agent| self.resolve_definition(session, &agent));
            match result {
                Ok(_) => available.push((name, path)),
                Err(error) => warnings.push(format!("agent '{name}' is unavailable: {error}")),
            }
        }
        (available, warnings)
    }

    /// Build the `ContainerOption` list for running an agent container.
    pub fn build_options(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
    ) -> Result<Vec<ContainerOption>, EngineError> {
        self.build_options_with_credentials(session, agent, run, &AgentCredentials::default())
    }

    /// Build container options while carrying a resolved credential delivery.
    /// The ordinary `build_options` wrapper deliberately remains credential-free
    /// for existing callers that only prepare a command preview.
    pub fn build_options_with_credentials(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
        credentials: &AgentCredentials,
    ) -> Result<Vec<ContainerOption>, EngineError> {
        let def = self.resolve_definition(session, agent)?;
        // A `--yolo` launch carries the configured `yoloDisallowedTools`.
        let yolo_run = with_yolo_disallowed_tools(session, &def, run, RunParadigm::Container)?;
        let run: &AgentRunOptions = &yolo_run;
        if self.lease_workspace_scope.is_some() && run.allow_docker {
            return Err(EngineError::Config(
                "leases cannot expose the host Docker socket".into(),
            ));
        }
        // Plan-mode support, ACP support and the plan/yolo conflict are all
        // definition facts, checked identically on both paradigms.
        def.validate_run(run, RunParadigm::Container)?;
        // Resolved first so a repo block that cannot be honoured (a widening
        // network policy, a foreign state directory) fails this launch before
        // anything else is prepared for it.
        let builtin_launch = self.builtin_launch_settings(session)?;

        let image_tag = run
            .image_tag_override
            .clone()
            .unwrap_or_else(|| agent_image_tag(session.git_root(), agent.as_str()));
        let image = ImageRef::new(image_tag.clone());
        // ACP launches use the agent's ACP entrypoint (e.g. `cline --acp`) and
        // select the persistent piped-stdio container path instead of a PTY, so
        // the JSON-RPC 2.0 framing never passes through a terminal's cooked-mode
        // layer. The guard above has already rejected `Acp` for any agent whose
        // definition lacks an `acp_entrypoint`, so `entrypoint_for_acp` cannot fail
        // here in practice — the `?` is defence-in-depth.
        let acp = matches!(run.launch_mode, LaunchMode::Acp);
        let entrypoint = if acp {
            def.entrypoint_for_acp()?
        } else {
            def.entrypoint_for(run.non_interactive)
        };

        let mut options = vec![
            ContainerOption::AgentIdentity {
                agent: agent.as_str().to_owned(),
                model: run.model.clone(),
            },
            ContainerOption::Image(image),
            ContainerOption::Entrypoint(entrypoint),
            ContainerOption::Interactive(!run.non_interactive),
            ContainerOption::AllowDocker(run.allow_docker),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: session.id().to_string(),
            },
        ];
        if let Some(launch) = builtin_launch {
            options.push(ContainerOption::BuiltinLaunch(launch));
        }

        // Select the persistent piped-stdio (`-i`, no PTY) container path when
        // launching over ACP. Emitted only when actually ACP so ordinary stdio
        // launches keep today's PTY/one-shot-piped behaviour untouched.
        if acp {
            options.push(ContainerOption::Acp(true));
        }
        if run.structured_output && run.non_interactive && !acp && def.output_format.is_structured()
        {
            options.push(ContainerOption::AgentOutputFormat(def.output_format));
        }

        options.push(ContainerOption::ProgramStatus(
            crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
                enabled: session.effective_config().program_status_enabled(),
                answer_queries: false,
            },
        ));

        // Mode flags.
        if let Some(y) = run.yolo {
            options.push(ContainerOption::Yolo(y));
        }
        if let Some(a) = run.auto {
            options.push(ContainerOption::Auto(a));
        }
        if let Some(p) = run.plan {
            options.push(ContainerOption::Plan(p));
        }

        // Tool allow/deny lists.
        if !run.allowed_tools.is_empty() {
            options.push(ContainerOption::AllowedTools(run.allowed_tools.clone()));
            if let Some(flag) = &def.allowed_tools_flag {
                options.push(ContainerOption::AllowedToolsFlag(flag.to_string()));
            }
        }
        if !run.disallowed_tools.is_empty() {
            options.push(ContainerOption::DisallowedTools(
                run.disallowed_tools.clone(),
            ));
            if let Some(flag) = &def.disallowed_tools_flag {
                options.push(ContainerOption::DisallowedToolsFlag(flag.to_string()));
            }
        }

        // Resolve per-agent mode flags into literal argv strings.
        let mode_flags = def.mode_flags(run);
        if !mode_flags.is_empty() {
            options.push(ContainerOption::AgentModeFlags(mode_flags));
        }

        // User-configured extra agent args (`agentFlags` / `--agent-flag`). One
        // that the entrypoint already carries for this launch (codex
        // `--no-daemon` on an interactive launch) is dropped, not doubled; the
        // command layer warns about it (`LaunchPolicy::warn_config`). An ACP
        // entrypoint carries none of them.
        let mut agent_flags = resolve_agent_flags(session, agent, run);
        if !acp {
            let managed = def.entrypoint_managed_flags(run.non_interactive);
            agent_flags.retain(|flag| !managed.contains(flag));
        }
        if !agent_flags.is_empty() {
            options.push(ContainerOption::AgentFlags(agent_flags));
        }

        // Initial prompt. In interactive mode most agents receive it as a
        // trailing positional arg; agents whose bare positional means something
        // else (opencode: a project dir) declare a delivery flag in their definition.
        // Headless delivery follows each CLI's prompt contract: some expect
        // an argv operand, while the others read stdin.
        if let Some(prompt) = run.initial_prompt.as_ref() {
            options.push(ContainerOption::SeededPrompt(prompt.clone()));
            if def.headless_prompt_in_argv {
                options.push(ContainerOption::HeadlessPromptInArgv(true));
            }
            if let InteractiveSeedDelivery::Flag(flag) = &def.interactive_seed_delivery {
                options.push(ContainerOption::InteractiveSeedFlag(flag.to_string()));
            }
        }

        // Model flag.
        if let Some(model) = run.model.as_deref() {
            let flag = def.model_flag_for(model)?;
            options.push(ContainerOption::Model { flag });
        }

        // Env passthrough.
        let env_pass = run.env_passthrough.as_deref().unwrap_or(&[]);
        let auth_env = custom_auth_env_in(session.env(), &def, env_pass);
        let mut emitted = std::collections::HashSet::new();
        for name in env_pass.iter().chain(auth_env.iter()) {
            if !emitted.insert(name) {
                continue;
            }
            options.push(ContainerOption::EnvPassthrough(EnvVar(name.clone())));
        }
        // Secret overrides never enter EnvSnapshot or literal env options.
        // The backend passes only `-e NAME` in argv, with the value in the
        // runtime child environment; an unrequested name is never injected.
        options.extend(
            run.secret_env()
                .into_iter()
                .map(|(key, value)| ContainerOption::EnvOverride { key, value }),
        );

        // Per-agent static env vars, from the one per-agent table.
        for (key, value) in &def.static_env {
            options.push(ContainerOption::EnvLiteral(
                crate::engine::container::options::EnvLiteral {
                    key: key.clone(),
                    value: value.clone(),
                },
            ));
        }

        // Mount the project source into the container's working directory.
        options.extend(self.workspace_mounts(session)?);

        // Overlays — agent settings + user-supplied dirs + skills.
        // Prefer the running image's baked-in `$HOME` (the actual runtime
        // authority) over what the local `Dockerfile.<agent>` says — the
        // two can diverge when the Dockerfile was changed but the image
        // hasn't been rebuilt yet, in which case mounting at the
        // Dockerfile-derived path silently breaks credential passthrough.
        let image_home = self.runtime.image_home_dir(&image_tag).ok().flatten();
        if self.runtime.capabilities().image_acquisition
            == crate::engine::agent_runtime::ImageAcquisition::Import
            && image_home
                .as_deref()
                .is_none_or(|home| !home.starts_with('/'))
        {
            return Err(EngineError::Config(format!(
                "builtin image {image_tag} has no absolute effective HOME: set `ENV HOME=/path` \
                 in its Dockerfile, or give its USER an absolute home directory in /etc/passwd"
            )));
        }
        let container_home = image_home.or_else(|| {
            let home = session.env().home_dir().unwrap_or_default();
            // The definition root is the invoking checkout for session worktrees, so a
            // branch or PR cannot pick the credential mount point via its Dockerfile.
            crate::engine::overlay::detect_container_home(
                &home,
                agent.as_str(),
                session.definition_root(),
            )
        });
        // Private writable mounts are definition facts. The generic mount
        // option owns each tempdir through the execution's guest-exit guard;
        // an OverlayEngine-wide TempDir would be shared across launches.
        for storage in &def.session_storage_dirs {
            let relative = std::path::Path::new(&storage.home_relative_path);
            if storage.home_relative_path.is_empty()
                || !relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
            {
                return Err(EngineError::Config(format!(
                    "invalid session storage path: {}",
                    storage.home_relative_path
                )));
            }
            let guest_path =
                std::path::Path::new(container_home.as_deref().unwrap_or("/root")).join(relative);
            let dir = tempfile::Builder::new()
                .prefix("awman-session-storage-")
                .tempdir()
                .map_err(|e| EngineError::Other(format!("create session storage: {e}")))?;
            options.push(ContainerOption::TemporaryDirectoryMount {
                container_path: guest_path.clone(),
                env_var: storage.env_var.clone(),
                guard: crate::engine::container::options::MountGuard::new_dir(dir),
            });
            if let Some((flag, key)) = &storage.config_override {
                options.push(ContainerOption::ManagedAgentFlags(vec![
                    flag.clone(),
                    format!("{key}={}", guest_path.display()),
                ]));
            }
        }
        let request = OverlayRequest {
            directories: run.directory_overlays.clone(),
            include_all_skills: run.include_all_skills,
            named_skills: run.named_skills.clone(),
            agent: Some(agent.clone()),
            yolo: matches!(run.yolo, Some(YoloMode::Enabled)),
            container_home: container_home.clone(),
            context_overlays: run.context_overlays.clone(),
            materialize_credentials: false,
            definition: Some(def.definition()),
        };
        let mut request = request;
        request.materialize_credentials =
            matches!(&credentials.delivery, CredentialDelivery::File(_));
        let (overlays, staged_credentials) = self.overlay_engine.build_scoped_overlays(
            session,
            &request,
            self.lease_workspace_scope.as_deref(),
            &run.command_mounts,
        )?;
        let sanctioned_mounts = overlays.clone();
        for spec in overlays {
            options.push(ContainerOption::Overlay(spec));
        }
        for staged in staged_credentials {
            let Some(spec) = crate::engine::auth::keychain::refreshable_spec_for(&staged.agent)
            else {
                continue;
            };
            options.push(ContainerOption::RefreshableCredential(
                RefreshableCredentialDelivery {
                    agent: staged.agent,
                    spec_agent: spec.agent,
                    credential_env_key: spec.credential_env_key,
                    staged_path: staged.path,
                    staged_root: staged.root,
                    initial_fingerprint: staged.fingerprint,
                },
            ));
        }
        // Who leases those credentials, carried explicitly (F-38). Emitted
        // unconditionally when the engine has a factory so the backends never
        // have to consult process-global state to find one.
        if let Some(factory) = self.lease_factory.as_ref() {
            options.push(ContainerOption::CredentialLeaseFactory(factory.clone()));
        }

        // System prompt delivery (context overlays).
        if let Some(ref prompt_text) = run.system_prompt {
            self.emit_system_prompt_options(
                &def,
                prompt_text,
                &run.context_overlays,
                &mut options,
            )?;
        }

        if let Some(body) = session.inline_work_item() {
            options.push(ContainerOption::Overlay(
                crate::engine::container::options::OverlaySpec {
                    host_path: body.to_path_buf(),
                    container_path: "/awman/work-item.md".into(),
                    permission: crate::engine::container::options::OverlayPermission::ReadOnly,
                },
            ));
        }
        // Default working dir for the agent container.
        options.push(ContainerOption::WorkingDir(std::path::PathBuf::from(
            "/workspace",
        )));

        if self.read_only_workspace || self.lease_workspace_scope.is_some() {
            validate_workspace_mounts(
                &mut options,
                session.git_root(),
                self.read_only_workspace,
                self.lease_workspace_scope.as_deref(),
                session.inline_work_item(),
                &sanctioned_mounts,
            )?;
        }
        Ok(options)
    }

    /// Cross-paradigm option builder. Maps a single `AgentRunOptions` (the same
    /// flag/config inputs the container path consumes) into the
    /// `ResolvedAgentOptions` variant matching the active runtime's paradigm,
    /// branching on `AgentRuntimeEngine::capabilities()` rather than on concrete
    /// runtime types. Resolved credential env-vars are folded in as the
    /// paradigm-appropriate option so each command no longer hand-builds its own
    /// option list.
    ///
    /// - container-class runtimes → `ResolvedAgentOptions::Container`
    /// - sandbox-class (kit-declarative) runtimes → `ResolvedAgentOptions::Sandbox`
    ///
    /// Credential **values** only ever reach the runtime as `AgentCredentials`;
    /// for the sandbox paradigm they are routed to `sbx secret set` by the dsbx
    /// driver and never written to `session.json`.
    pub fn resolve_agent_options(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
        credentials: &AgentCredentials,
    ) -> Result<ResolvedAgentOptions, EngineError> {
        // The runtime is the engine's own (F-40b): a caller passing a
        // *different* one than the engine was built with could resolve
        // sandbox options and then build them on a container runtime.
        if self.runtime.capabilities().kit_declarative {
            if !run.secret_env().is_empty() {
                return Err(EngineError::Config(
                    "command secrets require a container runtime".into(),
                ));
            }
            let mut options = self.build_sandbox_options(session, agent, run)?;
            if !credentials.env_vars.is_empty() {
                options.push(SandboxOption::AgentCredentials {
                    env_vars: credentials.env_vars.clone(),
                });
            }
            let mut resolved =
                crate::engine::sandbox::options::ResolvedSandboxOptions::resolve(options);
            resolved.host_env = Some(session.env().clone());
            Ok(ResolvedAgentOptions::Sandbox(Box::new(resolved)))
        } else {
            let mut options =
                self.build_options_with_credentials(session, agent, run, credentials)?;
            if matches!(&credentials.delivery, CredentialDelivery::Env)
                && !credentials.env_vars.is_empty()
            {
                options.push(ContainerOption::AgentCredentials {
                    env_vars: credentials.env_vars.clone(),
                });
            }
            Ok(ResolvedAgentOptions::Container(Box::new(
                crate::engine::container::options::ResolvedContainerOptions::resolve_in(
                    options,
                    session.env(),
                )?,
            )))
        }
    }

    /// Build the `SandboxOption` list for launching an agent under a
    /// sandbox-class runtime. Mirrors `build_options` (same agent definition, same
    /// up-front validation) but emits sandbox-paradigm options: a kit selector
    /// instead of an image ref, a workspace dir instead of arbitrary mounts, and
    /// system prompts delivered inline via `session.json` rather than via
    /// host-file mounts (sandbox VMs see only the workspace).
    pub fn build_sandbox_options(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
    ) -> Result<Vec<SandboxOption>, EngineError> {
        if self.read_only_workspace {
            return Err(read_only_unsupported(self.runtime.display_name()));
        }
        let def = self.resolve_definition(session, agent)?;

        // Parity validation with the container path so a sandbox-configured
        // user gets the same up-front errors for unsupported mode
        // combinations. The one asymmetry — ACP is a container-paradigm-only
        // launch mode, so a sandbox launch that clears the `supports_acp`
        // check is still refused as `NotImplemented` (WI 0089) rather than
        // silently launching in the wrong (stdio) mode — lives in
        // `validate_run` behind the `RunParadigm` it is given.
        let yolo_run = with_yolo_disallowed_tools(session, &def, run, RunParadigm::Sandbox)?;
        let run: &AgentRunOptions = &yolo_run;
        def.validate_run(run, RunParadigm::Sandbox)?;

        let mut options = vec![
            SandboxOption::AgentId(agent.as_str().to_string()),
            SandboxOption::WorkspaceDir(session.git_root().to_path_buf()),
            SandboxOption::Interactive(!run.non_interactive),
            SandboxOption::ProgramStatus(
                crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
                    enabled: session.effective_config().program_status_enabled(),
                    answer_queries: false,
                },
            ),
        ];

        // Seeded prompt — recorded in session.json exactly once. For `kind: agent`
        // kits the dsbx launcher appends it positionally; for `kind: mixin` kits
        // the apply script renders it from session.json. The driver never does
        // both, so the prompt is delivered exactly once.
        if let Some(prompt) = run.initial_prompt.as_ref() {
            options.push(SandboxOption::SeededPrompt(prompt.clone()));
        }

        // Model.
        if let Some(model) = run.model.as_deref() {
            let flag = def.model_flag_for(model)?;
            options.push(SandboxOption::Model { flag });
            // An agent whose built-in template has no mixin-safe model config
            // (copilot; the in-VM apply script also warns) declares the note
            // in its definition, so the flag does not vanish into the VM boot log.
            if let Some(note) = &def.sandbox_model_note {
                options.push(SandboxOption::UnsupportedNote(note.clone()));
            }
        }

        // Mode flags (plan/yolo/auto). `kind: agent` kits accept agent args
        // after the `--` delimiter, so they get the same literal argv flags
        // the container path emits. Mixin kits launch through Docker's
        // built-in template (no argv channel): claude has a native settings
        // equivalent (`permissions.defaultMode`, rendered by its apply
        // script from `session.json`); the other mixins have none awman
        // manages, so the request is warned about rather than dropped.
        let mode_flags = def.mode_flags(run);
        if !mode_flags.is_empty() {
            match def.sbx_kit_kind {
                SbxKitKind::Agent => {
                    options.push(SandboxOption::AgentModeFlags(mode_flags));
                }
                SbxKitKind::Mixin => {
                    let mode = if matches!(run.yolo, Some(YoloMode::Enabled)) {
                        "yolo"
                    } else if matches!(run.plan, Some(PlanMode::Enabled)) {
                        "plan"
                    } else {
                        "auto"
                    };
                    if def.sandbox_permission_mode_supported {
                        options.push(SandboxOption::PermissionMode(mode.to_string()));
                    } else {
                        options.push(SandboxOption::UnsupportedNote(format!(
                            "--{mode} cannot be delivered to '{agent}' under the sandbox \
                             runtime (the built-in template's launch command takes no \
                             flags and the agent has no settings-file equivalent awman \
                             manages); continuing without it",
                            agent = agent.as_str(),
                        )));
                    }
                }
            }
        }

        // User-configured extra agent args. Only `kind: agent` kits have an
        // argv channel; a mixin kit's launch command belongs to Docker's
        // built-in template, so the flags are warned about, not dropped.
        let mut agent_flags = resolve_agent_flags(session, agent, run);
        if matches!(def.sbx_kit_kind, SbxKitKind::Agent) {
            let managed = def.entrypoint_managed_flags(run.non_interactive);
            agent_flags.retain(|flag| !managed.contains(flag));
        }
        if !agent_flags.is_empty() {
            match def.sbx_kit_kind {
                SbxKitKind::Agent => {
                    options.push(SandboxOption::AgentFlags(agent_flags));
                }
                SbxKitKind::Mixin => {
                    options.push(SandboxOption::UnsupportedNote(format!(
                        "agentFlags ({}) cannot be delivered to '{agent}' under the sandbox \
                         runtime (the built-in template owns the launch command); continuing \
                         without them",
                        agent_flags.join(" "),
                        agent = agent.as_str(),
                    )));
                }
            }
        }

        if run.structured_output && run.non_interactive && def.output_format.is_structured() {
            options.push(SandboxOption::UnsupportedNote(format!(
                "structured output ({}) cannot be delivered under the sandbox runtime; continuing with text output",
                def.output_format,
            )));
        }

        // Headless shape (`--print` / `exec` / `run` / `task`): the
        // dsbx driver appends it as an agent arg for `kind: agent` kits, but
        // mixin kits launch through the built-in template's interactive
        // entrypoint — the prompt still arrives via piped stdin, the agent's
        // own headless mode just cannot be enabled. Warn, don't drop
        // silently.
        if run.non_interactive && matches!(def.sbx_kit_kind, SbxKitKind::Mixin) {
            if let Some(flag) = &def.non_interactive_flag {
                options.push(SandboxOption::UnsupportedNote(format!(
                    "headless mode ('{flag}') cannot be enabled for '{agent}' \
                     under the sandbox runtime (the built-in template owns the launch \
                     command); the prompt is piped to the agent's interactive \
                     entrypoint instead",
                    agent = agent.as_str(),
                )));
            }
        }

        // Tool allow/deny lists.
        if !run.allowed_tools.is_empty() {
            options.push(SandboxOption::AllowedTools(run.allowed_tools.clone()));
        }
        if !run.disallowed_tools.is_empty() {
            options.push(SandboxOption::DisallowedTools(run.disallowed_tools.clone()));
        }

        // Env passthrough. Credential-class names are filtered out of the
        // workspace-readable session.json by `DSbxSessionConfig`; non-sensitive
        // names reach the apply script.
        let env_pass = run.env_passthrough.as_deref().unwrap_or(&[]);
        for name in env_pass {
            options.push(SandboxOption::EnvPassthrough(EnvVar(name.clone())));
        }

        // Per-agent static env vars (mirrors the container path).
        for (key, value) in &def.static_env {
            options.push(SandboxOption::EnvLiteral(EnvLiteral {
                key: key.clone(),
                value: value.clone(),
            }));
        }

        // Every sandbox has a private DinD daemon, so the host-socket mount
        // `--allow-docker` requests is meaningless here (same trace as ready).
        if run.allow_docker {
            tracing::debug!("--allow-docker is a no-op under sbx (private DinD is always on)");
        }

        // Host-directory mounts cannot be honored: the VM sees only the
        // virtiofs-mounted workspace. Record each requested-but-unmountable
        // feature as a note; `run_interactive` surfaces them as warnings.
        for dir in run.directory_overlays.iter().chain(&run.command_mounts) {
            options.push(SandboxOption::UnsupportedNote(format!(
                "directory overlay '{}' cannot be mounted into the sandbox VM \
                 (only the workspace is visible); continuing without it",
                dir.host
            )));
        }
        if run.include_all_skills || !run.named_skills.is_empty() {
            options.push(SandboxOption::UnsupportedNote(
                "skill mounts (--include-all-skills / skill(...) overlays) are not \
                 supported under the sandbox runtime; continuing without them"
                    .into(),
            ));
        }
        if !run.context_overlays.is_empty() {
            options.push(SandboxOption::UnsupportedNote(
                "context directories cannot be mounted into the sandbox VM; the \
                 rendered system prompt text is still delivered, but /awman/context \
                 paths will not exist inside the sandbox"
                    .into(),
            ));
        }

        // System prompt — delivered inline via session.json so no host-file
        // mount is required (the sandbox VM can only see the workspace).
        if let Some(ref prompt_text) = run.system_prompt {
            emit_sandbox_system_prompt_options(&def, prompt_text, &mut options);
        }

        Ok(options)
    }

    /// Emit the `ContainerOption` variants for system-prompt delivery, consulting
    /// the agent definition for the correct delivery mechanism.
    fn emit_system_prompt_options(
        &self,
        def: &AgentDefinition,
        prompt_text: &str,
        context_overlays: &[ContextOverlay],
        options: &mut Vec<ContainerOption>,
    ) -> Result<(), EngineError> {
        match &def.system_prompt_delivery {
            SystemPromptMode::Append => {
                let flag = def
                    .system_prompt_flag
                    .as_deref()
                    .unwrap_or("--append-system-prompt-file");
                let (guard, container_path) = write_prompt_temp_file(prompt_text)?;
                options.push(ContainerOption::SystemPromptFile {
                    host_path: guard.host_path().to_path_buf(),
                    container_path,
                    flag: flag.to_string(),
                    guard,
                });
            }
            SystemPromptMode::AppendInline { key } => {
                let flag = def.system_prompt_flag.as_deref().unwrap_or("--config");
                options.push(ContainerOption::SystemPromptInline {
                    flag: flag.to_string(),
                    text: format!("{key}={prompt_text}"),
                });
            }
            SystemPromptMode::Replace => {
                let flag = def.system_prompt_flag.as_deref().unwrap_or("--system");
                let preamble = format!(
                    "You are {}, an AI coding assistant. Use your full capabilities \
                     and all available tools to complete the task.\n\n",
                    def.name
                );
                options.push(ContainerOption::SystemPromptInline {
                    flag: flag.to_string(),
                    text: format!("{preamble}{prompt_text}"),
                });
            }
            SystemPromptMode::AgentsMd => {
                for ctx in context_overlays {
                    plant_agents_md(&ctx.host_path, prompt_text);
                }
            }
            SystemPromptMode::EnvFile { var } => {
                let (guard, container_path) = write_prompt_temp_file(prompt_text)?;
                options.push(ContainerOption::SystemPromptEnvFile {
                    env_var: var.to_string(),
                    host_path: guard.host_path().to_path_buf(),
                    container_path,
                    guard,
                });
            }
            SystemPromptMode::EnvDir { var } => {
                let (guard, container_path) = write_prompt_temp_dir(prompt_text)?;
                options.push(ContainerOption::SystemPromptEnvFile {
                    env_var: var.to_string(),
                    host_path: guard.host_path().to_path_buf(),
                    container_path,
                    guard,
                });
            }
            SystemPromptMode::AddDir { flag } => {
                for ctx in context_overlays {
                    plant_agents_md(&ctx.host_path, prompt_text);
                    options.push(ContainerOption::AgentAddDir {
                        flag: flag.to_string(),
                        container_path: ctx.container_path.clone(),
                    });
                }
            }
            SystemPromptMode::Unsupported => {
                // The user-facing warning is emitted in `resolve_context_overlays`
                // (it has access to a UserMessageSink); here we just skip emitting
                // any system-prompt option.
            }
        }
        Ok(())
    }
}

/// Write the system prompt text to a temp file and return its guard and the
/// path it is mounted at. The guard travels with the launch's options, so the
/// file lives as long as that launch and no longer.
fn write_prompt_temp_file(
    text: &str,
) -> Result<
    (
        crate::engine::container::options::PromptFileGuard,
        std::path::PathBuf,
    ),
    EngineError,
> {
    use std::io::Write as _;
    let mut tmp = tempfile::Builder::new()
        .prefix("awman-ctx-prompt-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| EngineError::io(std::path::PathBuf::from("/tmp"), e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| EngineError::io(tmp.path(), e))?;
    let container_path =
        std::path::PathBuf::from("/tmp").join(tmp.path().file_name().unwrap_or_default());
    Ok((
        crate::engine::container::options::PromptFileGuard::new(tmp),
        container_path,
    ))
}

fn write_prompt_temp_dir(
    text: &str,
) -> Result<
    (
        crate::engine::container::options::PromptFileGuard,
        std::path::PathBuf,
    ),
    EngineError,
> {
    let dir = tempfile::Builder::new()
        .prefix("awman-ctx-instructions-")
        .tempdir()
        .map_err(|e| EngineError::io(std::path::PathBuf::from("/tmp"), e))?;
    let file = dir.path().join("AGENTS.md");
    std::fs::write(&file, text).map_err(|e| EngineError::io(&file, e))?;
    let container_path =
        std::path::PathBuf::from("/tmp").join(dir.path().file_name().unwrap_or_default());
    Ok((
        crate::engine::container::options::PromptFileGuard::new_dir(dir),
        container_path,
    ))
}

/// Emit the `SandboxOption` for system-prompt delivery. Unlike the container
/// path — which can mount a host file or set an env-file path — the sandbox VM
/// only sees the workspace, so every text-bearing delivery mode is collapsed to
/// `SystemPromptInline`: the prompt text travels in `session.json` and the
/// mixin apply script renders it into the agent's native config. `AddDir`
/// (extra mounted dirs) cannot be expressed and records an `UnsupportedNote`;
/// `Unsupported` stays a documented no-op (warned upstream).
fn emit_sandbox_system_prompt_options(
    def: &AgentDefinition,
    prompt_text: &str,
    options: &mut Vec<SandboxOption>,
) {
    match &def.system_prompt_delivery {
        SystemPromptMode::Append => {
            options.push(SandboxOption::SystemPromptInline {
                flag: def
                    .system_prompt_flag
                    .as_deref()
                    .unwrap_or("--append-system-prompt")
                    .to_string(),
                text: prompt_text.to_string(),
            });
        }
        SystemPromptMode::AppendInline { key } => {
            options.push(SandboxOption::SystemPromptInline {
                flag: def
                    .system_prompt_flag
                    .as_deref()
                    .unwrap_or("--config")
                    .to_string(),
                text: format!("{key}={prompt_text}"),
            });
        }
        SystemPromptMode::Replace => {
            let preamble = format!(
                "You are {}, an AI coding assistant. Use your full capabilities \
                 and all available tools to complete the task.\n\n",
                def.name
            );
            options.push(SandboxOption::SystemPromptInline {
                flag: def
                    .system_prompt_flag
                    .as_deref()
                    .unwrap_or("--system")
                    .to_string(),
                text: format!("{preamble}{prompt_text}"),
            });
        }
        SystemPromptMode::EnvFile { var } | SystemPromptMode::EnvDir { var } => {
            // The mixin apply script materializes the text into a VM-local file
            // and points `var` at it; the flag slot carries the env-var name.
            options.push(SandboxOption::SystemPromptInline {
                flag: var.to_string(),
                text: prompt_text.to_string(),
            });
        }
        SystemPromptMode::AgentsMd => {
            // No CLI flag exists for this mode; the mixin apply script reads
            // the inline text from session.json and stages it in the VM.
            options.push(SandboxOption::SystemPromptInline {
                flag: String::new(),
                text: prompt_text.to_string(),
            });
        }
        SystemPromptMode::AddDir { .. } => {
            // AddDir plants AGENTS.md into extra mounted directories, which
            // do not exist under the sandbox runtime — warn, don't drop.
            options.push(SandboxOption::UnsupportedNote(format!(
                "agent '{}' delivers system prompts via extra directory mounts, \
                 which are unavailable under the sandbox runtime; the system \
                 prompt was not applied",
                def.name
            )));
        }
        SystemPromptMode::Unsupported => {
            // Documented no-op — the user-facing warning is emitted by
            // `resolve_context_overlays`, same as the container path.
        }
    }
}

/// Write `AGENTS.md` into a context dir, skipping the write when the existing
/// content already matches. Logs (and silently moves on) when the write fails
/// e.g. because the host directory is read-only.
fn plant_agents_md(host_dir: &std::path::Path, prompt_text: &str) {
    let agents_md = host_dir.join("AGENTS.md");
    if let Ok(existing) = std::fs::read_to_string(&agents_md) {
        if existing == prompt_text {
            return;
        }
    }
    if let Err(e) = std::fs::write(&agents_md, prompt_text) {
        tracing::warn!(
            path = %agents_md.display(),
            error = %e,
            "context overlay: failed to plant AGENTS.md (host dir read-only?); \
             agent will not be automatically notified about the context directory"
        );
    }
}
