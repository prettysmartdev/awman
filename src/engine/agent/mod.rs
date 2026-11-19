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
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{AgentSetupStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::data::workspace::{WorkspaceAccess, WorkspaceMountSpec};
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::auth::{AgentCredentials, CredentialDelivery, RefreshableCredentialDelivery};
use crate::engine::container::options::{
    ContainerOption, EnvLiteral, EnvVar, ImageRef, PlanMode, YoloMode,
};
use crate::engine::error::EngineError;
use crate::engine::overlay::{ContextOverlay, DirectorySpec, OverlayEngine, OverlayRequest};
use crate::engine::sandbox::options::SandboxOption;
use crate::engine::worker::lease_workspace::{read_only_unsupported, validate_workspace_mounts};

pub mod automation;
pub mod custom;
pub mod definition;
pub mod download;
pub mod first_party;
pub mod frontend;
pub mod image_freshness;
pub mod opencode_plan_guard;
pub mod profile_state;
pub mod registry;
mod seeded_prompt;
pub mod session_seed;

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

    /// Bytes provisioning would leave on disk, without writing or downloading.
    /// A missing download recipe remains unknown until preparation runs.
    pub fn effective_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
        if self.action == DockerfileSync::Written {
            return Ok(self.rendered.as_ref().map(|text| text.as_bytes().to_vec()));
        }
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(EngineError::io(path, e)),
        }
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
            download::publish_in_repo(paths.git_root(), &path, content.as_bytes())?;
        } else {
            download::download_agent_dockerfile(&def.name, paths.git_root(), &path, project_tag)
                .await?;
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
    pub native_session: Option<crate::data::settings_profile::NativeSessionId>,
    pub profile_state: Option<profile_state::ProfileRunState>,
    /// WI 0070: the execution-private native lifecycle channel mounted at
    /// `/awman/automation` for an interactive automation launch. Container
    /// runtimes only; a sandbox runtime launches without it (native
    /// facility then stays unknown, never claimed).
    pub automation_channel: Option<crate::engine::agent::automation::observer::ChannelMount>,
    /// Caller values are used only for explicit env overlay names.
    pub command_secrets: Option<Arc<crate::data::secret::CommandSecrets>>,
    /// Explicit caller authorization, independent of unattended defaults.
    pub yolo_authorized: bool,
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

/// The container options that decide an agent's argv for one launch:
/// entrypoint (with the headless form, `headless_only_flags`,
/// `headless_text_flags` or the interactive model fallback), PTY/ACP
/// selection, structured output, permission modes, tool lists, seeded prompt
/// and model. Pure (no session, no I/O), so the per-agent argv tables are
/// pinned by unit tests against exactly what `build_options` emits. Callers
/// run [`AgentDefinition::validate_run`] first.
pub(crate) fn launch_argv_options(
    def: &AgentDefinition,
    run: &AgentRunOptions,
) -> Result<Vec<ContainerOption>, EngineError> {
    let acp = matches!(run.launch_mode, LaunchMode::Acp);
    let entrypoint = if acp {
        def.entrypoint_for_acp()?
    } else {
        def.entrypoint_for_run(run)
    };
    let mut options = vec![
        ContainerOption::Entrypoint(entrypoint),
        ContainerOption::Interactive(!run.non_interactive),
    ];
    // Select the persistent piped-stdio (`-i`, no PTY) container path when
    // launching over ACP. Emitted only when actually ACP so ordinary stdio
    // launches keep today's PTY/one-shot-piped behaviour untouched.
    if acp {
        options.push(ContainerOption::Acp(true));
    }
    // Exactly one of `headless_text_flags` (in the entrypoint) and the
    // structured format's flags (rendered at resolve) is ever applied.
    if def.uses_structured_output(run) {
        options.push(ContainerOption::AgentOutputFormat(def.output_format));
    }

    if let Some((tokens, position)) = def.native_resume_tokens(run)? {
        options.push(ContainerOption::NativeResume { tokens, position });
    }

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

    // Resolve per-agent mode flags into literal argv strings. The plan part
    // is this route's native plan delivery (WI 0070).
    let mode_flags = def.mode_flags(run, RunParadigm::Container)?;
    if !mode_flags.is_empty() {
        options.push(ContainerOption::AgentModeFlags(mode_flags));
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
            if let (true, Some(flag)) = (run.non_interactive, &def.headless_prompt_flag) {
                options.push(ContainerOption::HeadlessPromptFlag(flag.clone()));
            }
        }
        if let InteractiveSeedDelivery::Flag(flag) = &def.interactive_seed_delivery {
            options.push(ContainerOption::InteractiveSeedFlag(flag.to_string()));
        }
    }

    // Model flag. With an interactive model fallback the subcommand is part
    // of the entrypoint, so the flag lands after it.
    if let Some(model) = run.model.as_deref() {
        let flag = def.model_flag_for(model)?;
        options.push(ContainerOption::Model { flag });
    }
    Ok(options)
}

/// The execution-private host directory and inner path that hold
/// `relative` (HOME-relative), from a launch's seeded session storage.
fn private_dir<'a>(
    private_dirs: &'a [(String, std::path::PathBuf)],
    relative: &'a Path,
) -> Option<(&'a Path, &'a Path)> {
    private_dirs
        .iter()
        .find(|(base, _)| relative.starts_with(base))
        .map(|(base, dir)| {
            (
                dir.as_path(),
                relative.strip_prefix(base).unwrap_or(relative),
            )
        })
}

/// HOME-relative directory of OpenCode's global configuration.
const OPENCODE_GLOBAL_CONFIG: &str = ".config/opencode";

/// The host directory a container launch delivers as OpenCode's global
/// configuration: the execution-private seeded copy (v2) or the host
/// directory (1.x).
fn plan_global_config_dir(
    session: &Session,
    def: &AgentDefinition,
    private_dirs: &[(String, std::path::PathBuf)],
) -> Option<std::path::PathBuf> {
    match def.plan_config_guard {
        definition::PlanConfigGuard::None => None,
        definition::PlanConfigGuard::OpenCodeV2 => {
            private_dir(private_dirs, Path::new(OPENCODE_GLOBAL_CONFIG))
                .map(|(dir, inner)| dir.join(inner))
        }
        definition::PlanConfigGuard::OpenCodeV1 => {
            private_dir(private_dirs, Path::new(OPENCODE_GLOBAL_CONFIG))
                .map(|(dir, inner)| dir.join(inner))
                .or_else(|| {
                    session
                        .env()
                        .home_dir()
                        .map(|home| home.join(OPENCODE_GLOBAL_CONFIG))
                })
        }
    }
}

/// The definition's Plan-profile guard over the configuration a launch
/// delivers (WI 0070): `global` is the global directory the guest is given
/// (`None` when it is given none, e.g. a sandbox VM), the workspace is
/// `session.git_root()`, and the environment is exactly what the launch
/// passes. Shared by the container and sandbox builders. A no-op for a
/// definition without a guard.
fn verify_plan_config(
    session: &Session,
    def: &AgentDefinition,
    run: &AgentRunOptions,
    env_pass: &[String],
    global: Option<&Path>,
    settings_delivery: bool,
) -> Result<(), EngineError> {
    if def.plan_config_guard == definition::PlanConfigGuard::None {
        return Ok(());
    }
    // Only names the guest really receives: a passthrough name unset on
    // the host is not passed.
    let mut guest_env: Vec<(String, Option<String>)> = env_pass
        .iter()
        .filter_map(|name| {
            session
                .env()
                .get(name)
                .map(|value| (name.clone(), Some(value.to_string())))
        })
        .collect();
    guest_env.extend(
        run.secret_env()
            .into_iter()
            .map(|(name, value)| (name, Some(value.expose().to_string()))),
    );
    guest_env.extend(
        def.static_env
            .iter()
            .map(|(name, value)| (name.clone(), Some(value.clone()))),
    );
    let sources = opencode_plan_guard::collect_opencode_sources(
        &def.name,
        def.plan_config_guard,
        global,
        session.git_root(),
        &guest_env,
    )?;
    for notice in opencode_plan_guard::check_opencode_plan(
        def.plan_config_guard,
        &def.name,
        &sources,
        settings_delivery,
    )? {
        tracing::info!(agent = %def.name, "--plan: {notice}");
    }
    Ok(())
}

/// The Plan-profile guard and private settings patch of a `--plan` launch
/// (WI 0070). `private_dirs` maps each `session_storage_dirs` entry's
/// HOME-relative path to its execution-private host directory, already
/// seeded. A no-op when `--plan` is not requested.
fn prepare_native_plan(
    session: &Session,
    def: &AgentDefinition,
    run: &AgentRunOptions,
    env_pass: &[String],
    private_dirs: &[(String, std::path::PathBuf)],
) -> Result<(), EngineError> {
    let Some(delivery) = def.native_plan_delivery(run, RunParadigm::Container)? else {
        return Ok(());
    };
    verify_plan_config(
        session,
        def,
        run,
        env_pass,
        plan_global_config_dir(session, def, private_dirs).as_deref(),
        delivery.settings.is_some(),
    )?;
    if let Some(settings) = &delivery.settings {
        let relative = Path::new(&settings.home_relative_file);
        let (dir, inner) = private_dir(private_dirs, relative).ok_or_else(|| {
            EngineError::Config(format!(
                "agent '{}' declares a plan settings file outside its private session storage: {}",
                def.name, settings.home_relative_file
            ))
        })?;
        let jsonc = inner.with_extension("jsonc");
        if inner.extension().is_some_and(|e| e == "json")
            && dir.join(&jsonc).symlink_metadata().is_ok()
        {
            crate::engine::overlay::apply_native_settings_patch(dir, &jsonc, &settings.patch)?;
        }
        for notice in
            crate::engine::overlay::apply_native_settings_patch(dir, inner, &settings.patch)?
        {
            tracing::info!(agent = %def.name, "--plan: {notice}");
        }
    }
    Ok(())
}

/// Lexical host path, resolved through symlinks when it exists, so two
/// spellings of one directory compare equal.
fn comparable_host_path(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Prove that every OpenCode configuration file the guest can read for a
/// `--plan` launch is the host file the Plan-profile guard inspected
/// (WI 0070). For each protected guest file, the most specific mount that
/// covers it (equal path or an ancestor directory) must map it to the
/// inspected host file; a tmpfs, a different host source, or a file mount
/// replacing it is refused. Runs on the final option list, so it covers user
/// overlays, settings, skills, context, private storage and system prompt
/// mounts alike, with or without a read-only workspace.
fn verify_plan_config_mounts(
    session: &Session,
    def: &AgentDefinition,
    run: &AgentRunOptions,
    container_home: &Path,
    private_dirs: &[(String, std::path::PathBuf)],
    options: &[ContainerOption],
) -> Result<(), EngineError> {
    if def.plan_config_guard == definition::PlanConfigGuard::None
        || def
            .native_plan_delivery(run, RunParadigm::Container)?
            .is_none()
    {
        return Ok(());
    }
    // (guest file, host file the guard read or would have read).
    let mut protected: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let workspace = Path::new(crate::data::workspace::WORKSPACE_GUEST_PATH);
    let git_root = comparable_host_path(session.git_root());
    for file in opencode_plan_guard::project_config_files() {
        protected.push((workspace.join(&file), git_root.join(&file)));
    }
    if let Some(global) = plan_global_config_dir(session, def, private_dirs) {
        let global = comparable_host_path(&global);
        let guest = container_home.join(OPENCODE_GLOBAL_CONFIG);
        for file in opencode_plan_guard::config_dir_files() {
            protected.push((guest.join(&file), global.join(&file)));
        }
    }
    // (guest path, host source; `None` for memory-backed mounts).
    let mut mounts: Vec<(&Path, Option<&Path>)> = Vec::new();
    for option in options {
        match option {
            ContainerOption::Overlay(spec) => {
                mounts.push((&spec.container_path, Some(&spec.host_path)))
            }
            ContainerOption::AgentSettingsPassthrough(settings) => {
                mounts.extend(settings.overlays.iter().map(|spec| {
                    (
                        spec.container_path.as_path(),
                        Some(spec.host_path.as_path()),
                    )
                }))
            }
            ContainerOption::Tmpfs(path) => mounts.push((path, None)),
            ContainerOption::TemporaryDirectoryMount {
                container_path,
                guard,
                ..
            } => mounts.push((container_path, Some(guard.host_path()))),
            ContainerOption::TemporaryFileMount {
                host_path,
                container_path,
                ..
            }
            | ContainerOption::SystemPromptFile {
                host_path,
                container_path,
                ..
            }
            | ContainerOption::SystemPromptEnvFile {
                host_path,
                container_path,
                ..
            } => mounts.push((container_path, Some(host_path))),
            _ => {}
        }
    }
    for (guest, inspected) in &protected {
        let Some(depth) = mounts
            .iter()
            .filter(|(target, _)| guest.starts_with(target))
            .map(|(target, _)| target.components().count())
            .max()
        else {
            // Not mounted: the guest sees the image's own (absent) file.
            continue;
        };
        for (target, source) in mounts
            .iter()
            .filter(|(target, _)| guest.starts_with(target) && target.components().count() == depth)
        {
            let delivered = source.map(|source| {
                comparable_host_path(source).join(guest.strip_prefix(target).unwrap_or(guest))
            });
            if delivered.as_deref() != Some(inspected.as_path()) {
                return Err(EngineError::AgentModeConflict {
                    agent: def.name.clone(),
                    selector: format!("mount at {}", target.display()),
                    origin: "overlay",
                    reason: format!(
                        "it replaces {} with configuration awman did not verify for --plan; \
                         remove the mount or launch without --plan",
                        guest.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Where a run's agent flags come from, for an `AgentModeConflict`.
fn agent_flags_origin(run: &AgentRunOptions) -> &'static str {
    if run.agent_flags.is_some() {
        "--agent-flag"
    } else {
        "agents.flags"
    }
}

/// The env passthrough list of a launch, without the definition's
/// `plan_env_drop` names when `--plan` is requested. A command secret with
/// one of those names is an explicit request, so it is refused.
fn plan_env_passthrough(
    def: &AgentDefinition,
    run: &AgentRunOptions,
) -> Result<Vec<String>, EngineError> {
    let names = run.env_passthrough.clone().unwrap_or_default();
    if !matches!(run.plan, Some(PlanMode::Enabled)) || def.plan_env_drop.is_empty() {
        return Ok(names);
    }
    if let Some((name, _)) = run
        .secret_env()
        .into_iter()
        .find(|(name, _)| def.plan_env_drop.contains(name))
    {
        return Err(EngineError::AgentModeConflict {
            agent: def.name.clone(),
            selector: name,
            origin: "environment",
            reason: "it selects the agent's permissions and would override the native plan \
                     mode; do not pass it with --plan"
                .into(),
        });
    }
    Ok(names
        .into_iter()
        .filter(|name| !def.plan_env_drop.contains(name))
        .collect())
}

/// The names `plan_env_passthrough` removes from this run, for the launch
/// notice (`LaunchPolicy::launch_notices`).
pub fn plan_env_dropped(def: &AgentDefinition, run: &AgentRunOptions) -> Vec<String> {
    if !matches!(run.plan, Some(PlanMode::Enabled)) {
        return Vec::new();
    }
    run.env_passthrough
        .iter()
        .flatten()
        .filter(|name| def.plan_env_drop.contains(name))
        .cloned()
        .collect()
}

pub fn resolve_agent_flags(
    session: &Session,
    agent: &AgentName,
    run: &AgentRunOptions,
) -> Vec<String> {
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

fn emit_read_only_workspace_env(
    def: &AgentDefinition,
    container_home: &str,
    read_only: bool,
    options: &mut Vec<ContainerOption>,
) -> Result<(), EngineError> {
    if !read_only {
        return Ok(());
    }
    if !crate::data::workspace::is_normalized_guest_path(Path::new(container_home))
        || Path::new(container_home).starts_with(crate::data::workspace::WORKSPACE_GUEST_PATH)
    {
        return Err(EngineError::Config(
            "read-only workspace requires an absolute writable agent home outside /workspace"
                .into(),
        ));
    }
    let mut redirects = vec![
        (
            "XDG_CACHE_HOME".to_owned(),
            format!("{}/.cache", container_home.trim_end_matches('/')),
        ),
        (
            "XDG_STATE_HOME".to_owned(),
            format!("{}/.local/state", container_home.trim_end_matches('/')),
        ),
        (
            "TMPDIR".to_owned(),
            crate::data::workspace::SCRATCH_TMPFS_PATH.into(),
        ),
    ];
    for redirect in &def.read_only_workspace_redirects {
        if redirect.home_relative_path.is_empty()
            || redirect.home_relative_path.contains(['\\', ':', ',', '\0'])
            || redirect
                .home_relative_path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(EngineError::Config(format!(
                "invalid read-only workspace redirect path: {}",
                redirect.home_relative_path
            )));
        }
        redirects.push((
            redirect.env_var.clone(),
            format!(
                "{}/{}",
                container_home.trim_end_matches('/'),
                redirect.home_relative_path
            ),
        ));
    }
    for (key, path) in redirects {
        // Explicit env() overlays and per-launch secrets take precedence.
        if !options.iter().any(|option| match option {
            ContainerOption::EnvPassthrough(var) => var.0 == key,
            ContainerOption::EnvLiteral(var) => var.key == key,
            ContainerOption::EnvOverride { key: name, .. } => name == &key,
            _ => false,
        }) {
            options.push(ContainerOption::EnvLiteral(EnvLiteral { key, value: path }));
        }
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
        let spec = WorkspaceMountSpec::new(
            session.git_root(),
            WorkspaceAccess::from_read_only(self.read_only_workspace),
        );
        let permission = if spec.access.is_read_only() {
            if !self.runtime.capabilities().read_only_workspace() {
                return Err(read_only_unsupported(self.runtime.display_name()));
            }
            OverlayPermission::ReadOnly
        } else {
            OverlayPermission::ReadWrite
        };
        let mut mounts = vec![
            ContainerOption::ReadOnlyWorkspace(spec.access.is_read_only()),
            ContainerOption::Overlay(OverlaySpec {
                host_path: spec.host_path,
                container_path: spec.guest_path,
                permission,
            }),
        ];
        mounts.extend(spec.tmpfs.into_iter().map(ContainerOption::Tmpfs));
        if self.read_only_workspace {
            let scratch = tempfile::Builder::new()
                .prefix("awman-scratch-")
                .tempdir_in(session.env().temp_dir())
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
        if let Some(tag) = session
            .lease_images()
            .and_then(|images| images.agents.get(agent.as_str()))
        {
            // Lease images are content-keyed and already provisioned, but the
            // CLI inside still has to be the pinned major (same rule as the
            // build and import paths below).
            let def = self.resolve_definition(session, agent)?;
            return self.check_agent_cli(&def, tag);
        }
        if session.lease_images().is_some() {
            return Err(EngineError::Config(format!("agent '{agent}' was not provisioned for this lease; include it in the workflow agent pool")));
        }
        let def = self.resolve_definition(session, agent)?;
        let paths = RepoDockerfilePaths::new(session.git_root());
        let project_tag = session.project_image_tag();
        let agent_tag = session.agent_image_tag(agent.as_str());
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
            use crate::engine::image_prep::{
                AgentImagePreparer, AgentImageRequest, BuildPolicy, ImageBuildContext,
            };
            let tag = session.agent_image_tag(agent.as_str());
            let resolved = config.builtin_settings()?;
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
            // An enabled local builder checks source freshness even for an
            // installed image. Preparation reuses current, provenance-matched
            // images without importing. Without a usable builder, retain the
            // normal check-only policy for installed images.

            let policy =
                preparer.missing_image_policy(&tag, agent.as_str(), BuildPolicy::CheckOnly)?;
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
                _ => self.check_agent_cli(&def, &tag),
            };
        }

        // Check for the project base image. If absent, fail with a structured
        // error: agent images are layered FROM the project image.
        let project_tag = session.project_image_tag();
        if !image_exists(&project_tag) {
            return Err(EngineError::AgentRequiresProjectImage { tag: project_tag });
        }

        // WI 0163: an installed named project or agent image whose inputs
        // label does not match the working tree (or has none) is rebuilt
        // under the shared build lease. A failed rebuild keeps the previous
        // image and reports `image_stale`; a missing agent image is built
        // below. Lease sessions run content-keyed tags lease provisioning
        // already resolved, so they are not checked here.
        if session.lease_images().is_none()
            && self.runtime.capabilities().image_acquisition
                == crate::engine::agent_runtime::ImageAcquisition::Build
        {
            let inputs = image_freshness::SessionImageInputs::for_repo(session.git_root(), config)?;
            image_freshness::refresh_stale_images(
                self.runtime.clone(),
                &inputs,
                &[agent.as_str().to_string()],
                frontend,
            )
            .await?;
        }

        let paths = RepoDockerfilePaths::new(session.git_root());
        let agent_dockerfile = paths.agent_dockerfile(agent.as_str());
        let agent_tag = session.agent_image_tag(agent.as_str());

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
                Err(EngineError::ImageBuildExitNonzero {
                    exit_code, output, ..
                }) => {
                    let error = EngineError::AgentImageBuildFailed {
                        agent: agent.as_str().to_string(),
                        exit_code,
                        output,
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

        self.check_agent_cli(&def, &agent_tag)
    }

    /// WI 0157: refuse to launch an image whose CLI reports the wrong
    /// major. Missing or unparseable metadata is advisory. Probes the same binary the launch executes
    /// (`cli_version.binary == interactive_entrypoint[0]`) inside the image,
    /// once per image identity. Probe failures warn and never gate launch.
    fn check_agent_cli(&self, def: &AgentDefinition, tag: &str) -> Result<(), EngineError> {
        use crate::engine::container::agent_version::{mismatch_message, CliVersionStatus};
        let Some(requirement) = def.cli_version.as_ref() else {
            return Ok(());
        };
        match self.runtime.probe_agent_cli_version(tag, requirement) {
            CliVersionStatus::Mismatch { found, expected } => Err(EngineError::Config(
                mismatch_message(&def.display_name, &def.name, tag, &found, &expected),
            )),
            CliVersionStatus::Unknown { reason } => {
                tracing::warn!(agent = %def.name, image = tag, %reason, "agent CLI version is unknown; continuing launch");
                Ok(())
            }
            _ => Ok(()),
        }
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
        if session
            .lease_images()
            .is_some_and(|images| !images.agents.contains_key(agent.as_str()))
        {
            return Err(EngineError::Config(format!("agent '{agent}' was not provisioned for this lease; include it in the workflow agent pool")));
        }
        if self.read_only_workspace {
            if !self.runtime.capabilities().read_only_workspace() {
                return Err(read_only_unsupported(self.runtime.display_name()));
            }
            if run.allow_docker {
                return Err(EngineError::Config(
                    "--allow-docker gives the agent the host Docker daemon, which can write to the workspace".into(),
                ));
            }
        }
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

        let image_tag = session
            .lease_images()
            .and_then(|images| images.agents.get(agent.as_str()))
            .cloned()
            .or_else(|| run.image_tag_override.clone())
            .unwrap_or_else(|| session.agent_image_tag(agent.as_str()));
        self.check_agent_cli(&def, &image_tag)?;
        let image = ImageRef::new(image_tag.clone());
        // ACP launches use the agent's ACP entrypoint (e.g. `cline --acp`) and
        // select the persistent piped-stdio container path instead of a PTY, so
        // the JSON-RPC 2.0 framing never passes through a terminal's cooked-mode
        // layer. The guard above has already rejected `Acp` for any agent whose
        // definition lacks an `acp_entrypoint`, so `entrypoint_for_acp` cannot fail
        // here in practice — the `?` is defence-in-depth.
        let acp = matches!(run.launch_mode, LaunchMode::Acp);
        let mut options = vec![
            ContainerOption::AgentIdentity {
                agent: agent.as_str().to_owned(),
                model: run.model.clone(),
            },
            ContainerOption::Image(image),
            ContainerOption::AllowDocker(run.allow_docker),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: session.id().to_string(),
            },
        ];
        if let Some(launch) = builtin_launch {
            options.push(ContainerOption::BuiltinLaunch(launch));
        }
        options.push(ContainerOption::ProgramStatus(
            crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
                enabled: session.effective_config().program_status_enabled(),
                answer_queries: false,
            },
        ));

        // Entrypoint, modes, tools, seed and model: the agent's argv contract,
        // pure and shared with the argv unit tests.
        options.extend(launch_argv_options(&def, run)?);
        seeded_prompt::stage_large_argv_prompt(session.env(), &def, run, &mut options)?;

        // User-configured extra agent args (`agentFlags` / `--agent-flag`). One
        // that the entrypoint already carries for this launch (codex
        // `--no-daemon` on an interactive launch, cursor `--trust` on a
        // headless one) is dropped, not doubled; the command layer warns
        // about it (`LaunchPolicy::warn_config`). An ACP entrypoint carries
        // none of them.
        let mut agent_flags = resolve_agent_flags(session, agent, run);
        def.verify_resume_flags(run, &agent_flags)?;
        if !acp {
            let managed = def.entrypoint_managed_flags(run.non_interactive);
            agent_flags.retain(|flag| !managed.contains(flag));
        }
        // A provider selector that contradicts --plan is refused; a matching
        // duplicate is dropped once (WI 0070).
        let agent_flags =
            def.reconcile_agent_flags_for_plan(run, agent_flags, agent_flags_origin(run))?;
        if !agent_flags.is_empty() {
            options.push(ContainerOption::AgentFlags(agent_flags));
        }

        // Env passthrough. A --plan launch does not pass names that would
        // select an incompatible starting mode in the guest (the host
        // environment is never modified); a caller-supplied secret of that
        // name is explicit, so it is refused instead.
        let env_pass = plan_env_passthrough(&def, run)?;
        let env_pass = env_pass.as_slice();
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
        emit_read_only_workspace_env(
            &def,
            container_home.as_deref().unwrap_or("/root"),
            self.read_only_workspace,
            &mut options,
        )?;
        // Private writable mounts are definition facts. The generic mount
        // option owns each tempdir through the execution's guest-exit guard;
        // an OverlayEngine-wide TempDir would be shared across launches.
        let mut private_dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
        if let Some(profile) = &run.profile_state {
            let spec = def
                .profile_state
                .as_ref()
                .ok_or_else(|| EngineError::Config("profile state layout absent".into()))?;
            for root in &spec.roots {
                private_dirs.push((
                    root.home_relative.clone(),
                    profile.run_dir.join(&root.home_relative),
                ));
                if let Some(var) = &root.env_var {
                    options.push(ContainerOption::EnvLiteral(
                        crate::engine::container::options::EnvLiteral {
                            key: var.clone(),
                            value: Path::new(container_home.as_deref().unwrap_or("/root"))
                                .join(&root.home_relative)
                                .display()
                                .to_string(),
                        },
                    ));
                }
            }
            for storage in &def.session_storage_dirs {
                let guest = Path::new(container_home.as_deref().unwrap_or("/root"))
                    .join(&storage.home_relative_path);
                if let Some(var) = &storage.env_var {
                    options.push(ContainerOption::EnvLiteral(
                        crate::engine::container::options::EnvLiteral {
                            key: var.clone(),
                            value: guest.display().to_string(),
                        },
                    ));
                }
                if let Some((flag, key)) = &storage.config_override {
                    options.push(ContainerOption::ManagedAgentFlags(vec![
                        flag.clone(),
                        format!("{key}={}", guest.display()),
                    ]));
                }
            }
            options.push(ContainerOption::Name(
                crate::engine::container::options::ContainerName(format!(
                    "awman-profile-{}",
                    profile.execution_id.as_str()
                )),
            ));
            options.push(ContainerOption::Label {
                key: "awman.profile-execution".into(),
                value: profile.execution_id.as_str().into(),
            });
        }
        for storage in def
            .session_storage_dirs
            .iter()
            .filter(|_| run.profile_state.is_none())
        {
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
                .tempdir_in(session.env().temp_dir())
                .map_err(|e| EngineError::Other(format!("create session storage: {e}")))?;
            // One-way private copy of host state (opencode-v2's config). Never
            // copied back; the host files are never written.
            if let Some(seed) = &storage.seed_from_home {
                session_seed::seed_with_data_home(
                    session.env().home_dir().as_deref(),
                    crate::data::fs::data_paths::data_home(session.env())
                        .ok()
                        .as_deref(),
                    seed,
                    dir.path(),
                    session_seed::SEED_MAX_ENTRIES,
                    session_seed::SEED_MAX_BYTES,
                )?;
            }
            private_dirs.push((storage.home_relative_path.clone(), dir.path().to_path_buf()));
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
        // Native plan prerequisites (WI 0070): verify the configuration the
        // plan selection relies on, then stage any private settings patch.
        // Both run before any container exists and never touch host files.
        prepare_native_plan(session, &def, run, env_pass, &private_dirs)?;
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
        let (overlays, staged_credentials) = self.overlay_engine.build_scoped_profile_overlays(
            session,
            &request,
            self.lease_workspace_scope.as_deref(),
            &run.command_mounts,
            run.profile_state.as_ref(),
        )?;
        if let Some(profile) = &run.profile_state {
            if let Some(spec) = &def.profile_state {
                let home = Path::new(container_home.as_deref().unwrap_or("/root"));
                for mount in &overlays {
                    for root in &spec.roots {
                        let guest = home.join(&root.home_relative);
                        if mount.container_path.starts_with(&guest)
                            || guest.starts_with(&mount.container_path)
                        {
                            let expected = profile.run_dir.join(&root.home_relative);
                            if mount.container_path != guest
                                || comparable_host_path(&mount.host_path)
                                    != comparable_host_path(&expected)
                            {
                                return Err(EngineError::Config("an overlay replaces private profile settings or history; remove the conflicting overlay".into()));
                            }
                        }
                    }
                }
            }
        }
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
        if self.read_only_workspace {
            // The read-only notice is mandatory. Refuse any launch whose
            // delivery channel cannot reach the agent rather than running it
            // with an uninformed agent.
            let acp = matches!(run.launch_mode, LaunchMode::Acp);
            let needs_context = matches!(
                def.system_prompt_delivery,
                SystemPromptMode::AgentsMd | SystemPromptMode::AddDir { .. }
            );
            if run.system_prompt.is_none()
                || matches!(def.system_prompt_delivery, SystemPromptMode::Unsupported)
                || acp
                || (needs_context && run.context_overlays.is_empty())
            {
                return Err(EngineError::Config(format!(
                    "agent '{}' cannot receive the read-only workspace instructions in \
                     this launch mode (ACP, no system-prompt channel, or no context \
                     directory to carry them); use another agent, add a context \
                     overlay, or launch without --read-only-workspace",
                    def.name
                )));
            }
        }
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
        // The configuration the guard verified must be what the guest reads:
        // no overlay may replace it (WI 0070).
        verify_plan_config_mounts(
            session,
            &def,
            run,
            Path::new(container_home.as_deref().unwrap_or("/root")),
            &private_dirs,
            &options,
        )?;
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
            if let Some(channel) = &run.automation_channel {
                options.extend(channel.container_options());
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

        // An agent whose credential cannot reach a sandbox says so up front
        // instead of failing mysteriously inside the VM (cursor).
        if let Some(note) = &def.sandbox_auth_note {
            options.push(SandboxOption::UnsupportedNote(note.clone()));
        }

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
        //
        // A requested --plan never takes the "note and continue" path: the
        // delivery was proven by `validate_run` above (a mixin without a
        // managed settings route is refused there, before provisioning).
        let plan_delivery = def.native_plan_delivery(run, RunParadigm::Sandbox)?;
        let mode_flags = def.mode_flags(run, RunParadigm::Sandbox)?;
        if plan_delivery
            .as_ref()
            .is_some_and(|delivery| delivery.sandbox_permission_mode)
        {
            options.push(SandboxOption::PermissionMode("plan".to_string()));
        } else if !mode_flags.is_empty() {
            match def.sbx_kit_kind {
                SbxKitKind::Agent => {
                    options.push(SandboxOption::AgentModeFlags(mode_flags));
                }
                SbxKitKind::Mixin => {
                    let mode = if matches!(run.yolo, Some(YoloMode::Enabled)) {
                        "yolo"
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
        let agent_flags =
            def.reconcile_agent_flags_for_plan(run, agent_flags, agent_flags_origin(run))?;
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
            if let Some(flag) = def
                .non_interactive_flag
                .as_ref()
                .or(def.headless_prompt_flag.as_ref())
            {
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
        let env_pass = plan_env_passthrough(&def, run)?;
        // The Plan-profile guard verifies what the sandbox VM is given: the
        // workspace and the passed environment. No host-global configuration
        // directory reaches the VM, so none is inspected (WI 0070).
        if let Some(delivery) = &plan_delivery {
            verify_plan_config(
                session,
                &def,
                run,
                &env_pass,
                None,
                delivery.settings.is_some(),
            )?;
        }
        for name in &env_pass {
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
    use std::io::Write;
    let agents_md = host_dir.join("AGENTS.md");
    // The context directory is agent-writable, so an existing `AGENTS.md` may be
    // a symlink or hardlink into the checkout. Never read-through or truncate it:
    // keep a plain single-link file only when its content already matches,
    // otherwise drop the directory entry (which leaves any link target
    // untouched) and create a fresh inode with `create_new` (O_EXCL, which does
    // not follow a symlink raced into place).
    if let Ok(meta) = std::fs::symlink_metadata(&agents_md) {
        if meta.file_type().is_file() && !has_extra_links(&meta) {
            if let Ok(existing) = std::fs::read_to_string(&agents_md) {
                if existing == prompt_text {
                    return;
                }
            }
        }
        if let Err(e) = std::fs::remove_file(&agents_md) {
            tracing::warn!(path = %agents_md.display(), error = %e,
                "context overlay: failed to replace AGENTS.md; skipping");
            return;
        }
    }
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&agents_md)
        .and_then(|mut f| f.write_all(prompt_text.as_bytes()));
    if let Err(e) = result {
        tracing::warn!(
            path = %agents_md.display(),
            error = %e,
            "context overlay: failed to plant AGENTS.md (host dir read-only?); \
             agent will not be automatically notified about the context directory"
        );
    }
}

#[cfg(unix)]
fn has_extra_links(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn has_extra_links(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod workspace_access_tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    fn session(sandbox: &TestSandbox) -> Session {
        Session::open_at_directory_root(
            sandbox.work(),
            crate::data::session::SessionOpenOptions {
                env: Some(crate::data::config::env::EnvSnapshot::from_full_env(
                    sandbox
                        .env()
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.to_string_lossy().into_owned(),
                                v.to_string_lossy().into_owned(),
                            )
                        })
                        .collect(),
                )),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn only_opencode_major_probes_gate_launch() {
        let sandbox = TestSandbox::for_test(TestCase::component(
            "engine::agent::workspace_access_tests::only_opencode_major_probes_gate_launch",
        ));
        let session = session(&sandbox);
        let runtime = Arc::new(image_freshness::test_runtime::BuildRuntime::default());
        let engine = AgentEngine::new(
            Arc::new(OverlayEngine::new(&session).unwrap()),
            runtime.clone(),
        );
        for agent in ["opencode", "opencode-v2"] {
            let def = first_party::first_party(agent).unwrap();
            assert!(engine.check_agent_cli(&def, "fixture").is_ok());
        }
        for (version, v1_ok, v2_ok) in [
            ("1.999.999", true, false),
            ("2.999.999", false, true),
            ("nightly", true, true),
            ("", true, true),
        ] {
            *runtime.cli_version_line.lock().unwrap() = Some(version.into());
            for (agent, expected) in [("opencode", v1_ok), ("opencode-v2", v2_ok)] {
                assert_eq!(
                    engine
                        .check_agent_cli(&first_party::first_party(agent).unwrap(), "fixture")
                        .is_ok(),
                    expected
                );
            }
        }
        let def = first_party::first_party("cursor").unwrap();
        engine.check_agent_cli(&def, "fixture").unwrap();
        *runtime.cli_version_line.lock().unwrap() = Some("2025.01.01-aaaaaaa".into());
        engine.check_agent_cli(&def, "fixture").unwrap();
        *runtime.cli_version_line.lock().unwrap() = Some("2026.11.17-f532c45".into());
        engine.check_agent_cli(&def, "fixture").unwrap();
    }

    #[test]
    fn read_only_workspace_spec_adds_tmpfs_and_preserves_lease_scratch() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::agent::workspace_access_tests::read_only_workspace_spec_adds_tmpfs_and_preserves_lease_scratch"));
        let session = session(&sandbox);
        let engine = AgentEngine::new(
            Arc::new(OverlayEngine::new(&session).unwrap()),
            Arc::new(crate::engine::container::ContainerRuntime::docker_in(
                session.env().clone(),
            )),
        );
        let writable = engine.workspace_mounts(&session).unwrap();
        assert!(!writable
            .iter()
            .any(|o| matches!(o, ContainerOption::Tmpfs(_))));
        let engine = engine.with_read_only_workspace(true);
        let options = engine.workspace_mounts(&session).unwrap();
        assert!(options
            .iter()
            .any(|o| matches!(o, ContainerOption::Tmpfs(p) if p == Path::new("/tmp"))));
        assert!(options.iter().any(|o| matches!(o, ContainerOption::TemporaryDirectoryMount { container_path, env_var, .. } if container_path == Path::new(READ_ONLY_SCRATCH_PATH) && env_var.as_deref() == Some(READ_ONLY_SCRATCH_ENV))));
        let resolved =
            crate::engine::container::options::ResolvedContainerOptions::resolve(options).unwrap();
        assert!(resolved.read_only_workspace);
        resolved.validate_read_only_workspace().unwrap();
        let run = AgentRunOptions {
            allow_docker: true,
            ..Default::default()
        };
        assert!(engine
            .build_options(&session, &AgentName::new("claude").unwrap(), &run)
            .unwrap_err()
            .to_string()
            .contains("host Docker daemon"));
    }

    #[test]
    fn cache_redirects_use_effective_home_and_respect_explicit_env() {
        let mut def = first_party::first_party("claude").unwrap();
        def.read_only_workspace_redirects
            .push(ReadOnlyWorkspaceRedirect {
                env_var: "AGENT_STATE_DIR".into(),
                home_relative_path: ".agent/state".into(),
            });
        let mut options = vec![ContainerOption::EnvPassthrough(EnvVar(
            "XDG_CACHE_HOME".into(),
        ))];
        let before = options.clone();
        emit_read_only_workspace_env(&def, "/home/agent", false, &mut options).unwrap();
        assert_eq!(options, before);
        emit_read_only_workspace_env(&def, "/home/agent", true, &mut options).unwrap();
        assert!(!options
            .iter()
            .any(|o| matches!(o, ContainerOption::EnvLiteral(v) if v.key == "XDG_CACHE_HOME")));
        assert!(options.contains(&ContainerOption::EnvLiteral(EnvLiteral {
            key: "XDG_STATE_HOME".into(),
            value: "/home/agent/.local/state".into()
        })));
        assert!(options.contains(&ContainerOption::EnvLiteral(EnvLiteral {
            key: "TMPDIR".into(),
            value: "/tmp".into()
        })));
        assert!(options.contains(&ContainerOption::EnvLiteral(EnvLiteral {
            key: "AGENT_STATE_DIR".into(),
            value: "/home/agent/.agent/state".into()
        })));
        def.read_only_workspace_redirects[0].home_relative_path = "../workspace".into();
        assert!(emit_read_only_workspace_env(&def, "/home/agent", true, &mut Vec::new()).is_err());
        assert!(emit_read_only_workspace_env(&def, "/workspace", true, &mut Vec::new()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn plant_agents_md_never_writes_through_links() {
        let sandbox = TestSandbox::for_test(TestCase::component(
            "engine::agent::workspace_access_tests::plant_agents_md_never_writes_through_links",
        ));
        let ctx = sandbox.tmp().join("ctx");
        std::fs::create_dir_all(&ctx).unwrap();
        let sentinel = sandbox.work().join("tracked.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        let agents = ctx.join("AGENTS.md");

        std::os::unix::fs::symlink(&sentinel, &agents).unwrap();
        super::plant_agents_md(&ctx, "instructions");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "instructions");

        std::fs::remove_file(&agents).unwrap();
        std::fs::hard_link(&sentinel, &agents).unwrap();
        super::plant_agents_md(&ctx, "instructions");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "instructions");
    }
}

#[cfg(test)]
mod native_plan_option_tests {
    //! WI 0070: what the production option builders emit for `--plan`,
    //! `--read-only-workspace` and both, independently.
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    fn sandbox(name: &str) -> TestSandbox {
        let id: &'static str =
            Box::leak(format!("engine::agent::native_plan_option_tests::{name}").into_boxed_str());
        TestSandbox::for_test(TestCase::component(id))
    }

    fn session(sandbox: &TestSandbox) -> Session {
        Session::open_at_directory_root(
            sandbox.work(),
            crate::data::session::SessionOpenOptions {
                env: Some(crate::data::config::env::EnvSnapshot::from_full_env(
                    sandbox
                        .env()
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.to_string_lossy().into_owned(),
                                v.to_string_lossy().into_owned(),
                            )
                        })
                        .collect(),
                )),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn engine(session: &Session) -> AgentEngine {
        AgentEngine::new(
            Arc::new(OverlayEngine::new(session).unwrap()),
            Arc::new(image_freshness::test_runtime::BuildRuntime::default()),
        )
    }

    fn plan(headless: bool) -> AgentRunOptions {
        AgentRunOptions {
            plan: Some(PlanMode::Enabled),
            non_interactive: headless,
            initial_prompt: Some("look".into()),
            system_prompt: Some("context".into()),
            ..Default::default()
        }
    }

    fn agent(name: &str) -> AgentName {
        AgentName::new(name).unwrap()
    }

    fn mode_flags(options: &[ContainerOption]) -> Vec<String> {
        options
            .iter()
            .filter_map(|o| match o {
                ContainerOption::AgentModeFlags(f) => Some(f.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn read_only_traces(options: &[ContainerOption]) -> Vec<String> {
        let mut traces = Vec::new();
        for option in options {
            match option {
                ContainerOption::Tmpfs(p) => traces.push(format!("tmpfs {}", p.display())),
                ContainerOption::ReadOnlyWorkspace(true) => traces.push("read-only".into()),
                ContainerOption::TemporaryDirectoryMount { container_path, .. }
                    if container_path == Path::new(READ_ONLY_SCRATCH_PATH) =>
                {
                    traces.push("scratch".into())
                }
                ContainerOption::EnvLiteral(v)
                    if ["XDG_CACHE_HOME", "XDG_STATE_HOME", "TMPDIR"].contains(&v.key.as_str()) =>
                {
                    traces.push(v.key.clone())
                }
                _ => {}
            }
        }
        traces
    }

    #[test]
    fn plan_only_adds_native_argv_and_no_mount_restriction() {
        let sb = sandbox("plan_only_adds_native_argv_and_no_mount_restriction");
        let session = session(&sb);
        let options = engine(&session)
            .build_options(&session, &agent("claude"), &plan(false))
            .unwrap();
        assert_eq!(mode_flags(&options), ["--permission-mode", "plan"]);
        assert!(
            read_only_traces(&options).is_empty(),
            "{:?}",
            read_only_traces(&options)
        );
    }

    #[test]
    fn read_only_only_adds_mount_restriction_and_no_plan_tokens() {
        let sb = sandbox("read_only_only_adds_mount_restriction_and_no_plan_tokens");
        let session = session(&sb);
        let mut run = plan(false);
        run.plan = None;
        let engine = engine(&session).with_read_only_workspace(true);
        for name in ["claude", "codex"] {
            let options = engine.build_options(&session, &agent(name), &run).unwrap();
            assert!(
                mode_flags(&options).is_empty(),
                "{name}: {:?}",
                mode_flags(&options)
            );
            let traces = read_only_traces(&options);
            assert!(
                traces.contains(&"scratch".to_string()),
                "{name}: {traces:?}"
            );
            assert!(traces.contains(&"TMPDIR".to_string()), "{name}: {traces:?}");
        }
        // Both: plan tokens and the read-only mount, each from its own input.
        let both = engine
            .build_options(&session, &agent("codex"), &plan(false))
            .unwrap();
        assert_eq!(mode_flags(&both), ["--sandbox", "read-only"]);
        assert!(read_only_traces(&both).contains(&"scratch".to_string()));
    }

    #[test]
    fn opencode_v2_tui_plan_patches_only_its_private_config_copy() {
        let sb = sandbox("opencode_v2_tui_plan_patches_only_its_private_config_copy");
        let host_config = sb.home().join(".config/opencode");
        std::fs::create_dir_all(&host_config).unwrap();
        let host_text = "{ // mine\n \"default_agent\": \"build\", \"theme\": \"dark\" }";
        std::fs::write(host_config.join("opencode.jsonc"), host_text).unwrap();
        let session = session(&sb);
        let options = engine(&session)
            .build_options(&session, &agent("opencode-v2"), &plan(false))
            .unwrap();
        assert!(
            mode_flags(&options).is_empty(),
            "the full TUI takes no plan argv"
        );
        let private = options
            .iter()
            .find_map(|o| match o {
                ContainerOption::TemporaryDirectoryMount {
                    container_path,
                    guard,
                    ..
                } if container_path.ends_with(".config/opencode") => {
                    Some(guard.host_path().to_path_buf())
                }
                _ => None,
            })
            .expect("private config copy");
        assert!(!private.starts_with(sb.home()), "never the host directory");
        let patched: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(private.join("opencode.jsonc")).unwrap())
                .unwrap();
        assert_eq!(patched["default_agent"], "plan");
        assert_eq!(patched["theme"], "dark");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(private.join("opencode.jsonc"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "private file");
        }
        assert_eq!(
            std::fs::read_to_string(host_config.join("opencode.jsonc")).unwrap(),
            host_text
        );

        // Headless uses argv and writes no patch.
        let headless = engine(&session)
            .build_options(&session, &agent("opencode-v2"), &plan(true))
            .unwrap();
        assert_eq!(mode_flags(&headless), ["--agent", "plan"]);
    }

    #[test]
    fn a_redefined_plan_agent_or_outranking_default_fails_before_any_container() {
        let sb = sandbox("a_redefined_plan_agent_or_outranking_default_fails_before_any_container");
        let session = session(&sb);
        std::fs::write(
            session.git_root().join("opencode.json"),
            "{\"agent\": {\"plan\": {\"permission\": {\"edit\": \"allow\"}}}}",
        )
        .unwrap();
        for name in ["opencode", "opencode-v2"] {
            let err = engine(&session)
                .build_options(&session, &agent(name), &plan(true))
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    EngineError::AgentModeConflict {
                        origin: "settings",
                        ..
                    }
                ),
                "{name}: {err:?}"
            );
        }
        std::fs::write(
            session.git_root().join("opencode.json"),
            "{\"default_agent\": \"build\"}",
        )
        .unwrap();
        assert!(matches!(
            engine(&session).build_options(&session, &agent("opencode-v2"), &plan(false)),
            Err(EngineError::AgentModeConflict { .. })
        ));
        engine(&session)
            .build_options(&session, &agent("opencode-v2"), &plan(true))
            .unwrap();
        // Without --plan the guard is never consulted.
        let mut no_plan = plan(false);
        no_plan.plan = None;
        engine(&session)
            .build_options(&session, &agent("opencode-v2"), &no_plan)
            .unwrap();
    }

    #[test]
    fn plan_drops_inherited_permission_env_and_refuses_contradicting_agent_flags() {
        let sb =
            sandbox("plan_drops_inherited_permission_env_and_refuses_contradicting_agent_flags");
        let session = session(&sb);
        let env_names = |options: &[ContainerOption]| -> Vec<String> {
            options
                .iter()
                .filter_map(|o| match o {
                    ContainerOption::EnvPassthrough(v) => Some(v.0.clone()),
                    _ => None,
                })
                .collect()
        };
        let mut run = plan(true);
        run.env_passthrough = Some(vec!["OPENCODE_PERMISSION".into(), "KEEP".into()]);
        let options = engine(&session)
            .build_options(&session, &agent("opencode"), &run)
            .unwrap();
        assert_eq!(env_names(&options), ["KEEP"]);
        assert_eq!(
            plan_env_dropped(&first_party::first_party("opencode").unwrap(), &run),
            ["OPENCODE_PERMISSION"]
        );
        run.plan = None;
        let options = engine(&session)
            .build_options(&session, &agent("opencode"), &run)
            .unwrap();
        assert_eq!(env_names(&options), ["OPENCODE_PERMISSION", "KEEP"]);

        let mut flags = plan(false);
        flags.agent_flags = Some(vec!["--permission-mode".into(), "acceptEdits".into()]);
        assert!(matches!(
            engine(&session).build_options(&session, &agent("claude"), &flags),
            Err(EngineError::AgentModeConflict {
                origin: "--agent-flag",
                ..
            })
        ));
        flags.agent_flags = Some(vec![
            "--permission-mode".into(),
            "plan".into(),
            "--verbose".into(),
        ]);
        let options = engine(&session)
            .build_options(&session, &agent("claude"), &flags)
            .unwrap();
        assert_eq!(mode_flags(&options), ["--permission-mode", "plan"]);
        let user: Vec<String> = options
            .iter()
            .filter_map(|o| match o {
                ContainerOption::AgentFlags(f) => Some(f.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(user, ["--verbose"], "duplicate normalised once");
    }

    #[test]
    fn sandbox_plan_is_delivered_or_refused_never_noted() {
        let sb = sandbox("sandbox_plan_is_delivered_or_refused_never_noted");
        let session = session(&sb);
        let engine = engine(&session);
        let claude = engine
            .build_sandbox_options(&session, &agent("claude"), &plan(false))
            .unwrap();
        assert!(claude
            .iter()
            .any(|o| matches!(o, SandboxOption::PermissionMode(m) if m == "plan")));
        assert!(!claude
            .iter()
            .any(|o| matches!(o, SandboxOption::UnsupportedNote(n) if n.contains("plan"))));
        for name in ["codex", "copilot"] {
            assert!(
                matches!(
                    engine.build_sandbox_options(&session, &agent(name), &plan(false)),
                    Err(EngineError::NativePlanRouteUnsupported { .. })
                ),
                "{name}"
            );
        }
        let kit = engine
            .build_sandbox_options(&session, &agent("opencode"), &plan(true))
            .unwrap();
        assert!(kit
            .iter()
            .any(|o| matches!(o, SandboxOption::AgentModeFlags(f) if f == &["--agent", "plan"])));
        // yolo on a mixin keeps its existing note behaviour.
        let yolo = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            ..Default::default()
        };
        assert!(engine
            .build_sandbox_options(&session, &agent("codex"), &yolo)
            .unwrap()
            .iter()
            .any(|o| matches!(o, SandboxOption::UnsupportedNote(n) if n.contains("--yolo"))));
    }

    fn session_with_env(sandbox: &TestSandbox, extra: &[(&str, &str)]) -> Session {
        let mut env: std::collections::BTreeMap<String, String> = sandbox
            .env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Session::open_at_directory_root(
            sandbox.work(),
            crate::data::session::SessionOpenOptions {
                env: Some(crate::data::config::env::EnvSnapshot::from_full_env(env)),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn conflict_origin(result: Result<impl std::fmt::Debug, EngineError>) -> &'static str {
        match result {
            Err(EngineError::AgentModeConflict { origin, .. }) => origin,
            other => panic!("expected an agent mode conflict, got {other:?}"),
        }
    }

    /// WI 0070 review finding 2: `.opencode/opencode.json[c]` is project
    /// configuration that outranks the direct files, so the guard reads it
    /// (and refuses a symlinked configuration directory it cannot map).
    #[test]
    fn nested_project_opencode_config_is_inspected() {
        let sb = sandbox("nested_project_opencode_config_is_inspected");
        let session = session(&sb);
        let nested = session.git_root().join(".opencode");
        std::fs::create_dir_all(&nested).unwrap();
        for name in ["opencode.json", "opencode.jsonc"] {
            std::fs::write(nested.join(name), "{\"default_agent\": \"build\"}").unwrap();
            assert_eq!(
                conflict_origin(engine(&session).build_options(
                    &session,
                    &agent("opencode-v2"),
                    &plan(false)
                )),
                "settings",
                "{name}"
            );
            std::fs::write(
                nested.join(name),
                "{\"agent\": {\"plan\": {\"disable\": true}}}",
            )
            .unwrap();
            for name in ["opencode", "opencode-v2"] {
                assert_eq!(
                    conflict_origin(engine(&session).build_options(
                        &session,
                        &agent(name),
                        &plan(true)
                    )),
                    "settings"
                );
            }
            std::fs::remove_file(nested.join(name)).unwrap();
        }
        engine(&session)
            .build_options(&session, &agent("opencode-v2"), &plan(false))
            .unwrap();
        #[cfg(unix)]
        {
            std::fs::remove_dir(&nested).unwrap();
            let elsewhere = sb.root().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &nested).unwrap();
            assert_eq!(
                conflict_origin(engine(&session).build_options(
                    &session,
                    &agent("opencode"),
                    &plan(true)
                )),
                "settings"
            );
        }
    }

    /// WI 0070 review finding 4: the configuration the guard checked must be
    /// the configuration the guest reads, so no overlay may replace a
    /// project file, a `.opencode` directory or a (private) global file,
    /// with or without the read-only workspace restriction.
    #[test]
    fn an_overlay_cannot_replace_the_verified_plan_configuration() {
        let sb = sandbox("an_overlay_cannot_replace_the_verified_plan_configuration");
        let session = session(&sb);
        let payload = sb.root().join("payload.json");
        std::fs::write(&payload, "{\"agent\": {\"plan\": {\"disable\": true}}}").unwrap();
        let payload_dir = sb.root().join("payload-dir");
        std::fs::create_dir_all(&payload_dir).unwrap();
        let overlay = |host: &Path, guest: &str| DirectorySpec {
            host: host.display().to_string(),
            container: guest.into(),
            permission: crate::engine::container::options::OverlayPermission::ReadOnly,
        };
        // A read-only OpenCode launch carries its instructions in a context
        // directory; it is unrelated to the configuration under test.
        let context = sb.root().join("context");
        std::fs::create_dir_all(&context).unwrap();
        let with = |overlays: Vec<DirectorySpec>| AgentRunOptions {
            directory_overlays: overlays,
            context_overlays: vec![ContextOverlay {
                scope: crate::engine::overlay::ContextScope::Repo,
                host_path: context.clone(),
                container_path: "/awman/context/repo".into(),
                permission: crate::engine::container::options::OverlayPermission::ReadOnly,
            }],
            ..plan(true)
        };
        // The guest home of the v2 private configuration copy.
        let private_config = engine(&session)
            .build_options(&session, &agent("opencode-v2"), &plan(false))
            .unwrap()
            .into_iter()
            .find_map(|o| match o {
                ContainerOption::TemporaryDirectoryMount { container_path, .. }
                    if container_path.ends_with(".config/opencode") =>
                {
                    Some(container_path.display().to_string())
                }
                _ => None,
            })
            .expect("private config mount");
        for read_only in [false, true] {
            let engine = engine(&session).with_read_only_workspace(read_only);
            for (name, run) in [
                (
                    "opencode",
                    with(vec![overlay(&payload, "/workspace/opencode.json")]),
                ),
                (
                    "opencode",
                    with(vec![overlay(
                        &payload,
                        "/workspace/.opencode/opencode.jsonc",
                    )]),
                ),
                (
                    "opencode",
                    with(vec![overlay(&payload_dir, "/workspace/.opencode")]),
                ),
                (
                    "opencode-v2",
                    with(vec![overlay(
                        &payload,
                        &format!("{private_config}/opencode.json"),
                    )]),
                ),
                (
                    "opencode-v2",
                    with(vec![overlay(
                        &payload_dir,
                        &format!("{private_config}/agents"),
                    )]),
                ),
            ] {
                assert_eq!(
                    conflict_origin(engine.build_options(&session, &agent(name), &run)),
                    "overlay",
                    "{name} read_only={read_only}: {:?}",
                    run.directory_overlays
                );
            }
            // An unrelated overlay and the same overlay without --plan pass.
            engine
                .build_options(
                    &session,
                    &agent("opencode"),
                    &with(vec![overlay(&payload_dir, "/mnt/reference")]),
                )
                .unwrap();
            if !read_only {
                let mut no_plan = with(vec![overlay(&payload, "/workspace/opencode.json")]);
                no_plan.plan = None;
                engine
                    .build_options(&session, &agent("opencode"), &no_plan)
                    .unwrap();
            }
        }
    }

    /// WI 0070 review finding 5: the Docker Sandboxes builder runs the same
    /// Plan-profile guard over what the VM is given (workspace and passed
    /// environment) before emitting `--agent plan`.
    #[test]
    fn sandbox_plan_runs_the_opencode_configuration_guard() {
        let sb = sandbox("sandbox_plan_runs_the_opencode_configuration_guard");
        let session = session(&sb);
        std::fs::write(
            session.git_root().join("opencode.json"),
            "{\"agent\": {\"plan\": {\"disable\": true}}}",
        )
        .unwrap();
        assert_eq!(
            conflict_origin(engine(&session).build_sandbox_options(
                &session,
                &agent("opencode"),
                &plan(true)
            )),
            "settings"
        );
        std::fs::remove_file(session.git_root().join("opencode.json")).unwrap();
        engine(&session)
            .build_sandbox_options(&session, &agent("opencode"), &plan(true))
            .unwrap();
        for (name, value, origin) in [
            ("OPENCODE_CONFIG", "/elsewhere/opencode.json", "environment"),
            (
                "OPENCODE_CONFIG_CONTENT",
                "{\"agent\": {\"plan\": {\"disable\": true}}}",
                "settings",
            ),
        ] {
            let session = session_with_env(&sb, &[(name, value)]);
            let run = AgentRunOptions {
                env_passthrough: Some(vec![name.into()]),
                ..plan(true)
            };
            assert_eq!(
                conflict_origin(engine(&session).build_sandbox_options(
                    &session,
                    &agent("opencode"),
                    &run
                )),
                origin,
                "{name}"
            );
            // The container route refuses the same input.
            assert_eq!(
                conflict_origin(engine(&session).build_options(&session, &agent("opencode"), &run)),
                origin,
                "{name}"
            );
        }
        // A host-global file the VM never receives is not inspected.
        let host = sb.home().join(".config/opencode");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(
            host.join("opencode.json"),
            "{\"agent\": {\"plan\": {\"disable\": true}}}",
        )
        .unwrap();
        engine(&session)
            .build_sandbox_options(&session, &agent("opencode"), &plan(true))
            .unwrap();
    }

    use definition::PlanConfigGuard;
    use opencode_plan_guard::{SourceKind, SourceScope};

    #[test]
    fn relocated_or_unreadable_opencode_config_cannot_be_verified() {
        use crate::engine::agent::opencode_plan_guard::collect_opencode_sources;
        let dir = tempfile::tempdir().unwrap();
        for name in ["OPENCODE_CONFIG", "OPENCODE_CONFIG_DIR"] {
            let err = collect_opencode_sources(
                "opencode",
                PlanConfigGuard::OpenCodeV1,
                None,
                dir.path(),
                &[(name.to_string(), Some("/x".into()))],
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    EngineError::AgentModeConflict {
                        origin: "environment",
                        ..
                    }
                ),
                "{err:?}"
            );
        }
        let inline = collect_opencode_sources(
            "opencode",
            PlanConfigGuard::OpenCodeV1,
            None,
            dir.path(),
            &[("OPENCODE_CONFIG_CONTENT".into(), Some("{}".into()))],
        )
        .unwrap();
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].scope, SourceScope::Environment);
        // Oversize and symlinked files are conflicts; absent files are not.
        std::fs::write(
            dir.path().join("opencode.json"),
            vec![b' '; 1024 * 1024 + 1],
        )
        .unwrap();
        assert!(collect_opencode_sources(
            "opencode",
            PlanConfigGuard::OpenCodeV1,
            None,
            dir.path(),
            &[]
        )
        .is_err());
        std::fs::remove_file(dir.path().join("opencode.json")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", dir.path().join("opencode.jsonc")).unwrap();
            assert!(collect_opencode_sources(
                "opencode",
                PlanConfigGuard::OpenCodeV1,
                None,
                dir.path(),
                &[]
            )
            .is_err());
            std::fs::remove_file(dir.path().join("opencode.jsonc")).unwrap();
        }
        std::fs::create_dir_all(dir.path().join(".opencode/agents")).unwrap();
        std::fs::write(dir.path().join(".opencode/agents/plan.md"), "x").unwrap();
        let found = collect_opencode_sources(
            "opencode",
            PlanConfigGuard::OpenCodeV1,
            None,
            dir.path(),
            &[],
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, SourceKind::MarkdownAgent);
        assert!(
            collect_opencode_sources("claude", PlanConfigGuard::None, None, dir.path(), &[])
                .unwrap()
                .is_empty()
        );
    }
}

pub mod credential_routes;
