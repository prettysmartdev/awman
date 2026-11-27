//! `DSbxBackend` — the Docker Sandboxes (`sbx`) driver.
//!
//! Lifecycle (`create`/`stop`), listing (`sbx ls --json`) and the
//! interactive launch ([`run_interactive`]), which follows the contract's
//! §6.2 sequence for a fresh, a running and a stopped sandbox alike:
//!
//! 0. validate with no side effects (matrix, undeliverable options,
//!    outside-workspace overlays, CLI admission);
//! 1. emit and load the v3 kit;
//! 2. take the per-sandbox launch lock;
//! 3. list sandboxes; create a missing one, or check the identity record of
//!    an existing one (a mismatch stops here and changes nothing);
//! 4. refuse a reused sandbox that reports an active session;
//! 5. register sandbox-scoped credentials;
//! 6. apply and verify this launch's policy in the guest (`awman-apply`);
//!    the agent is never started unless the apply acknowledged this launch;
//! 7. probe the agent CLI major (advisory except a confirmed wrong major);
//! 8. spawn `sbx run`, holding the lock until the agent exits.
//!
//! Every headless `sbx` invocation goes through [`super::spawn`]; every argv
//! is built in [`super::cli`]. Nothing here falls back to another runtime,
//! retries with an older kit, or removes a sandbox.

use chrono::Utc;

use crate::data::fs::SandboxKitPaths;
use crate::data::session::AgentHandle;
use crate::engine::agent::definition::SbxTailRequest;
use crate::engine::agent::first_party::first_party;
use crate::engine::agent::sbx_contract::{
    apply_acknowledged, kit_dir_v3, sbx_route, ApplyDocument, EmittedKit, SbxCliVersion,
    SbxDelivery, SbxRoute, APPLY_EXIT_OK, APPLY_EXIT_STALE_KIT, CLAUDE_MANAGED_SETTINGS,
    EXEC_EXIT_COMMAND_MISSING,
};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentStats, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::SandboxBackend;
use crate::engine::sandbox::dsbx::auth;
use crate::engine::sandbox::dsbx::cli;
use crate::engine::sandbox::dsbx::identity::{self, IntendedSandbox, ReuseDecision};
use crate::engine::sandbox::dsbx::io_bridge;
use crate::engine::sandbox::dsbx::lock::SandboxLock;
use crate::engine::sandbox::dsbx::session_config;
use crate::engine::sandbox::dsbx::spawn::{announce, SbxCommand, SBX_BIN, SBX_NOT_FOUND};
use crate::engine::sandbox::naming::sandbox_name_for;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Deadline of the per-launch policy apply (contract §6.2 step 6). On a
/// stopped sandbox the same call boots it.
const APPLY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Default)]
pub(in crate::engine::sandbox) struct DSbxBackend {
    env: crate::data::config::env::EnvSnapshot,
}

impl DSbxBackend {
    pub(in crate::engine::sandbox) fn with_env(env: crate::data::config::env::EnvSnapshot) -> Self {
        Self { env }
    }
}

impl SandboxBackend for DSbxBackend {
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        // `sbx stop` pauses the VM and preserves its persistent volume.
        // Best-effort: a non-zero exit (already stopped) is not an error.
        let _ = SbxCommand::new(cli::stop_argv(&handle.name))
            .in_env(&self.env)
            .run_quiet();
        Ok(())
    }

    fn list_running(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(list_sandbox_names(&self.env)?
            .into_iter()
            .filter(|n| n.starts_with("awman-"))
            .map(|name| AgentHandle {
                id: name.clone(),
                image_tag: String::new(),
                name,
                started_at: Utc::now(),
            })
            .collect())
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        // Sandboxes run the agent directly in the VM (not as a Docker
        // container), so there are no per-resource metrics to poll.
        Ok(AgentStats {
            name: handle.name.clone(),
            cpu_percent: 0.0,
            memory_mb: 0.0,
        })
    }

    fn name(&self) -> &'static str {
        "docker-sbx-experimental"
    }

    fn cli_binary(&self) -> &'static str {
        SBX_BIN
    }
}

// ─── Shared steps (launch and `ready`) ──────────────────────────────────────

fn warn(sink: &mut dyn crate::data::message::UserMessageSink, text: String) {
    sink.write_message(crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Warning,
        text,
    });
}

/// The supported SBX route of a first-party agent, or the refusal. Runs the
/// definition's own sandbox admission so the matrix and `validate_run`
/// cannot disagree; custom agents never have a route.
pub(super) fn supported_route(agent: &str) -> Result<SbxRoute, EngineError> {
    if agent.is_empty() {
        return Err(EngineError::Sandbox(
            "sandbox launch requires an agent id".into(),
        ));
    }
    let Some(def) = first_party(agent) else {
        return Err(EngineError::CustomAgentRuntimeUnsupported {
            name: agent.to_string(),
            runtime: "docker-sbx-experimental".to_string(),
        });
    };
    def.validate_run(
        &Default::default(),
        crate::engine::agent::definition::RunParadigm::Sandbox,
    )?;
    let route = sbx_route(agent).ok_or_else(|| EngineError::CustomAgentRuntimeUnsupported {
        name: agent.to_string(),
        runtime: "docker-sbx-experimental".to_string(),
    })?;
    if let crate::engine::agent::sbx_contract::SbxDisposition::Refused { reason } =
        route.disposition
    {
        return Err(EngineError::Config(format!(
            "agent '{agent}' {reason}; select a container runtime"
        )));
    }
    Ok(route)
}

/// Every sandbox name `sbx ls --json` reports. A failed or unreadable
/// listing is an error: deciding reuse from a guess is not allowed.
pub(super) fn list_sandbox_names(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Result<Vec<String>, EngineError> {
    let out = SbxCommand::new(cli::ls_argv())
        .in_env(snapshot)
        .run_quiet()?;
    if !out.success() {
        return Err(EngineError::Sandbox(format!(
            "`{SBX_BIN} {}` exited with code {}: {}",
            cli::ls_argv().join(" "),
            out.exit_code,
            out.stderr.trim()
        )));
    }
    cli::parse_ls_json(&out.stdout).ok_or_else(|| {
        EngineError::Sandbox(format!(
            "`{SBX_BIN} {}` did not print a JSON sandbox list",
            cli::ls_argv().join(" ")
        ))
    })
}

/// Emit the agent's v3 kit into its kits directory and load it back. The
/// load verifies the manifest and that the baked kit id matches the files.
pub(super) fn emit_and_load_kit(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
) -> Result<EmittedKit, EngineError> {
    let dir = kit_dir(snapshot, agent)?;
    super::kit::DSbxKitEmitter::new().emit_for_agent(agent, &dir)?;
    Ok(EmittedKit::load(&dir)?)
}

/// `<config home>/kits/v3/<handle>` for `agent`.
pub(super) fn kit_dir(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
) -> Result<std::path::PathBuf, EngineError> {
    let root =
        SandboxKitPaths::at_home(crate::data::fs::data_paths::config_home(snapshot)?).kits_root();
    kit_dir_v3(&root, agent)
        .ok_or_else(|| EngineError::Sandbox(format!("agent '{agent}' has no Docker Sandboxes kit")))
}

/// Resolve the deterministic sandbox name. Uses a caller-supplied name when
/// present, otherwise derives `awman-<worktree-hash>-<agent>`.
fn resolve_sandbox_name(opts: &ResolvedSandboxOptions) -> String {
    opts.sandbox_name
        .clone()
        .unwrap_or_else(|| sandbox_name_for(&opts.workspace_dir, &opts.agent_id))
}

/// Layer 2 admission of resolved options against the route: everything the
/// route cannot deliver is refused here, before any `sbx` call, including
/// for callers that build `ResolvedSandboxOptions` directly.
fn refuse_undeliverable(
    route: &SbxRoute,
    options: &ResolvedSandboxOptions,
) -> Result<(), EngineError> {
    let def =
        first_party(route.agent).ok_or_else(|| EngineError::CustomAgentRuntimeUnsupported {
            name: route.agent.to_string(),
            runtime: "docker-sbx-experimental".to_string(),
        })?;
    if options
        .seeded_prompt
        .as_deref()
        .is_some_and(|p| p.trim_start().starts_with('-'))
    {
        return Err(EngineError::Config(
            "prompts must not start with '-' on Docker Sandboxes; prefix the prompt with a \
             sentence"
                .into(),
        ));
    }
    let refusals = [
        (
            !options.mode_flags.is_empty() && route.mode != SbxDelivery::Argv,
            "the requested permission mode",
        ),
        (
            options.permission_mode.is_some() && route.mode != SbxDelivery::ManagedFile,
            "the requested permission mode",
        ),
        (
            !options.agent_flags.is_empty() && route.agent_flags != SbxDelivery::Argv,
            "agentFlags",
        ),
        (
            options.model.is_some() && route.model == SbxDelivery::Refused,
            "--model",
        ),
        (
            options.system_prompt_inline.is_some() && route.system_prompt == SbxDelivery::Refused,
            "the context system prompt",
        ),
        (
            options.system_prompt_file.is_some() || options.system_prompt_env_file.is_some(),
            "a file-mounted system prompt",
        ),
        (
            options.entrypoint_override.is_some(),
            "an entrypoint override",
        ),
        (
            !options.agent_settings.is_empty()
                && route.managed_file(CLAUDE_MANAGED_SETTINGS).is_none(),
            "agentSettings",
        ),
    ];
    if let Some((_, option)) = refusals.iter().find(|(refused, _)| *refused) {
        return Err(def.sbx_option_refusal(option));
    }
    Ok(())
}

/// Step 0 without the CLI: everything that can be refused from the options
/// alone. Returns the route.
fn validate_launch(options: &ResolvedSandboxOptions) -> Result<SbxRoute, EngineError> {
    options.validate_tool_restrictions()?;
    let route = supported_route(&options.agent_id)?;
    refuse_undeliverable(&route, options)?;
    if options.workspace_dir.as_os_str().is_empty() {
        return Err(EngineError::Sandbox(
            "a Docker Sandboxes launch requires the approved workspace directory".into(),
        ));
    }
    // The VM sees only the workspace; silently launching without the
    // overlay would surprise the user.
    for overlay in &options.extra_overlays {
        if !overlay.host_path.starts_with(&options.workspace_dir) {
            return Err(EngineError::Sandbox(format!(
                "overlay '{}' is outside the workspace and cannot be mounted into the sandbox \
                 VM; move it inside the workspace or remove the overlay when using \
                 docker-sbx-experimental",
                overlay.host_path.display()
            )));
        }
    }
    Ok(route)
}

// ─── Interactive launch ─────────────────────────────────────────────────────

/// Kit source of a launch. Production emits the kit; tests load a prepared
/// directory so they need no real templates.
type KitLoader<'a> =
    &'a dyn Fn(&crate::data::config::env::EnvSnapshot, &str) -> Result<EmittedKit, EngineError>;

/// Perform the full launch (contract §6.2). This is the sandbox tier's
/// equivalent of `DockerContainerInstance::run_with_frontend`, reached from
/// `SandboxRuntime`'s `AgentInstance`.
pub(in crate::engine::sandbox) fn run_interactive(
    options: ResolvedSandboxOptions,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    run_with_kit_loader(options, frontend, &emit_and_load_kit)
}

fn run_with_kit_loader(
    options: ResolvedSandboxOptions,
    mut frontend: Box<dyn AgentFrontend>,
    load_kit: KitLoader<'_>,
) -> Result<AgentExecution, EngineError> {
    let snapshot = options
        .host_env
        .clone()
        .unwrap_or_else(crate::engine::host_cli::process_env);

    // 0. Validation, then CLI admission before any other `sbx` call.
    let route = validate_launch(&options)?;
    let cli_version = cli::admit_installed_cli(&snapshot)?;
    let def = first_party(route.agent).ok_or_else(|| {
        EngineError::Sandbox(format!("agent '{}' is not first-party", route.agent))
    })?;
    let tail = def.sbx_launch_tail(&SbxTailRequest {
        interactive: options.interactive,
        model: options.model.as_ref(),
        mode_flags: &options.mode_flags,
        agent_flags: &options.agent_flags,
        system_prompt: options
            .system_prompt_inline
            .as_ref()
            .map(|(flag, text)| (flag.as_str(), text.as_str())),
        seeded_prompt: options.seeded_prompt.as_deref(),
    });

    // 1. Kit.
    let kit = load_kit(&snapshot, route.agent)?;
    if kit.manifest.agent != route.agent {
        return Err(EngineError::Sandbox(format!(
            "kit {} belongs to '{}', not '{}'; run `awman ready` again",
            kit.dir.display(),
            kit.manifest.agent,
            route.agent
        )));
    }
    let data_home = crate::data::fs::data_paths::data_home(&snapshot)?;
    let name = resolve_sandbox_name(&options);

    // Warnings about options that are noted, not delivered.
    if let Some(notice) = session_config::host_terminal_notice(&options) {
        warn(&mut *frontend, notice);
    }
    if options.cpu_limit.is_some() {
        warn(
            &mut *frontend,
            "sbx: CPU limits are not supported by Docker Sandboxes; ignoring.".to_string(),
        );
    }
    if options.system_prompt_inline.is_some() && route.system_prompt == SbxDelivery::NotedOnly {
        warn(
            &mut *frontend,
            format!(
                "sbx: the context system prompt cannot be delivered to '{}' on Docker \
                 Sandboxes; continuing without it.",
                route.agent
            ),
        );
    }
    for note in &options.unsupported_notes {
        warn(&mut *frontend, format!("sbx: {note}"));
    }

    // 2. Lock: held until the agent exits, so concurrent launches into the
    //    same sandbox cannot apply over each other.
    let lock = SandboxLock::acquire(&data_home, &name)?;

    // 3. Existence and identity.
    let existing = list_sandbox_names(&snapshot)?;
    verify_workspace(&options)?;
    let exists = existing.contains(&name);
    let intended = IntendedSandbox::new(&name, &route, &kit, &options.workspace_dir)?;
    let record = identity::load(&data_home, &name);
    match identity::decide(exists, record.as_ref(), &intended) {
        ReuseDecision::Create { discard_record } => {
            if discard_record {
                identity::discard(&data_home, &name)?;
            }
            create_sandbox(
                &snapshot,
                &data_home,
                &intended,
                &kit,
                cli_version,
                &options,
                &mut *frontend,
            )?;
            if route.is_owned_workload() {
                if let Some(service) = route.credential_service {
                    warn(
                        &mut *frontend,
                        auth::credential_approval_warning(&kit.manifest.handle, service, &name),
                    );
                }
            }
        }
        ReuseDecision::Reuse => {
            // An idle inspection cannot exclude independently started sessions
            // between apply and run. Refuse until SBX supplies a qualified
            // exclusion mechanism covering that whole interval.
            return Err(EngineError::Sandbox(format!(
                "sandbox '{name}' cannot be reused safely: SBX session exclusion is unqualified; \
                 save guest files and conversations, then remove it explicitly with `sbx rm {name}`"
            )));
        }
        ReuseDecision::Stale { reason } => {
            return Err(identity::stale_error(&name, reason, &options.workspace_dir));
        }
    }

    // 5. Credentials (sandbox-scoped; never reported as authentication).
    auth::register_launch_credentials(
        &snapshot,
        route.agent,
        &name,
        (&options.env_passthrough, &options.env_literal),
        &options.agent_credentials,
        &auth::CredentialLookups {
            env: &|var| snapshot.get(var).map(str::to_owned),
            stored: &|service| auth::secret_stored_in(&snapshot, service, &name),
        },
        &mut *frontend,
    )?;

    // 6. Apply and verify this launch's policy before the agent can start.
    let launch_id = uuid::Uuid::new_v4().to_string();
    let document =
        session_config::render_apply_document(&route, &kit.digest, &launch_id, &options)?;
    apply_policy(
        &snapshot,
        &name,
        &document,
        &options.workspace_dir,
        &mut *frontend,
    )?;

    // 7. Refuse only a confirmed breaking-major mismatch.
    if let Some(requirement) = def.cli_version.as_ref() {
        match SbxCommand::new(cli::agent_version_argv(&name, &requirement.binary))
            .in_env(&snapshot)
            .run_quiet()
        {
            Ok(out) => verify_sandbox_version(&name, requirement, &out, &options.workspace_dir)?,
            Err(error) => {
                tracing::warn!(sandbox = %name, %error, "agent CLI version probe failed; continuing launch")
            }
        }
    }

    // 8. Launch.
    let session_env = session_config::run_environment(&route, &launch_id, &options);
    let env_keys: Vec<String> = session_env.iter().map(|(k, _)| k.clone()).collect();
    let argv = cli::run_argv(&name, &env_keys, &tail);

    let started_at = Utc::now();
    let handle = AgentHandle {
        id: name.clone(),
        image_tag: route.agent.to_string(),
        name: name.clone(),
        started_at,
    };
    frontend.report_status(AgentStatus::Running {
        container_name: name.clone(),
    });
    announce(&mut *frontend, &format!("{SBX_BIN} {}", argv.join(" ")));

    frontend = crate::engine::agent_runtime::execution::with_program_status_frontend(
        frontend,
        options.program_status,
    );
    let io = frontend.take_io();
    let context = LaunchContext {
        env: snapshot,
        name,
        session_env,
        lock,
    };
    if io.initial_size.is_some() {
        spawn_pty_bridged(context, argv, started_at, handle, io, frontend)
    } else {
        spawn_piped(context, argv, started_at, handle, io, frontend)
    }
}

/// Create the sandbox (contract 5.4), recording `creating` before and
/// `created` (with the inspect evidence) after.
fn create_sandbox(
    snapshot: &crate::data::config::env::EnvSnapshot,
    data_home: &std::path::Path,
    intended: &IntendedSandbox,
    kit: &EmittedKit,
    cli_version: SbxCliVersion,
    options: &ResolvedSandboxOptions,
    sink: &mut dyn crate::data::message::UserMessageSink,
) -> Result<(), EngineError> {
    let record = intended.creating_record(cli_version);
    identity::store(data_home, &record)?;
    let selector = kit.create_selector()?;
    verify_workspace(options)?;
    SbxCommand::new(cli::create_argv(
        &intended.sandbox,
        &selector,
        options.memory_gb,
        &options.workspace_dir,
    ))
    .in_env(snapshot)
    .run_announced(sink)?;
    let inspect = SbxCommand::new(cli::inspect_argv(&intended.sandbox))
        .in_env(snapshot)
        .run_quiet()
        .ok()
        .filter(|out| out.success())
        .map(|out| out.stdout);
    identity::store(data_home, &record.created(inspect.as_deref()))
}

fn verify_workspace(options: &ResolvedSandboxOptions) -> Result<(), EngineError> {
    let guard = options.mount_guard.as_ref().ok_or_else(|| {
        EngineError::Sandbox("SBX requires an engine-admitted workspace mount authority".into())
    })?;
    let checked = || {
        guard.authority.check_workspace(&options.workspace_dir)?;
        if !guard
            .sources
            .iter()
            .any(|(path, _)| path == &options.workspace_dir)
        {
            return Err(
                crate::data::mount_authority::MountAuthorityError::Persisted(
                    "workspace source was not pinned".into(),
                ),
            );
        }
        guard.verify()
    };
    checked().map_err(|e| EngineError::Sandbox(format!("SBX workspace refused: {e}")))
}

/// Run `awman-apply` as root with the document on stdin (contract 5.5) and
/// require its exact acknowledgement. Never best-effort.
fn apply_policy(
    snapshot: &crate::data::config::env::EnvSnapshot,
    name: &str,
    document: &ApplyDocument,
    workspace: &std::path::Path,
    sink: &mut dyn crate::data::message::UserMessageSink,
) -> Result<(), EngineError> {
    let failed = |detail: String| EngineError::SbxPolicyApplyFailed {
        sandbox: name.to_string(),
        detail,
    };
    let wire = document.encode()?;
    let argv = cli::apply_argv(name);
    announce(
        sink,
        &format!(
            "{SBX_BIN} {} (launch policy piped via stdin)",
            argv.join(" ")
        ),
    );
    let out = SbxCommand::new(argv)
        .in_env(snapshot)
        .with_stdin(wire)
        .with_deadline(APPLY_DEADLINE)
        .run_quiet()
        .map_err(|e| failed(e.to_string()))?;
    classify_apply(name, document, &out, workspace)
}

/// Contract §6.2 step 6 failure semantics.
fn classify_apply(
    name: &str,
    document: &ApplyDocument,
    out: &super::spawn::SbxOutput,
    workspace: &std::path::Path,
) -> Result<(), EngineError> {
    let failed = |detail: String| EngineError::SbxPolicyApplyFailed {
        sandbox: name.to_string(),
        detail,
    };
    match out.exit_code {
        APPLY_EXIT_STALE_KIT => {
            let baked = out
                .stdout
                .lines()
                .find_map(|l| l.strip_prefix("AWMAN-APPLY-STALE "))
                .map(str::trim)
                .unwrap_or("unknown");
            Err(identity::stale_error(
                name,
                format!(
                    "its image was built from a different awman kit (baked kit {baked}, this \
                     launch needs {})",
                    document.kit_digest
                ),
                workspace,
            ))
        }
        EXEC_EXIT_COMMAND_MISSING => Err(identity::stale_error(
            name,
            "it does not contain awman's v3 kit (awman-apply is missing)".into(),
            workspace,
        )),
        APPLY_EXIT_OK if apply_acknowledged(document, &out.stdout) => Ok(()),
        APPLY_EXIT_OK => Err(failed(
            "the apply step exited 0 without acknowledging this launch".into(),
        )),
        code => {
            let line = out
                .stderr
                .lines()
                .rev()
                .find(|l| l.contains("awman-apply:"))
                .or_else(|| out.stderr.lines().rev().find(|l| !l.trim().is_empty()))
                .map(str::trim)
                .unwrap_or("no diagnostic");
            Err(failed(format!("apply exited with code {code}: {line}")))
        }
    }
}

fn verify_sandbox_version(
    name: &str,
    requirement: &crate::engine::agent::definition::CliMajorRequirement,
    out: &super::spawn::SbxOutput,
    workspace: &std::path::Path,
) -> Result<(), EngineError> {
    use crate::engine::container::agent_version::{classify_cli_version, CliVersionStatus};
    if !out.success() {
        tracing::warn!(
            sandbox = name,
            "agent CLI version probe failed; continuing launch"
        );
        return Ok(());
    }
    match classify_cli_version(requirement, out.stdout.lines().next().unwrap_or_default()) {
        CliVersionStatus::Mismatch { found, expected } => {
            return Err(identity::stale_error(
                name,
                format!("it runs {found}, not the expected {expected}"),
                workspace,
            ))
        }
        CliVersionStatus::Unknown { reason } => {
            tracing::warn!(sandbox = name, %reason, "agent CLI version is unknown; continuing launch")
        }
        CliVersionStatus::Compatible { .. } => {}
    }
    Ok(())
}

struct LaunchContext {
    env: crate::data::config::env::EnvSnapshot,
    name: String,
    /// Non-secret session variables set on the `sbx run` child; only their
    /// names are in its argv.
    session_env: Vec<(String, String)>,
    lock: SandboxLock,
}

/// The environment of the host `sbx run` client: host-terminal variables
/// removed and `TERM` (plus `COLORTERM` only when the host set it)
/// normalised, then the session variables. Returns `(remove, set)`.
fn run_client_env(
    snapshot: &crate::data::config::env::EnvSnapshot,
    session_env: &[(String, String)],
) -> (Vec<String>, Vec<(String, String)>) {
    let (remove, mut set) = agent_client_env(snapshot.spawn_env().map(|(k, v)| {
        (
            k.to_string_lossy().into_owned(),
            v.to_string_lossy().into_owned(),
        )
    }));
    set.extend(session_env.iter().cloned());
    (remove, set)
}

fn spawn_pty_bridged(
    context: LaunchContext,
    argv: Vec<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    use portable_pty::{native_pty_system, PtySize};
    let LaunchContext {
        env: snapshot,
        name,
        session_env,
        lock,
    } = context;

    let (cols, rows) = io.initial_size.expect("PTY path requires initial_size");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| EngineError::Sandbox(format!("openpty: {e}")))?;

    let mut cmd = crate::engine::host_cli::pty_command(&snapshot, SBX_BIN);
    for arg in &argv {
        cmd.arg(arg);
    }
    let (remove, set) = run_client_env(&snapshot, &session_env);
    for key in remove {
        cmd.env_remove(key);
    }
    for (key, value) in set {
        cmd.env(key, value);
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Sandbox(format!("spawn sbx via pty: {e}")))?;

    let (master_arc, bridge) = io_bridge::bridge_pty(
        io,
        pair,
        frontend.program_status_config(),
        frontend.terminal_launch(),
    )?;

    let backend = DSbxExecution {
        env: snapshot,
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        sandbox_name: name,
        started_at,
        output: bridge.output,
        sink: Some(frontend),
        wait_sink: None,
        _lock: lock,
    };
    Ok(
        AgentExecution::new(handle, Box::new(backend), bridge.stuck_tx, None)
            .with_program_status(bridge.program_status),
    )
}

/// WI 0141 §3: the environment of the host `sbx run` client through which
/// the agent's session starts — the process whose terminal environment the
/// session inherits. Host-terminal variables are removed and `TERM` (plus
/// `COLORTERM` only when the host set it) is normalised, so the sandbox
/// agent never sees the host's terminal identity. Returns `(remove, set)`.
fn agent_client_env(
    host: impl IntoIterator<Item = (String, String)>,
) -> (Vec<String>, Vec<(String, String)>) {
    use crate::engine::container::host_env_denylist::{
        container_terminal_env, is_host_terminal_var,
    };
    let mut remove = Vec::new();
    let mut colorterm = None;
    for (name, value) in host {
        if name == "COLORTERM" {
            colorterm = Some(value);
            remove.push(name);
        } else if is_host_terminal_var(&name) {
            remove.push(name);
        }
    }
    (remove, container_terminal_env(colorterm.as_deref()))
}

fn spawn_piped(
    context: LaunchContext,
    argv: Vec<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    use std::process::Stdio;
    let LaunchContext {
        env: snapshot,
        name,
        session_env,
        lock,
    } = context;

    let mut cmd = crate::engine::host_cli::command(&snapshot, SBX_BIN);
    cmd.args(&argv);
    let (remove, set) = run_client_env(&snapshot, &session_env);
    for key in remove {
        cmd.env_remove(key);
    }
    cmd.envs(set);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::Sandbox(SBX_NOT_FOUND.into())
        } else {
            EngineError::Sandbox(format!("spawn sbx: {e}"))
        }
    })?;

    let bridge = io_bridge::bridge_piped(io, &mut child, frontend.program_status_config());
    // The prompt travels in argv; drop the injector so the writer task
    // closes the child's stdin once the frontend's own sender is gone.
    drop(bridge.stdin_injector);

    let backend = DSbxExecution {
        env: snapshot,
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        sandbox_name: name,
        started_at,
        output: bridge.output,
        sink: Some(frontend),
        wait_sink: None,
        _lock: lock,
    };
    Ok(
        AgentExecution::new(handle, Box::new(backend), bridge.stuck_tx, None)
            .with_program_status(bridge.program_status),
    )
}

// ─── Execution backend ──────────────────────────────────────────────────────

/// How many captured output lines to replay into the sink when the agent
/// exits non-zero.
const FAILURE_TAIL_LINES: usize = 30;

struct DSbxExecution {
    env: crate::data::config::env::EnvSnapshot,
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<io_bridge::PtyMaster>,
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    sandbox_name: String,
    started_at: chrono::DateTime<Utc>,
    /// Tail of everything `sbx run` wrote, captured by the io bridge.
    output: std::sync::Arc<std::sync::Mutex<io_bridge::OutputCapture>>,
    /// The launch frontend, retained so a non-zero exit can replay the
    /// captured output as messages in the execution window.
    sink: Option<Box<dyn AgentFrontend>>,
    wait_sink: Option<std::sync::Weak<std::sync::Mutex<Box<dyn AgentFrontend>>>>,
    /// The per-sandbox launch lock, released when the execution is dropped
    /// (after the agent exits).
    _lock: SandboxLock,
}

impl DSbxExecution {
    /// On a non-zero exit, replay the captured tail of the agent's output
    /// into the message sink — `sbx run` launch failures otherwise vanish
    /// with the PTY, leaving only an exit code.
    fn report_failure_output(&mut self, exit_code: i32) {
        use crate::data::message::{MessageLevel, UserMessage};
        if exit_code == 0 {
            return;
        }
        // The bridge reader threads can still be draining the child's final
        // bytes when its exit is observed; wait (briefly) until the capture
        // stops growing. Runs inside spawn_blocking, so sleeping is fine.
        let mut prev = usize::MAX;
        for _ in 0..10 {
            let len = self.output.lock().map(|c| c.len()).unwrap_or(0);
            if len == prev {
                break;
            }
            prev = len;
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let lines = match self.output.lock() {
            Ok(capture) => capture.tail_lines(FAILURE_TAIL_LINES),
            Err(_) => return,
        };
        if lines.is_empty() {
            return;
        }
        let report = |sink: &mut dyn AgentFrontend| {
            sink.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("sbx exited with code {exit_code}; last output:"),
            });
            for line in lines {
                sink.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: line,
                });
            }
        };
        if let Some(sink) = self.sink.as_mut() {
            report(sink.as_mut());
        } else if let Some(sink) = self.wait_sink.as_ref().and_then(std::sync::Weak::upgrade) {
            report(sink.lock().unwrap().as_mut());
        }
    }
}

impl ExecutionBackend for DSbxExecution {
    fn frontend_for_wait(
        &mut self,
    ) -> Option<crate::engine::agent_runtime::execution::FrontendWaitGuard> {
        let frontend = self.sink.take()?;
        let guard = crate::engine::agent_runtime::execution::FrontendWaitGuard::new(frontend);
        self.wait_sink = Some(guard.downgrade());
        Some(guard)
    }

    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Sandbox(format!("wait sbx (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            self.report_failure_output(exit_code);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: Utc::now(),
            });
        }

        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Sandbox("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Sandbox(format!("wait sbx: {e}")))?;
        let exit_code = status.code().unwrap_or(-1);
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        self.report_failure_output(exit_code);
        Ok(AgentExitInfo {
            exit_code,
            signal,
            started_at: self.started_at,
            ended_at: Utc::now(),
        })
    }

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.stdin_injector {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Sandbox(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        // Cancel preserves the persistent volume — `sbx stop`, never `sbx rm`.
        let _ = SbxCommand::new(cli::stop_argv(&self.sandbox_name))
            .in_env(&self.env)
            .run_quiet();
        Ok(())
    }

    fn cancel_handle(&self) -> crate::engine::agent_runtime::execution::CancelHandle {
        let name = self.sandbox_name.clone();
        let snapshot = self.env.clone();
        crate::engine::agent_runtime::execution::CancelHandle::new(move || {
            let _ = SbxCommand::new(cli::stop_argv(&name))
                .in_env(&snapshot)
                .run_quiet();
            Ok(())
        })
    }
}

#[cfg(test)]
mod adapter_tail_tests {
    //! The full `sbx run` argv per agent and route (contract §3.2 and 5.6):
    //! the tail after `--` appears once, never repeats a workload's baked
    //! subcommand, and carries only argv deliveries.
    use super::*;
    use crate::engine::container::options::ModelFlagForm;
    use crate::engine::sandbox::options::SandboxOption;

    fn run(agent: &str, interactive: bool, model: Option<&str>, seed: Option<&str>) -> Vec<String> {
        let mut options = vec![
            SandboxOption::AgentId(agent.into()),
            SandboxOption::Interactive(interactive),
        ];
        if let Some(m) = model {
            options.push(SandboxOption::Model {
                flag: ModelFlagForm::Argument(m.into()),
            });
        }
        if let Some(s) = seed {
            options.push(SandboxOption::SeededPrompt(s.into()));
        }
        let options = ResolvedSandboxOptions::resolve(options);
        let def = first_party(agent).unwrap();
        let tail = def.sbx_launch_tail(&SbxTailRequest {
            interactive: options.interactive,
            model: options.model.as_ref(),
            mode_flags: &options.mode_flags,
            agent_flags: &options.agent_flags,
            system_prompt: None,
            seeded_prompt: options.seeded_prompt.as_deref(),
        });
        cli::run_argv("n", &[], &tail)
    }

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    fn with_prefix(tail: &[&str]) -> Vec<String> {
        let mut argv = s(&["run", "--name", "n", "-e", "AWMAN_LAUNCH_ID"]);
        if !tail.is_empty() {
            argv.push("--".into());
            argv.extend(s(tail));
        }
        argv
    }

    #[test]
    fn production_launch_requires_engine_mount_admission() {
        let error = verify_workspace(&ResolvedSandboxOptions::default()).unwrap_err();
        assert!(error
            .to_string()
            .contains("engine-admitted workspace mount authority"));
    }

    #[test]
    fn published_workloads_get_only_their_headless_selector_and_prompt() {
        assert_eq!(run("claude", true, None, None), with_prefix(&[]));
        assert_eq!(run("claude", true, None, Some("hi")), with_prefix(&["hi"]));
        assert_eq!(
            run("claude", false, Some("opus"), Some("hi")),
            with_prefix(&["--print", "hi"]),
            "claude's model is a managed setting, never argv"
        );
        assert_eq!(
            run("codex", true, Some("o3"), Some("hi")),
            with_prefix(&["--model", "o3", "hi"]),
            "codex --no-daemon describes awman's image and is never sent"
        );
        assert_eq!(
            run("codex", false, Some("o3"), Some("hi")),
            with_prefix(&["exec", "--model", "o3", "hi"])
        );
        assert_eq!(
            run("copilot", true, None, Some("hi")),
            with_prefix(&["-i", "hi"])
        );
        assert_eq!(
            run("copilot", false, None, Some("hi")),
            with_prefix(&["-p", "hi"])
        );
        for agent in ["claude", "codex", "copilot"] {
            let argv = run(agent, false, None, Some("hi"));
            assert!(!argv.contains(&agent.to_string()), "{agent}: {argv:?}");
            assert_eq!(argv.iter().filter(|a| *a == "run").count(), 1, "{argv:?}");
        }
    }

    #[test]
    fn owned_workload_tail_keeps_the_definition_shape_after_its_entrypoint() {
        assert_eq!(
            run("opencode", false, None, Some("hi")),
            with_prefix(&["run", "hi"])
        );
        assert_eq!(
            run("opencode", true, Some("a/b"), Some("hi")),
            with_prefix(&["--model", "a/b", "--prompt", "hi"])
        );
        assert!(!run("opencode", false, None, Some("hi")).contains(&"opencode".to_string()));
    }

    #[test]
    fn sandbox_version_metadata_is_advisory_except_confirmed_wrong_major() {
        let requirement = first_party("opencode").unwrap().cli_version.unwrap();
        for (exit_code, stdout, compatible) in [
            (1, "1.19.6", true),
            (1, "2.3.2", true),
            (0, "", true),
            (0, "nightly", true),
            (0, "1.999.999", true),
            (0, "2.3.2", false),
        ] {
            let out = super::super::spawn::SbxOutput {
                stdout: stdout.into(),
                stderr: String::new(),
                exit_code,
            };
            assert_eq!(
                verify_sandbox_version("fixture", &requirement, &out, std::path::Path::new("/w"))
                    .is_ok(),
                compatible
            );
        }
    }

    #[test]
    fn refused_agents_and_undeliverable_options_fail_before_any_sbx_call() {
        for agent in ["cursor", "opencode-v2"] {
            assert!(supported_route(agent).is_err(), "{agent}");
        }
        assert!(matches!(
            supported_route("my-custom"),
            Err(EngineError::CustomAgentRuntimeUnsupported { .. })
        ));
        let refused = |agent: &str, extra: Vec<SandboxOption>| {
            let mut options = vec![
                SandboxOption::AgentId(agent.into()),
                SandboxOption::WorkspaceDir("/repo".into()),
            ];
            options.extend(extra);
            validate_launch(&ResolvedSandboxOptions::resolve(options))
                .unwrap_err()
                .to_string()
        };
        assert!(
            refused("codex", vec![SandboxOption::PermissionMode("yolo".into())])
                .contains("cannot be delivered to 'codex'")
        );
        assert!(refused(
            "claude",
            vec![SandboxOption::AgentModeFlags(vec!["--x".into()])]
        )
        .contains("cannot be delivered to 'claude'"));
        assert!(refused(
            "claude",
            vec![SandboxOption::AgentFlags(vec!["--x".into()])]
        )
        .contains("agentFlags"));
        assert!(refused(
            "copilot",
            vec![SandboxOption::Model {
                flag: ModelFlagForm::Argument("m".into())
            }]
        )
        .contains("--model"));
        assert!(
            refused("claude", vec![SandboxOption::SeededPrompt("--rm".into())])
                .contains("must not start with '-'")
        );
        assert!(refused(
            "opencode",
            vec![SandboxOption::AgentSetting {
                key: "opencode".into(),
                value: serde_json::json!({})
            }]
        )
        .contains("agentSettings"));
        assert!(refused(
            "claude",
            vec![SandboxOption::ExtraOverlay(
                crate::engine::container::options::OverlaySpec {
                    host_path: "/elsewhere/x".into(),
                    container_path: "/elsewhere/x".into(),
                    permission: crate::engine::container::options::OverlayPermission::ReadOnly,
                }
            )]
        )
        .contains("outside the workspace"));
        assert!(validate_launch(&ResolvedSandboxOptions::resolve([
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/repo".into()),
            SandboxOption::PermissionMode("plan".into()),
        ]))
        .is_ok());
    }
}

#[cfg(all(test, unix))]
mod launch_tests;
