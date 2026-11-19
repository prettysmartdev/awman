//! Typed `ContainerOption` enum and surrounding option types.
//!
//! Each supported container launch setting has a typed variant. Adding an
//! option requires one variant and one branch in `ResolvedContainerOptions::ingest`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::agent_output_format::AgentOutputFormat;
use crate::data::config::builtin_launch::BuiltinLaunchSettings;
use crate::engine::auth::RefreshableCredentialDelivery;

/// A reference to a container image (e.g. `awman-myproj-claude:latest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef(pub String);

impl ImageRef {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Container entrypoint command + args (e.g. `["claude", "--print"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrypoint(pub Vec<String>);

impl Entrypoint {
    pub fn new(parts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(parts.into_iter().map(Into::into).collect())
    }
}

/// Stable name for a container (e.g. `awman-abc123`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerName(pub String);

impl ContainerName {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A directory or file overlay to mount into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

// `OverlayPermission` is Layer 0: the overlay grammar in
// `data::config::overlays` parses it, and a Layer 0 parser cannot name a
// Layer 1 type (WI 0114 F-27). Re-exported here for one release so existing
// `engine::container::options::OverlayPermission` paths still compile.
pub use crate::data::config::overlays::OverlayPermission;

/// A passthrough environment variable (read from host at launch time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar(pub String);

/// A literal env-var key/value pair injected into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvLiteral {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YoloMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanMode {
    #[default]
    Disabled,
    Enabled,
}

/// CPU limit in fractional cores (e.g. `2.0` for two cores).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLimit(pub f64);

/// Memory limit in megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimit(pub u64);

/// How a model flag is delivered to the agent (e.g. `--model NAME` vs
/// `--model-claude-opus-4-6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFlagForm {
    /// `--model NAME`
    Argument(String),
    /// A standalone shorthand like `--model-claude-opus-4-6`.
    Shorthand(String),
}

/// A bundle of host-side agent settings prepared by `OverlayEngine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSettings {
    /// Container `$HOME` (typically `/root` or `/home/<user>`).
    pub container_home: String,
    /// Pre-built overlay specs derived from the host's agent config files.
    pub overlays: Vec<OverlaySpec>,
}

/// Owns one launch's temporary mounted file or directory. Clones share it and the
/// last one normally deletes it. The file follows this launch's options,
/// instance and execution; an observer lost before confirmed termination can
/// preserve the path while still releasing the descriptor.
#[derive(Debug, Clone)]
pub struct MountGuard(Arc<PromptFile>);

/// Historical name retained for the system-prompt mount paths.
pub type PromptFileGuard = MountGuard;

#[derive(Debug)]
struct PromptFile {
    file: Option<tempfile::NamedTempFile>,
    dir: Option<tempfile::TempDir>,
    preserve: std::sync::atomic::AtomicBool,
}

impl Drop for PromptFile {
    fn drop(&mut self) {
        // If the last observer disappears before guest termination is known,
        // leave the path readable but close the descriptor normally.
        if self.preserve.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(file) = &mut self.file {
                file.disable_cleanup(true);
            }
            if let Some(dir) = self.dir.take() {
                std::mem::forget(dir);
            }
        }
    }
}

impl MountGuard {
    pub fn new(file: tempfile::NamedTempFile) -> Self {
        Self(Arc::new(PromptFile {
            file: Some(file),
            dir: None,
            preserve: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub fn new_dir(dir: tempfile::TempDir) -> Self {
        Self(Arc::new(PromptFile {
            file: None,
            dir: Some(dir),
            preserve: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub fn host_path(&self) -> &Path {
        self.0
            .file
            .as_ref()
            .map(|f| f.path())
            .or_else(|| self.0.dir.as_ref().map(|d| d.path()))
            .expect("prompt guard has a file or directory")
    }

    pub(super) fn preserve_for_unconfirmed_guest(&self) {
        self.0
            .preserve
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl PartialEq for MountGuard {
    fn eq(&self, other: &Self) -> bool {
        self.host_path() == other.host_path()
    }
}

/// Every knob a `AgentInstance` accepts. Adding a new option is a single
/// variant and a single branch in `ResolvedContainerOptions::ingest`.
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerOption {
    AgentIdentity {
        agent: String,
        model: Option<String>,
    },
    ProgramStatus(crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig),
    Image(ImageRef),
    Entrypoint(Entrypoint),
    Overlay(OverlaySpec),
    /// Writable memory-backed guest directory (never a host bind).
    Tmpfs(PathBuf),
    /// Required workspace policy, carried to the backend's final launch check.
    ReadOnlyWorkspace(bool),
    /// A private host directory mounted read-write for this execution. The
    /// guard follows the same confirmed-guest-exit lifecycle as prompt mounts.
    TemporaryDirectoryMount {
        container_path: PathBuf,
        env_var: Option<String>,
        guard: MountGuard,
    },
    /// A read-only file inside a private temporary directory owned by `guard`.
    /// Retained until confirmed guest exit, like system prompt files.
    TemporaryFileMount {
        host_path: PathBuf,
        container_path: PathBuf,
        guard: MountGuard,
    },
    EnvPassthrough(EnvVar),
    /// Per-launch value, kept out of argv and redacted in Debug.
    EnvOverride {
        key: String,
        value: crate::engine::auth::credential::SecretString,
    },
    EnvLiteral(EnvLiteral),
    NativeResume {
        tokens: Vec<String>,
        position: crate::engine::agent::definition::ResumePosition,
    },
    SeededPrompt(String),
    HeadlessPromptInArgv(bool),
    /// Flag used to deliver the seeded prompt in *interactive* mode (e.g.
    /// opencode `--prompt <text>`). Absent means the prompt is appended as a
    /// trailing positional argv arg, which is what most agents expect. Emitted
    /// alongside `SeededPrompt` by `build_options` when the agent matrix's
    /// `interactive_seed_delivery` is `Flag`.
    InteractiveSeedFlag(String),
    /// Flag placed directly before a headless argv prompt (copilot `-p`),
    /// after every other agent flag, so nothing can land between the flag
    /// and its value.
    HeadlessPromptFlag(String),
    Interactive(bool),
    /// Structured stdout protocol for a headless stdio launch.
    AgentOutputFormat(AgentOutputFormat),
    /// Launch the agent over ACP (Agent Client Protocol) instead of raw
    /// container stdio. Selects the persistent-piped spawn path (`-i`, no PTY)
    /// so a newline-delimited JSON-RPC 2.0 channel can ride the container's
    /// stdio pipes for the whole session. Introduces NO new host exposure —
    /// no ports, no `--network`, no new mounts (see
    /// `aspec/architecture/security.md`); the bytes flow over the exact stdio
    /// pipes `-i` already wires up.
    Acp(bool),
    AllowDocker(bool),
    Yolo(YoloMode),
    Auto(AutoMode),
    Plan(PlanMode),
    WorkingDir(PathBuf),
    Name(ContainerName),
    Cpu(CpuLimit),
    Memory(MemoryLimit),
    AgentSettingsPassthrough(AgentSettings),
    AgentCredentials {
        env_vars: Vec<(String, String)>,
    },
    /// A refreshable credential already planted in a staged settings overlay.
    /// It carries paths and an opaque fingerprint only — never secret bytes.
    RefreshableCredential(RefreshableCredentialDelivery),
    /// Who registers the leases for this launch's refreshable credentials
    /// (F-38). Absent means leases are disabled — see
    /// [`ResolvedContainerOptions::lease_factory`].
    CredentialLeaseFactory(crate::engine::credential_refresh::LeaseFactoryHandle),
    DisallowedTools(Vec<String>),
    AllowedTools(Vec<String>),
    Model {
        flag: ModelFlagForm,
    },
    /// Container-side `$HOME` remapped from `/root` when a non-root `USER`
    /// directive is detected in the agent's Dockerfile.
    DockerfileUser(String),
    /// A container label — emitted as `--label <key>=<value>`. Used to
    /// attribute containers (e.g. `awman.session=<id>` so `list_running` can
    /// map a container to an awman session, or `awman.squad.task=<name>`
    /// for squad's background agents). Multiple labels accumulate.
    Label {
        key: String,
        value: String,
    },
    /// Per-agent mode flags (yolo, auto, plan) — emitted as literal argv
    /// strings after the entrypoint in `build_run_argv`.
    AgentModeFlags(Vec<String>),
    /// User-configured extra agent arguments (`agentFlags` / `--agent-flag`) —
    /// appended after every awman-generated agent flag and before the seeded
    /// prompt.
    AgentFlags(Vec<String>),
    /// Per-launch agent flags derived from awman-owned mounts. These follow
    /// user flags so a user config cannot redirect private session storage.
    ManagedAgentFlags(Vec<String>),
    /// The flag name to use when emitting disallowed tools (e.g. `--disallowedTools`).
    DisallowedToolsFlag(String),
    /// The flag name to use when emitting allowed tools (e.g. `--allowedTools`).
    AllowedToolsFlag(String),
    /// Keep the container after exit (do not pass `--rm`).
    KeepContainer,
    /// System prompt delivered via a file mount + CLI flag (e.g.
    /// `--append-system-prompt-file <container_path>`).
    SystemPromptFile {
        host_path: PathBuf,
        container_path: PathBuf,
        flag: String,
        guard: PromptFileGuard,
    },
    /// System prompt delivered via an env var pointing to a mounted file
    /// (a custom agent's `[system_prompt] env`, e.g. `AGENT_SYSTEM_MD=<container_path>`).
    SystemPromptEnvFile {
        env_var: String,
        host_path: PathBuf,
        container_path: PathBuf,
        guard: PromptFileGuard,
    },
    /// System prompt delivered as inline text via a CLI flag (e.g.
    /// `--system <text>` for cline).
    SystemPromptInline {
        flag: String,
        text: String,
    },
    /// Extra workspace dir for the agent (e.g. `--add-dir <container_path>`).
    AgentAddDir {
        flag: String,
        container_path: PathBuf,
    },
    /// How long the container may stay silent at startup before it is
    /// considered failed to start and stopped. Overrides the frontend's
    /// `grace_timeout`: that default suits an agent, but a shell command may
    /// legitimately print nothing for a long time.
    StartupGrace(std::time::Duration),
    /// Keep the container's whole output in its `OutputTail` instead of the
    /// last `DEFAULT_OUTPUT_TAIL_LINES` lines.
    FullTranscript,
    /// The session's builtin launch settings (default vCPUs and memory,
    /// network policy), resolved by `AgentEngine` from the session's
    /// `EffectiveConfig`. Only the builtin backend reads them, and it refuses
    /// to launch without them; every other backend ignores them.
    BuiltinLaunch(BuiltinLaunchSettings),
}

/// Injection-time dedup: drop any entry from `agent_credentials` whose
/// credential key maps to the same provider service as a key already declared
/// in `env_passthrough` or `env_literal`, **and whose host value is actually
/// resolvable**.
///
/// Mirrors the rationale of the sbx path's `CLAUDE_CODE_OAUTH_TOKEN` silent
/// skip ([`crate::engine::sandbox::dsbx::auth::inject_credentials`]):
/// when the harness has **declared** an env var (via `env(VAR)`) that already
/// authenticates the same provider, the keychain OAuth token is redundant and
/// its presence causes the container to receive two conflicting credentials for
/// the same service.
///
/// An `env_passthrough` entry only counts as "covering" a service when its host
/// value is actually set — mirroring `build_run_argv`'s own emission gate
/// (`if let Ok(value) = std::env::var(name)`).  A declared but unset passthrough
/// var does NOT suppress a keychain credential, because the passthrough will emit
/// nothing and the container would otherwise receive zero credentials for that
/// service.  `env_literal` entries always carry a value and always count.
///
/// `lookup_env` abstracts the host env lookup so that callers in tests can
/// supply a hermetic closure instead of reading `std::env::var` directly.
///
/// If dedup would drop the **last** remaining credential for a service, a
/// `log::warn!` is emitted so the situation is never silent.
///
/// Example: harness declares `env(ANTHROPIC_API_KEY)` → service "anthropic";
/// keychain resolves `CLAUDE_CODE_OAUTH_TOKEN` → also service "anthropic";
/// host has `ANTHROPIC_API_KEY` set → the passthrough will be emitted →
/// `CLAUDE_CODE_OAUTH_TOKEN` is dropped from `agent_credentials`.
///
/// Counter-example: same declaration but `ANTHROPIC_API_KEY` is NOT set on the
/// host → the passthrough emits nothing → `CLAUDE_CODE_OAUTH_TOKEN` is retained.
pub(crate) fn dedup_credentials_by_declared_env(
    agent_credentials: &mut Vec<(String, String)>,
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
    lookup_env: &dyn Fn(&str) -> Option<String>,
) {
    // Collect the set of provider services already covered by declared env vars
    // that will actually be emitted to the container:
    //   - env_passthrough: only when the host value is set (and non-empty).
    //   - env_literal: always (they carry an explicit value).
    let covered_services: Vec<&'static str> = env_passthrough
        .iter()
        .filter(|v| lookup_env(v.0.as_str()).is_some_and(|val| !val.is_empty()))
        .map(|v| v.0.as_str())
        .chain(env_literal.iter().map(|l| l.key.as_str()))
        .filter_map(crate::engine::auth::service_for_credential)
        .collect();

    if covered_services.is_empty() {
        return;
    }

    // Before dropping, check whether this dedup would leave any service with
    // zero credentials.  If so, warn — the outcome is intentional (the literal
    // or resolvable passthrough will cover it), but it should never be silent.
    for service in &covered_services {
        let service_creds_before: Vec<_> = agent_credentials
            .iter()
            .filter(|(k, _)| crate::engine::auth::service_for_credential(k) == Some(service))
            .collect();
        if !service_creds_before.is_empty() {
            tracing::warn!(
                dropped_keys = ?service_creds_before
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
                service = service,
                "awman: dropping keychain credential(s) for service because the repo \
                 declared an env var that covers the same provider; the container will \
                 receive credentials via the declared env overlay.",
            );
        }
    }

    // Retain only credentials whose service is NOT already covered by a
    // harness-declared env var that will be emitted to the container.
    agent_credentials.retain(|(key, _)| {
        match crate::engine::auth::service_for_credential(key) {
            Some(service) => !covered_services.contains(&service),
            // Credential with no known service mapping — retain unconditionally.
            None => true,
        }
    });
}

/// Apply the same declared-env service coverage rule to staged credential
/// files as to env-delivered credentials. A file that loses dedup is removed
/// before the overlay can be mounted, so an explicit API-key configuration
/// never also exposes an OAuth credential file.
pub(crate) fn dedup_refreshable_by_declared_env(
    refreshable: &mut Vec<RefreshableCredentialDelivery>,
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
    lookup_env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), ResolveError> {
    let covered_services = covered_credential_services(env_passthrough, env_literal, lookup_env);
    if covered_services.is_empty() {
        return Ok(());
    }

    let mut kept: Vec<RefreshableCredentialDelivery> = Vec::with_capacity(refreshable.len());
    for credential in std::mem::take(refreshable) {
        let service = crate::engine::auth::service_for_credential(credential.credential_env_key);
        let covered = service
            .map(|s| covered_services.contains(&s))
            .unwrap_or(false);
        if !covered {
            kept.push(credential);
            continue;
        }
        // A declared env var covers this service, so the staged OAuth file must
        // not be mounted. Suppression is FAIL CLOSED (MEDIUM-8): if the file
        // cannot be unlinked, neutralize it in place (overwrite with an empty
        // 0600 payload) so the mount carries no secret; only if BOTH fail do we
        // abort the launch rather than silently mount the OAuth credential
        // alongside the user's chosen API key.
        neutralize_suppressed_credential(&credential.staged_path).map_err(|error| {
            ResolveError::CredentialSuppression(format!(
                "could not suppress staged OAuth credential {} that is superseded by a declared \
                 env var (service {:?}): {error}",
                credential.staged_path.display(),
                service
            ))
        })?;
        tracing::warn!(
            credential_env_key = credential.credential_env_key,
            ?service,
            "dropping staged refreshable credential because a declared env var covers the same service"
        );
    }
    *refreshable = kept;
    Ok(())
}

/// Ensure a suppressed staged credential file carries no secret in the mount.
/// Prefers unlink; on failure overwrites the file with an empty 0600 JSON
/// object. Returns the last error only when neither could be done.
fn neutralize_suppressed_credential(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(unlink_error) => {
            // Fall back to overwriting the contents in place: unlink needs write
            // permission on the parent directory, but truncating the file itself
            // only needs write permission on the file.
            #[cfg(unix)]
            let overwrite = {
                use std::io::Write as _;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(path)
                    .and_then(|mut f| f.write_all(b"{}"))
            };
            #[cfg(not(unix))]
            let overwrite = std::fs::write(path, b"{}");
            overwrite.map_err(|_| unlink_error)
        }
    }
}

fn covered_credential_services(
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
    lookup_env: &dyn Fn(&str) -> Option<String>,
) -> Vec<&'static str> {
    env_passthrough
        .iter()
        .filter(|v| lookup_env(v.0.as_str()).is_some_and(|val| !val.is_empty()))
        .map(|v| v.0.as_str())
        .chain(env_literal.iter().map(|l| l.key.as_str()))
        .filter_map(crate::engine::auth::service_for_credential)
        .collect()
}

/// Credential names only, allowing native Docker reattach to recover the
/// same output protection from the container environment without persisting values.
pub(crate) const OUTPUT_SECRET_ENV_LABEL: &str = "awman.output-secret-env";

/// Resolved option bag — all options merged into a single struct that the
/// backend consumes. Conflicting options are detected here.
#[derive(Debug, Clone, Default)]
pub struct ResolvedContainerOptions {
    pub output_redaction: crate::engine::auth::redaction::RedactionSet,
    pub host_env: Option<crate::data::config::env::EnvSnapshot>,
    pub agent_identity: Option<(String, Option<String>)>,
    pub program_status: crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig,
    pub image: Option<ImageRef>,
    pub entrypoint: Option<Entrypoint>,
    pub overlays: Vec<OverlaySpec>,
    pub tmpfs: Vec<PathBuf>,
    pub read_only_workspace: bool,
    /// Apple accepts bare tmpfs destinations; Docker accepts mount options.
    /// Set by the Apple backend before handing options to the shared launcher.
    pub tmpfs_bare_paths: bool,
    pub env_passthrough: Vec<EnvVar>,
    /// Values selected for this execution; opaque, never serialized or placed in argv.
    pub resolved_env_grants: Option<Vec<(String, crate::engine::auth::credential::SecretString)>>,
    pub env_overrides: Vec<(String, crate::engine::auth::credential::SecretString)>,
    pub env_literal: Vec<EnvLiteral>,
    pub seeded_prompt: Option<String>,
    pub headless_prompt_in_argv: bool,
    /// Flag used to deliver `seeded_prompt` in interactive mode. When `None`,
    /// the prompt is appended as a trailing positional argv arg. When `Some`,
    /// it is delivered as `<flag> <text>` (e.g. opencode `--prompt <text>`,
    /// since opencode treats a bare positional as a project directory).
    pub interactive_seed_flag: Option<String>,
    /// From `ContainerOption::HeadlessPromptFlag`.
    pub headless_prompt_flag: Option<String>,
    pub interactive: bool,
    /// The protocol actually selected by the launch builder (never inferred
    /// from the agent name or by parsing stdout).
    pub agent_output_format: AgentOutputFormat,
    /// When `true`, the runtime uses the persistent-piped spawn path (`-i`,
    /// never a PTY) so an ACP JSON-RPC channel can span the whole session.
    /// Default `false` keeps today's PTY/one-shot-piped behaviour unchanged.
    pub acp: bool,
    pub allow_docker: bool,
    pub yolo: YoloMode,
    pub auto: AutoMode,
    pub plan: PlanMode,
    pub working_dir: Option<PathBuf>,
    pub native_resume: Option<(
        Vec<String>,
        crate::engine::agent::definition::ResumePosition,
    )>,
    pub name: Option<ContainerName>,
    pub cpu: Option<CpuLimit>,
    pub memory: Option<MemoryLimit>,
    pub agent_settings: Option<AgentSettings>,
    pub agent_credentials: Vec<(String, String)>,
    /// File-delivered credentials that the refresh monitor will later lease.
    pub refreshable_credentials: Vec<RefreshableCredentialDelivery>,
    /// Who hands out the leases for `refreshable_credentials`, carried
    /// explicitly rather than reached through a process-global (F-38).
    ///
    /// `None` means leases are disabled for this launch: the backends
    /// register nothing and `assert_leases_before_spawn` accepts the empty
    /// lease vec. That is the state a command with `authRefresh.enabled:
    /// false` produces, and it is also the default, so any options value
    /// built without a factory behaves exactly as it did when no monitor was
    /// installed in the process.
    pub lease_factory: Option<crate::engine::credential_refresh::LeaseFactoryHandle>,
    pub disallowed_tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<ModelFlagForm>,
    pub dockerfile_user: Option<String>,
    /// Container labels accumulated from `ContainerOption::Label`, emitted as
    /// one `--label key=value` each (after the hardcoded `awman=true`).
    pub labels: Vec<(String, String)>,
    pub agent_mode_flags: Vec<String>,
    /// From `ContainerOption::AgentFlags`.
    pub agent_flags: Vec<String>,
    pub managed_agent_flags: Vec<String>,
    pub disallowed_tools_flag: Option<String>,
    pub allowed_tools_flag: Option<String>,
    pub remove_on_exit: bool,
    pub system_prompt_file: Option<(PathBuf, PathBuf, String)>,
    pub system_prompt_env_file: Option<(String, PathBuf, PathBuf)>,
    pub system_prompt_inline: Option<(String, String)>,
    /// Keeps temporary mounts (prompt files and private directories) on disk.
    /// Each backend moves these into its execution at spawn.
    pub prompt_files: Vec<PromptFileGuard>,
    pub agent_add_dirs: Vec<(String, PathBuf)>,
    /// From `ContainerOption::StartupGrace`; `None` uses the frontend's.
    pub startup_grace: Option<std::time::Duration>,
    /// From `ContainerOption::FullTranscript`.
    pub full_transcript: bool,
    /// From `ContainerOption::BuiltinLaunch`; read by the builtin backend
    /// only. `None` makes a builtin launch fail closed.
    pub builtin_launch: Option<Box<BuiltinLaunchSettings>>,
}

/// A container runtime control socket lets the agent start a sibling container
/// with the checkout mounted writable, so no mount of one (or of a directory
/// that exposes one) is allowed for a read-only workspace, whatever the
/// overlay's own permission says: a `:ro` socket bind is still a usable API
/// (Linux does not apply a read-only mount to `connect(2)` on a socket).
///
/// Two layers: the runtime endpoints awman can name (well-known defaults
/// plus the host environment's `DOCKER_HOST`/`CONTAINER_HOST`, runtime
/// directory and per-user runtime homes) protect their ancestors lexically;
/// and every mounted directory except the workspace and its Git
/// directories is inspected, without following links, for sockets and other
/// special files ([`reject_special_files`]).
fn reject_runtime_control_mounts(
    overlays: &[OverlaySpec],
    env: Option<&crate::data::config::env::EnvSnapshot>,
    skip_inspection: &[PathBuf],
) -> Result<(), crate::engine::error::EngineError> {
    reject_runtime_control_mounts_at(overlays, &runtime_endpoints(env))?;
    reject_special_files(overlays, skip_inspection, SPECIAL_FILE_SCAN_LIMIT)
}

/// Runtime control endpoints (sockets or the directories that hold them)
/// whose ancestors a read-only launch may not mount.
fn runtime_endpoints(env: Option<&crate::data::config::env::EnvSnapshot>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [
        "/var/run/docker.sock",
        "/run/docker.sock",
        "/run/user",
        "/run/podman",
        "/var/run/podman",
        "/run/containerd",
        "/var/run/containerd",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    let Some(env) = env else {
        return paths;
    };
    for key in ["DOCKER_HOST", "CONTAINER_HOST"] {
        if let Some(path) = env
            .get(key)
            .and_then(|value| value.trim().strip_prefix("unix://"))
            .filter(|path| path.starts_with('/'))
        {
            paths.push(PathBuf::from(path));
        }
    }
    if let Some(dir) = env
        .get("XDG_RUNTIME_DIR")
        .filter(|dir| dir.starts_with('/'))
    {
        paths.push(PathBuf::from(dir));
    }
    if let Some(home) = env.home_dir() {
        for relative in [
            ".docker/run",
            ".colima",
            ".lima",
            ".rd",
            ".orbstack",
            ".local/share/containers",
        ] {
            paths.push(home.join(relative));
        }
    }
    paths
}

/// Most directory entries [`reject_special_files`] visits under one mount
/// source; a larger source is refused as unverifiable.
const SPECIAL_FILE_SCAN_LIMIT: usize = 50_000;

/// Refuse a mount whose source is, or contains, a socket, FIFO or device
/// file. Directories are walked without following symlinks (a link inside
/// the mount resolves in the guest's namespace, where no host socket is
/// mounted). Sources at or under `skip` (the workspace and its Git
/// directories, which awman mounts itself and the read-only check already
/// constrains) are not walked. Fails closed when a directory cannot be read
/// or the source holds more than `limit` entries. A socket created on the
/// host after this check is outside it: do not mount live runtime state.
fn reject_special_files(
    overlays: &[OverlaySpec],
    skip: &[PathBuf],
    limit: usize,
) -> Result<(), crate::engine::error::EngineError> {
    let refuse = |source: &Path, why: String| {
        crate::engine::error::EngineError::Config(format!(
            "read-only workspace cannot mount '{}': {why}",
            source.display()
        ))
    };
    for overlay in overlays {
        let source =
            std::fs::canonicalize(&overlay.host_path).unwrap_or_else(|_| overlay.host_path.clone());
        if skip.iter().any(|root| source.starts_with(root)) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&source) else {
            continue;
        };
        if meta.is_file() {
            continue;
        }
        if !meta.is_dir() {
            return Err(refuse(
                &overlay.host_path,
                "it exposes a container runtime control socket or special file".into(),
            ));
        }
        let mut budget = limit;
        let mut stack = vec![source.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| {
                refuse(
                    &overlay.host_path,
                    format!("awman cannot inspect {} for sockets ({e})", dir.display()),
                )
            })?;
            for entry in entries {
                let entry = entry.map_err(|e| {
                    refuse(
                        &overlay.host_path,
                        format!("awman cannot inspect {} for sockets ({e})", dir.display()),
                    )
                })?;
                if budget == 0 {
                    return Err(refuse(
                        &overlay.host_path,
                        format!(
                            "it holds more than {limit} entries to inspect for sockets; mount a \
                             narrower directory"
                        ),
                    ));
                }
                budget -= 1;
                let kind = entry.file_type().map_err(|e| {
                    refuse(
                        &overlay.host_path,
                        format!("awman cannot inspect {} ({e})", entry.path().display()),
                    )
                })?;
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if !kind.is_file() && !kind.is_symlink() {
                    return Err(refuse(
                        &overlay.host_path,
                        format!(
                            "it contains {}, a socket or special file that may be a container \
                             runtime control endpoint",
                            entry.path().display()
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn reject_runtime_control_mounts_at(
    overlays: &[OverlaySpec],
    sockets: &[PathBuf],
) -> Result<(), crate::engine::error::EngineError> {
    let sockets: Vec<PathBuf> = sockets
        .iter()
        .cloned()
        // Keep the lexical location as well as its target: resolving
        // /var/run/docker.sock to /run/docker.sock must not unprotect /var.
        .flat_map(|path| {
            let resolved = std::fs::canonicalize(&path).ok();
            std::iter::once(path).chain(resolved)
        })
        .collect();
    for overlay in overlays {
        let source =
            std::fs::canonicalize(&overlay.host_path).unwrap_or_else(|_| overlay.host_path.clone());
        let special = std::fs::metadata(&source)
            .map(|m| !m.is_dir() && !m.is_file())
            .unwrap_or(false);
        if special
            || sockets
                .iter()
                .any(|s| s.starts_with(&source) || s.starts_with(&overlay.host_path))
        {
            return Err(crate::engine::error::EngineError::Config(format!(
                "read-only workspace cannot mount '{}': it exposes a container \
                 runtime control socket or special file",
                overlay.host_path.display()
            )));
        }
    }
    Ok(())
}

impl ResolvedContainerOptions {
    /// Render the provider argv with the one runtime-neutral argv builder.
    pub fn agent_argv(&self) -> Result<Vec<String>, crate::engine::error::EngineError> {
        Ok(super::agent_argv::AgentArgv::from_options(self)?
            .append_to(self.entrypoint.clone().map(|e| e.0).unwrap_or_default()))
    }

    /// Final backend check. Reuse the lease overlay enforcement, but refuse
    /// any correction here: options reaching a spawn must already be safe.
    pub(crate) fn validate_read_only_workspace(
        &self,
    ) -> Result<(), crate::engine::error::EngineError> {
        use crate::data::workspace::{SCRATCH_TMPFS_PATH, WORKSPACE_GUEST_PATH};
        use crate::engine::error::EngineError;
        for path in &self.tmpfs {
            if !crate::data::workspace::is_normalized_guest_path(path) {
                return Err(EngineError::Config(
                    "tmpfs guest path must be absolute and normalized".into(),
                ));
            }
        }
        if !self.read_only_workspace {
            return Ok(());
        }
        if self.allow_docker {
            return Err(EngineError::Config(
                "read-only workspace cannot expose the host Docker socket".into(),
            ));
        }
        let mut overlays = self.overlays.clone();
        if let Some(settings) = &self.agent_settings {
            overlays.extend(settings.overlays.clone());
        }
        if let Some((host, guest, _)) = &self.system_prompt_file {
            overlays.push(OverlaySpec {
                host_path: host.clone(),
                container_path: guest.clone(),
                permission: OverlayPermission::ReadOnly,
            });
        }
        if let Some((_, host, guest)) = &self.system_prompt_env_file {
            overlays.push(OverlaySpec {
                host_path: host.clone(),
                container_path: guest.clone(),
                permission: OverlayPermission::ReadOnly,
            });
        }
        let workspaces: Vec<_> = overlays
            .iter()
            .filter(|o| o.container_path == Path::new(WORKSPACE_GUEST_PATH))
            .collect();
        if workspaces.len() != 1 || workspaces[0].permission != OverlayPermission::ReadOnly {
            return Err(EngineError::Config(
                "read-only workspace requires exactly one read-only /workspace mount".into(),
            ));
        }
        if self
            .tmpfs
            .iter()
            .filter(|p| *p == Path::new(SCRATCH_TMPFS_PATH))
            .count()
            != 1
            || self.tmpfs.iter().any(|p| {
                p.starts_with(WORKSPACE_GUEST_PATH)
                    || Path::new(WORKSPACE_GUEST_PATH).starts_with(p)
            })
            || overlays
                .iter()
                .any(|o| Path::new(SCRATCH_TMPFS_PATH).starts_with(&o.container_path))
        {
            return Err(EngineError::Config(
                "read-only workspace requires an unshadowed writable tmpfs at /tmp".into(),
            ));
        }
        let workspace = workspaces[0].host_path.clone();
        let mut skip_inspection =
            vec![std::fs::canonicalize(&workspace).unwrap_or_else(|_| workspace.clone())];
        skip_inspection.extend(crate::engine::worker::lease_workspace::workspace_git_dirs(
            &workspace,
        )?);
        reject_runtime_control_mounts(&overlays, self.host_env.as_ref(), &skip_inspection)?;
        let mut checked: Vec<_> = overlays
            .iter()
            .cloned()
            .map(ContainerOption::Overlay)
            .collect();
        crate::engine::worker::lease_workspace::validate_workspace_mounts(
            &mut checked,
            &workspace,
            true,
            None,
            None,
            &[],
        )?;
        for (original, checked) in overlays.iter().zip(checked) {
            let ContainerOption::Overlay(checked) = checked else {
                unreachable!()
            };
            if original.permission != checked.permission {
                return Err(EngineError::Config(
                    "writable overlay exposes a read-only workspace or its Git directory".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn resolve(
        options: impl IntoIterator<Item = ContainerOption>,
    ) -> Result<Self, ResolveError> {
        Self::resolve_with_env(options, None)
    }

    pub fn resolve_in(
        options: impl IntoIterator<Item = ContainerOption>,
        env: &crate::data::config::env::EnvSnapshot,
    ) -> Result<Self, ResolveError> {
        Self::resolve_with_env(options, Some(env.clone()))
    }

    fn resolve_with_env(
        options: impl IntoIterator<Item = ContainerOption>,
        env: Option<crate::data::config::env::EnvSnapshot>,
    ) -> Result<Self, ResolveError> {
        let mut r = Self {
            host_env: env,
            yolo: YoloMode::Disabled,
            auto: AutoMode::Disabled,
            plan: PlanMode::Disabled,
            remove_on_exit: true,
            ..Self::default()
        };
        for opt in options {
            r.ingest(opt)?;
        }
        // Part A: drop agent_credentials that duplicate a service already covered
        // by a harness-declared env var.  Applies to ALL container runtimes.
        // Production callers pass the real host-env lookup; tests inject a
        // hermetic closure to avoid mutating process-global state.
        let lookup = |name: &str| {
            // A private caller override is already resolved, even if the
            // host has no value. Only presence matters to credential dedup;
            // do not copy the secret into this ordinary String lookup.
            if r.env_overrides
                .iter()
                .any(|(key, value)| key == name && !value.expose().is_empty())
            {
                return Some("present".into());
            }
            r.host_env.as_ref().map_or_else(
                || crate::data::config::env::host_var(name),
                |env| env.host_payload(name),
            )
        };
        dedup_credentials_by_declared_env(
            &mut r.agent_credentials,
            &r.env_passthrough,
            &r.env_literal,
            &lookup,
        );
        dedup_refreshable_by_declared_env(
            &mut r.refreshable_credentials,
            &r.env_passthrough,
            &r.env_literal,
            &lookup,
        )?;
        r.register_output_secrets();
        r.validate()?;
        // Render through the existing shared argv tail on every container
        // backend. ACP and interactive launches never receive structured flags.
        if !r.interactive && !r.acp {
            r.agent_mode_flags.extend(match r.agent_output_format {
                AgentOutputFormat::OpenCodeJson | AgentOutputFormat::OpenCodeV2Json => {
                    vec!["--format".into(), "json".into()]
                }
                AgentOutputFormat::CopilotJson => vec!["--output-format".into(), "json".into()],
                AgentOutputFormat::ClineJson => vec!["--json".into()],
                AgentOutputFormat::AntigravityStreamJson => {
                    vec!["--output-format".into(), "stream-json".into()]
                }
                AgentOutputFormat::Text => Vec::new(),
                AgentOutputFormat::ClaudeStreamJson => vec![
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                ],
                AgentOutputFormat::CodexJson => vec!["--json".into()],
                AgentOutputFormat::CursorStreamJson => {
                    vec!["--output-format".into(), "stream-json".into()]
                }
            });
        } else {
            r.agent_output_format = AgentOutputFormat::Text;
        }
        Ok(r)
    }

    fn ingest(&mut self, opt: ContainerOption) -> Result<(), ResolveError> {
        match opt {
            ContainerOption::AgentIdentity { agent, model } => {
                self.agent_identity = Some((agent, model))
            }
            ContainerOption::ProgramStatus(config) => self.program_status = config,
            ContainerOption::Image(v) => self.image = Some(v),
            ContainerOption::Entrypoint(v) => self.entrypoint = Some(v),
            ContainerOption::Overlay(v) => self.overlays.push(v),
            ContainerOption::Tmpfs(v) => self.tmpfs.push(v),
            ContainerOption::ReadOnlyWorkspace(v) => self.read_only_workspace |= v,
            ContainerOption::TemporaryFileMount {
                host_path,
                container_path,
                guard,
            } => {
                self.overlays.push(OverlaySpec {
                    host_path,
                    container_path,
                    permission: OverlayPermission::ReadOnly,
                });
                self.prompt_files.push(guard);
            }
            ContainerOption::TemporaryDirectoryMount {
                container_path,
                env_var,
                guard,
            } => {
                self.overlays.push(OverlaySpec {
                    host_path: guard.host_path().to_path_buf(),
                    container_path: container_path.clone(),
                    permission: OverlayPermission::ReadWrite,
                });
                if let Some(key) = env_var {
                    self.env_literal.push(EnvLiteral {
                        key,
                        value: container_path.to_string_lossy().into_owned(),
                    });
                }
                self.prompt_files.push(guard);
            }
            ContainerOption::EnvPassthrough(v) => self.env_passthrough.push(v),
            ContainerOption::EnvOverride { key, value } => self.env_overrides.push((key, value)),
            ContainerOption::EnvLiteral(v) => self.env_literal.push(v),
            ContainerOption::NativeResume { tokens, position } => {
                self.native_resume = Some((tokens, position))
            }
            ContainerOption::SeededPrompt(v) => self.seeded_prompt = Some(v),
            ContainerOption::HeadlessPromptInArgv(v) => self.headless_prompt_in_argv = v,
            ContainerOption::InteractiveSeedFlag(v) => self.interactive_seed_flag = Some(v),
            ContainerOption::HeadlessPromptFlag(v) => self.headless_prompt_flag = Some(v),
            ContainerOption::Interactive(v) => self.interactive = v,
            ContainerOption::AgentOutputFormat(v) => self.agent_output_format = v,
            ContainerOption::Acp(v) => self.acp = v,
            ContainerOption::AllowDocker(v) => self.allow_docker = v,
            ContainerOption::Yolo(v) => self.yolo = v,
            ContainerOption::Auto(v) => self.auto = v,
            ContainerOption::Plan(v) => self.plan = v,
            ContainerOption::WorkingDir(v) => self.working_dir = Some(v),
            ContainerOption::Name(v) => self.name = Some(v),
            ContainerOption::Cpu(v) => self.cpu = Some(v),
            ContainerOption::Memory(v) => self.memory = Some(v),
            ContainerOption::AgentSettingsPassthrough(v) => self.agent_settings = Some(v),
            ContainerOption::AgentCredentials { env_vars } => {
                self.agent_credentials.extend(env_vars);
            }
            ContainerOption::RefreshableCredential(credential) => {
                self.refreshable_credentials.push(credential);
            }
            ContainerOption::CredentialLeaseFactory(factory) => {
                self.lease_factory = Some(factory);
            }
            ContainerOption::DisallowedTools(v) => self.disallowed_tools.extend(v),
            ContainerOption::AllowedTools(v) => self.allowed_tools.extend(v),
            ContainerOption::Model { flag } => self.model = Some(flag),
            ContainerOption::DockerfileUser(v) => self.dockerfile_user = Some(v),
            ContainerOption::Label { key, value } => self.labels.push((key, value)),
            ContainerOption::AgentModeFlags(v) => self.agent_mode_flags.extend(v),
            ContainerOption::AgentFlags(v) => self.agent_flags.extend(v),
            ContainerOption::ManagedAgentFlags(v) => self.managed_agent_flags.extend(v),
            ContainerOption::DisallowedToolsFlag(v) => self.disallowed_tools_flag = Some(v),
            ContainerOption::AllowedToolsFlag(v) => self.allowed_tools_flag = Some(v),
            ContainerOption::KeepContainer => self.remove_on_exit = false,
            ContainerOption::StartupGrace(v) => self.startup_grace = Some(v),
            ContainerOption::FullTranscript => self.full_transcript = true,
            ContainerOption::BuiltinLaunch(v) => self.builtin_launch = Some(Box::new(v)),
            ContainerOption::SystemPromptFile {
                host_path,
                container_path,
                flag,
                guard,
            } => {
                self.prompt_files.push(guard);
                self.system_prompt_file = Some((host_path, container_path, flag));
            }
            ContainerOption::SystemPromptEnvFile {
                env_var,
                host_path,
                container_path,
                guard,
            } => {
                self.prompt_files.push(guard);
                self.system_prompt_env_file = Some((env_var, host_path, container_path));
            }
            ContainerOption::SystemPromptInline { flag, text } => {
                self.system_prompt_inline = Some((flag, text));
            }
            ContainerOption::AgentAddDir {
                flag,
                container_path,
            } => {
                self.agent_add_dirs.push((flag, container_path));
            }
        }
        Ok(())
    }

    /// Freeze grants before launch so later daemon environment updates cannot
    /// replace the value whose output protection was registered.
    pub(crate) fn register_output_secrets(&mut self) {
        if self.resolved_env_grants.is_none() {
            self.resolved_env_grants = Some(
                self.env_passthrough
                    .iter()
                    .filter_map(|var| {
                        self.host_env
                            .as_ref()
                            .map_or_else(
                                || crate::data::config::env::host_var(&var.0),
                                |env| env.host_payload(&var.0),
                            )
                            .map(|value| {
                                (
                                    var.0.clone(),
                                    crate::engine::auth::credential::SecretString::new(value),
                                )
                            })
                    })
                    .collect(),
            );
        }
        for (_, value) in self
            .resolved_env_grants
            .iter()
            .flatten()
            .chain(self.env_overrides.iter())
        {
            self.output_redaction.add(value.clone());
        }
        let mut names: Vec<_> = self
            .resolved_env_grants
            .iter()
            .flatten()
            .map(|(key, _)| key.clone())
            .chain(self.env_overrides.iter().map(|(key, _)| key.clone()))
            .chain(self.agent_credentials.iter().map(|(key, _)| key.clone()))
            .collect();
        names.sort();
        names.dedup();
        self.labels
            .retain(|(key, _)| key != OUTPUT_SECRET_ENV_LABEL);
        if !names.is_empty() {
            self.labels
                .push((OUTPUT_SECRET_ENV_LABEL.into(), names.join(",")));
        }
        for (_, value) in &self.agent_credentials {
            self.output_redaction
                .add(crate::engine::auth::credential::SecretString::new(value));
        }
    }

    pub(crate) fn validate_caller_env(&self) -> Result<(), ResolveError> {
        for (name, _) in &self.env_overrides {
            super::host_env_denylist::validate_caller_env_name(name)
                .map_err(ResolveError::Conflict)?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ResolveError> {
        self.validate_caller_env()?;
        // Yolo + Plan are mutually exclusive — yolo grants permissions, plan
        // forbids them.
        if matches!(self.yolo, YoloMode::Enabled) && matches!(self.plan, PlanMode::Enabled) {
            return Err(ResolveError::Conflict(
                "yolo and plan modes are mutually exclusive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("conflicting container options: {0}")]
    Conflict(String),
    #[error("could not suppress a superseded credential: {0}")]
    CredentialSuppression(String),
}

impl From<ResolveError> for crate::engine::error::EngineError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::Conflict(msg) => {
                crate::engine::error::EngineError::ConflictingOptions(msg)
            }
            ResolveError::CredentialSuppression(msg) => {
                crate::engine::error::EngineError::ConflictingOptions(msg)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod workspace_access_tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    pub(crate) fn options(sandbox: &TestSandbox) -> ResolvedContainerOptions {
        let output = sandbox.root().join("output");
        let context = sandbox.root().join("context");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::create_dir_all(&context).unwrap();
        ResolvedContainerOptions {
            image: Some(ImageRef::new("read-only-test")),
            entrypoint: Some(Entrypoint::new(["review-agent"])),
            read_only_workspace: true,
            tmpfs: vec!["/tmp".into()],
            overlays: vec![
                OverlaySpec {
                    host_path: sandbox.work(),
                    container_path: "/workspace".into(),
                    permission: OverlayPermission::ReadOnly,
                },
                OverlaySpec {
                    host_path: output,
                    container_path: "/awman/output".into(),
                    permission: OverlayPermission::ReadWrite,
                },
                OverlaySpec {
                    host_path: context,
                    container_path: "/awman/context/repo".into(),
                    permission: OverlayPermission::ReadWrite,
                },
            ],
            host_env: Some(crate::data::config::env::EnvSnapshot::from_full_env(
                Default::default(),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn read_only_intent_cannot_be_cleared_by_a_later_option() {
        let options = ResolvedContainerOptions::resolve([
            ContainerOption::ReadOnlyWorkspace(true),
            ContainerOption::ReadOnlyWorkspace(false),
            ContainerOption::Tmpfs("/tmp".into()),
        ])
        .unwrap();
        assert!(options.read_only_workspace);
        assert_eq!(options.tmpfs, [PathBuf::from("/tmp")]);
    }

    #[test]
    fn final_check_refuses_writable_aliases_missing_tmpfs_and_docker_socket() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::container::options::workspace_access_tests::final_check_refuses_writable_aliases_missing_tmpfs_and_docker_socket"));
        let valid = options(&sandbox);
        valid.validate_read_only_workspace().unwrap();
        let mut writable = valid.clone();
        writable.overlays[0].permission = OverlayPermission::ReadWrite;
        assert!(writable.validate_read_only_workspace().is_err());
        let mut alias = valid.clone();
        alias.overlays.push(OverlaySpec {
            host_path: sandbox.work(),
            container_path: "/elsewhere".into(),
            permission: OverlayPermission::ReadWrite,
        });
        assert!(alias.validate_read_only_workspace().is_err());
        let mut missing_tmpfs = valid.clone();
        missing_tmpfs.tmpfs.clear();
        assert!(missing_tmpfs.validate_read_only_workspace().is_err());
        let mut shadowed_tmpfs = valid.clone();
        shadowed_tmpfs.overlays.push(OverlaySpec {
            host_path: sandbox.tmp(),
            container_path: "/tmp".into(),
            permission: OverlayPermission::ReadWrite,
        });
        assert!(shadowed_tmpfs.validate_read_only_workspace().is_err());
        let mut socket = valid;
        socket.allow_docker = true;
        assert!(socket.validate_read_only_workspace().is_err());
    }

    #[test]
    fn final_check_refuses_socket_and_parent_directory_overlays() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::container::options::workspace_access_tests::final_check_refuses_socket_and_parent_directory_overlays"));
        let overlay = |p: &str, perm| OverlaySpec {
            host_path: p.into(),
            container_path: "/run/engine".into(),
            permission: perm,
        };
        for p in ["/var/run", "/run", "/var", "/"] {
            for perm in [OverlayPermission::ReadOnly, OverlayPermission::ReadWrite] {
                assert!(
                    super::reject_runtime_control_mounts(&[overlay(p, perm)], None, &[]).is_err(),
                    "{p}"
                );
            }
        }
        // Unix device files are refused regardless of path.
        #[cfg(unix)]
        assert!(super::reject_runtime_control_mounts(
            &[overlay("/dev/null", OverlayPermission::ReadOnly)],
            None,
            &[]
        )
        .is_err());
        let dir = sandbox.tmp();
        // Ordinary directories remain accepted.
        assert!(super::reject_runtime_control_mounts(
            &[overlay(dir.to_str().unwrap(), OverlayPermission::ReadWrite)],
            None,
            &[]
        )
        .is_ok());
    }

    /// WI 0070 review finding 1: a runtime socket at a non-default location
    /// (or a special file nested inside an otherwise ordinary directory)
    /// must not reach a read-only launch through its parent, whatever the
    /// mount permission.
    /// hygiene-allow(H4): Socket path is leased by TestSandbox Ports and the listener is dropped and its file removed before the test ends.
    #[cfg(unix)]
    #[test]
    fn final_check_refuses_parents_of_non_default_runtime_sockets() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::container::options::workspace_access_tests::final_check_refuses_parents_of_non_default_runtime_sockets"));
        let socket = sandbox.ports().unix_socket("d.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let socket_dir = socket.parent().unwrap().to_path_buf();
        let nested = sandbox.root().join("rt/nested");
        std::fs::create_dir_all(&nested).unwrap();
        nix::unistd::mkfifo(&nested.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        for (source, permission) in [
            (socket_dir.clone(), OverlayPermission::ReadOnly),
            (socket_dir.clone(), OverlayPermission::ReadWrite),
            (sandbox.root().join("rt"), OverlayPermission::ReadOnly),
            (nested.clone(), OverlayPermission::ReadOnly),
        ] {
            let mut options = options(&sandbox);
            options.overlays.push(OverlaySpec {
                host_path: source.clone(),
                container_path: "/control".into(),
                permission,
            });
            let err = options.validate_read_only_workspace().unwrap_err();
            assert!(
                err.to_string().contains("socket"),
                "{}: {err}",
                source.display()
            );
        }
        // The same directories without the special files are ordinary mounts.
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_file(nested.join("pipe")).unwrap();
        for source in [socket_dir, sandbox.root().join("rt")] {
            let mut options = options(&sandbox);
            options.overlays.push(OverlaySpec {
                host_path: source,
                container_path: "/control".into(),
                permission: OverlayPermission::ReadOnly,
            });
            options.validate_read_only_workspace().unwrap();
        }
    }

    /// The configured endpoint (`DOCKER_HOST`) protects its ancestors even
    /// when nothing is listening there yet.
    #[test]
    fn final_check_protects_the_configured_runtime_endpoint() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::container::options::workspace_access_tests::final_check_protects_the_configured_runtime_endpoint"));
        let endpoint_dir = sandbox.root().join("engine-home");
        std::fs::create_dir_all(&endpoint_dir).unwrap();
        let mut options = options(&sandbox);
        options.host_env = Some(crate::data::config::env::EnvSnapshot::from_full_env(
            [(
                "DOCKER_HOST".to_string(),
                format!("unix://{}/engine.sock", endpoint_dir.display()),
            )]
            .into_iter()
            .collect(),
        ));
        options.validate_read_only_workspace().unwrap();
        options.overlays.push(OverlaySpec {
            host_path: endpoint_dir,
            container_path: "/engine".into(),
            permission: OverlayPermission::ReadOnly,
        });
        assert!(options.validate_read_only_workspace().is_err());
    }

    /// A source too large to inspect fails closed rather than unchecked.
    #[test]
    fn special_file_inspection_fails_closed_past_its_limit() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::container::options::workspace_access_tests::special_file_inspection_fails_closed_past_its_limit"));
        let dir = sandbox.root().join("many");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            std::fs::write(dir.join(format!("f{i}")), b"").unwrap();
        }
        let overlay = OverlaySpec {
            host_path: dir.clone(),
            container_path: "/many".into(),
            permission: OverlayPermission::ReadOnly,
        };
        super::reject_special_files(std::slice::from_ref(&overlay), &[], 4).unwrap();
        assert!(super::reject_special_files(std::slice::from_ref(&overlay), &[], 3).is_err());
        // A skipped root (the workspace) is not walked.
        super::reject_special_files(&[overlay], &[std::fs::canonicalize(&dir).unwrap()], 0)
            .unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn final_check_keeps_symlinked_socket_parents_protected() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::container::options::workspace_access_tests::final_check_keeps_symlinked_socket_parents_protected"));
        let var = sandbox.root().join("var");
        let run = sandbox.root().join("run");
        std::fs::create_dir_all(&var).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        std::os::unix::fs::symlink(&run, var.join("run")).unwrap();
        // A known endpoint must protect its parents even without a live daemon.
        std::fs::write(run.join("docker.sock"), b"").unwrap();
        let sockets = [var.join("run/docker.sock")];
        for source in [&var, &run, &var.join("run")] {
            for permission in [OverlayPermission::ReadOnly, OverlayPermission::ReadWrite] {
                let overlay = OverlaySpec {
                    host_path: source.clone(),
                    container_path: "/run/engine".into(),
                    permission,
                };
                assert!(
                    super::reject_runtime_control_mounts_at(&[overlay], &sockets).is_err(),
                    "{}",
                    source.display()
                );
            }
        }
    }
}
