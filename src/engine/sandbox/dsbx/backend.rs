//! `DSbxBackend` — the Docker Sandboxes (`sbx`) driver.
//!
//! Replaces the WI 0089 stub with the real `sbx`-driven implementation:
//! lifecycle (`run`/`create`/`stop`/`rm`), listing (`sbx ls --json`),
//! exec, and the interactive PTY-bridged launch ([`run_interactive`]).
//!
//! Every headless `sbx` invocation goes through [`super::spawn`]; the
//! interactive `sbx run` PTY session is bridged by [`super::io_bridge`] after
//! its argv is announced on the sink.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::data::fs::SandboxKitPaths;
use crate::data::session::AgentHandle;
use crate::engine::agent::definition::SbxKitKind;
use crate::engine::agent::first_party::first_party;
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentStats, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::SandboxBackend;
use crate::engine::sandbox::dsbx::auth;
use crate::engine::sandbox::dsbx::io_bridge;
use crate::engine::sandbox::dsbx::session_config::DSbxSessionConfig;
use crate::engine::sandbox::dsbx::spawn::{SbxCommand, SBX_BIN};
use crate::engine::sandbox::naming::sandbox_name_for;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// In-VM path of the per-launch config apply script. Must match the
/// `commands.startup` entry the kit templates declare
/// (`templates/sbx-kit.<agent>.yaml`).
const APPLY_SCRIPT_VM_PATH: &str = "/home/agent/.awman/apply-session-config.sh";

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
        let _ = SbxCommand::new(["stop", &handle.name])
            .in_env(&self.env)
            .run_quiet();
        Ok(())
    }

    fn list_running(&self) -> Result<Vec<AgentHandle>, EngineError> {
        list_awman_sandboxes(&self.env)
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        // Sandboxes run the agent directly in the VM (not as a Docker
        // container), so there are no per-resource metrics to poll. Report
        // zeros with the sandbox name, per the unified AgentStats shape.
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

// ─── Naming / kit-dir helpers ───────────────────────────────────────────────

/// Emit a Warning-level message on a sink trait object (the `warning`
/// convenience method is `Self: Sized` and so unavailable on `dyn`).
fn warn(sink: &mut dyn crate::data::message::UserMessageSink, text: String) {
    sink.write_message(crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Warning,
        text,
    });
}

fn require_agent(opts: &ResolvedSandboxOptions) -> Result<String, EngineError> {
    if opts.agent_id.is_empty() {
        return Err(EngineError::Sandbox(
            "sandbox launch requires an agent id".into(),
        ));
    }
    Ok(opts.agent_id.clone())
}

/// Resolve the deterministic sandbox name. Uses a caller-supplied name when
/// present, otherwise derives `awman-<worktree-hash>-<agent>`.
fn resolve_sandbox_name(opts: &ResolvedSandboxOptions) -> String {
    opts.sandbox_name
        .clone()
        .unwrap_or_else(|| sandbox_name_for(&opts.workspace_dir, &opts.agent_id))
}

fn kit_dir_for(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
) -> Result<PathBuf, EngineError> {
    Ok(
        SandboxKitPaths::at_home(crate::data::fs::data_paths::config_home(snapshot)?)
            .kit_dir(agent),
    )
}

/// The sbx kit kind for `agent`. Sandbox-class runtimes run first-party
/// agents only (custom agents are refused before any sbx code runs), so this
/// reads the first-party table directly; any other name is a mixin.
fn kit_kind_for(agent: &str) -> SbxKitKind {
    first_party(agent)
        .map(|d| d.sbx_kit_kind)
        .unwrap_or(SbxKitKind::Mixin)
}

// ─── Listing ────────────────────────────────────────────────────────────────

/// All awman-owned sandbox handles from `sbx ls`.
fn list_awman_sandboxes(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Result<Vec<AgentHandle>, EngineError> {
    Ok(try_list_all_sandbox_names(snapshot)?
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

/// Every sandbox name `sbx ls` reports (awman-owned or not). Tolerant of both
/// the `--json` mode and the plain table fallback; returns an empty list when
/// `sbx` is unavailable or not logged in.
pub(super) fn list_all_sandbox_names_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Vec<String> {
    try_list_all_sandbox_names(snapshot).unwrap_or_default()
}

fn try_list_all_sandbox_names(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Result<Vec<String>, EngineError> {
    if let Ok(out) = SbxCommand::new(["ls", "--json"])
        .in_env(snapshot)
        .run_quiet()
    {
        if out.success() {
            let names = parse_ls_json(&out.stdout);
            if !names.is_empty() || out.stdout.trim() == "[]" {
                return Ok(names);
            }
        }
    }
    crate::engine::host_cli::check_collection_cancel()?;
    let output = SbxCommand::new(["ls"]).in_env(snapshot).run_quiet()?;
    Ok(parse_ls_table(&output.stdout))
}

/// Parse `sbx ls --json`. The exact schema is unverified as of June 2026
/// (Phase 0), so this accepts either a top-level array or an object wrapping
/// a `sandboxes` array, and reads `name` / `Name`.
fn parse_ls_json(stdout: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let array = value
        .as_array()
        .cloned()
        .or_else(|| value.get("sandboxes").and_then(|s| s.as_array().cloned()))
        .unwrap_or_default();
    array
        .iter()
        .filter_map(|item| {
            item.get("name")
                .or_else(|| item.get("Name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Fallback parser for the plain `sbx ls` table: pick the first
/// `awman-`-prefixed whitespace token on each line.
fn parse_ls_table(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip(1) // header row
        .filter_map(|line| {
            line.split_whitespace()
                .find(|tok| tok.starts_with("awman-"))
                .map(|s| s.to_string())
        })
        .collect()
}

fn sandbox_name_exists(snapshot: &crate::data::config::env::EnvSnapshot, name: &str) -> bool {
    list_all_sandbox_names_in(snapshot)
        .iter()
        .any(|n| n == name)
}

// ─── Interactive launch ─────────────────────────────────────────────────────

/// Perform the full interactive launch: write `session.json`, ensure the
/// sandbox exists (`sbx create` on first launch), register sandbox-scoped
/// credentials, then announce `sbx run <name>` and bridge the PTY/piped I/O
/// to the frontend.
///
/// This is the sandbox tier's equivalent of `DockerContainerInstance::
/// run_with_frontend`. It is reached from `SandboxRuntime`'s `AgentInstance`.
pub(in crate::engine::sandbox) fn run_interactive(
    options: ResolvedSandboxOptions,
    mut frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    let snapshot = options
        .host_env
        .clone()
        .unwrap_or_else(crate::engine::host_cli::process_env);
    options.validate_tool_restrictions()?;
    let agent = require_agent(&options)?;
    let name = resolve_sandbox_name(&options);
    let kit_dir = kit_dir_for(&snapshot, &agent)?;

    // 1. Per-launch session config (no credential values).
    DSbxSessionConfig::write_for(&options, &options.workspace_dir)?;
    if let Some(notice) = DSbxSessionConfig::host_terminal_notice(&options) {
        warn(&mut *frontend, notice);
    }

    // 2. Surface unsupported / withheld options.
    if options.cpu_limit.is_some() {
        warn(
            &mut *frontend,
            "sbx: CPU limits are not supported by Docker Sandboxes; ignoring.".to_string(),
        );
    }
    // Outside-workspace overlays are rejected with a clear error (WI 0090):
    // the VM can only see virtiofs-mounted workspace paths, and silently
    // launching without the overlay would surprise the user.
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
    for note in &options.unsupported_notes {
        warn(&mut *frontend, format!("sbx: {note}"));
    }

    // 3. Ensure the sandbox exists — every secret registration below is
    //    sandbox-scoped, which requires the sandbox to exist. First launch
    //    runs `sbx create --kit ...` (announced); later launches skip
    //    straight to auth.
    let created = if sandbox_name_exists(&snapshot, &name) {
        false
    } else {
        SbxCommand::new(create_argv(&name, &agent, &kit_dir, &options))
            .in_env(&snapshot)
            .run_announced(&mut *frontend)?;
        true
    };

    // 4. Launch-time auto-auth: register `env(VAR)` overlay credentials for
    //    supported provider auth vars with sandbox-scoped `sbx secret set`
    //    (scoped secrets apply immediately; global ones only at creation —
    //    awman never sets global secrets), then keychain-resolved credentials
    //    (announced + redacted throughout).
    let auth_result = auth::auto_auth_env_overlays_in(
        &snapshot,
        &agent,
        &name,
        (&options.env_passthrough, &options.env_literal),
        &|name| snapshot.get(name).map(str::to_owned),
        &|service| auth::secret_registered_for_service_in(&snapshot, service),
        &mut *frontend,
    )
    .and_then(|_overlay_registered| {
        auth::inject_credentials_in(&snapshot, &options.agent_credentials, &name, &mut *frontend)
    });
    if let Err(e) = auth_result {
        // A failure after a successful create leaves the sandbox behind on
        // purpose: the next launch finds it and skips straight to auth.
        return Err(if created {
            EngineError::Sandbox(format!(
                "{e}; sandbox '{name}' was created and will be reused on the next launch"
            ))
        } else {
            e
        });
    }

    // 5. Re-apply the per-launch session config on a pre-existing sandbox.
    //    `commands.startup` only runs when the VM boots; a sandbox still
    //    running from a previous session would otherwise keep the config it
    //    booted with — a changed `--model` (or permission mode, tool lists,
    //    …) in the freshly written session.json would silently not apply.
    //    Best-effort: against a stopped sandbox this exec may fail, and the
    //    boot-time startup then applies the same file. A freshly created
    //    sandbox needs none of this — session.json was written before
    //    `sbx create`, so its first boot reads the current config.
    if !created {
        let mut argv = vec!["exec".to_string()];
        if !options.workspace_dir.as_os_str().is_empty() {
            // The apply script locates session.json via $WORKDIR; boot-time
            // startup gets it from sbx, but `sbx exec` does not set it.
            argv.push("--env".to_string());
            argv.push(format!("WORKDIR={}", options.workspace_dir.display()));
        }
        argv.push(name.clone());
        argv.push("bash".to_string());
        argv.push(APPLY_SCRIPT_VM_PATH.to_string());
        crate::engine::sandbox::dsbx::spawn::announce(
            &mut *frontend,
            &format!("{SBX_BIN} {}", argv.join(" ")),
        );
        match SbxCommand::new(argv).in_env(&snapshot).run_quiet() {
            Ok(out) if out.success() => {}
            _ => warn(
                &mut *frontend,
                "sbx: could not re-apply the session config on the existing sandbox; \
                 if it was stopped, the boot-time startup script applies it instead."
                    .to_string(),
            ),
        }
    }

    // 6. The launch argv is always the positional-run form — the kit and
    //    workspace are baked into the sandbox at creation.
    let argv = run_argv(&name, &agent, &options);

    let started_at = Utc::now();
    let handle = AgentHandle {
        id: name.clone(),
        image_tag: agent.clone(),
        name: name.clone(),
        started_at,
    };

    frontend.report_status(AgentStatus::Running {
        container_name: name.clone(),
    });

    // 7. Announce the command, then bridge I/O.
    crate::engine::sandbox::dsbx::spawn::announce(
        &mut *frontend,
        &format!("{SBX_BIN} {}", argv.join(" ")),
    );

    frontend = crate::engine::agent_runtime::execution::with_program_status_frontend(
        frontend,
        options.program_status,
    );
    let io = frontend.take_io();
    let seed = stdin_seed(&agent, &options);
    let context = LaunchContext {
        env: snapshot,
        name,
    };
    if io.initial_size.is_some() {
        spawn_pty_bridged(context, argv, seed, started_at, handle, io, frontend)
    } else {
        spawn_piped(context, argv, seed, started_at, handle, io, frontend)
    }
}

/// The seeded prompt to write into the launch's stdin, or `None`.
///
/// `kind: agent` kits already carry the prompt as a positional launch arg
/// (`append_seeded_positional`), so writing it to stdin too would deliver it
/// twice. `kind: mixin` kits launch via Docker's built-in template — the
/// apply script can only stage the prompt, not deliver it — so stdin
/// injection is their single delivery path (PTY type-ahead when interactive,
/// piped stdin otherwise).
fn stdin_seed(agent: &str, options: &ResolvedSandboxOptions) -> Option<String> {
    match kit_kind_for(agent) {
        SbxKitKind::Agent => None,
        SbxKitKind::Mixin => options.seeded_prompt.clone(),
    }
}

/// Creation argv: `sbx create --kit <dir> --name <name> [--memory Ng] <agent>
/// <workspace>`. `--name` is valid here (it is creation-only), and the
/// workspace is a positional path after the agent — there is no
/// `--workspace-dir` flag.
fn create_argv(
    name: &str,
    agent: &str,
    kit_dir: &Path,
    options: &ResolvedSandboxOptions,
) -> Vec<String> {
    let mut argv = vec![
        "create".to_string(),
        "--kit".to_string(),
        kit_dir.display().to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    if let Some(mem) = options.memory_gb {
        argv.push("--memory".to_string());
        argv.push(format!("{mem}g"));
    }
    argv.push(agent.to_string());
    push_workspace_positional(&mut argv, options);
    argv
}

/// Launch argv: `sbx run <name> [-- AGENT_ARGS...]`. Used for every
/// interactive launch — the sandbox always exists by this point (first
/// launches run `sbx create` beforehand), and an existing sandbox is
/// addressed by its positional name: `--name` is creation-only and sbx errors
/// with "sandbox '<name>' already exists" if used here. The kit, agent, and
/// workspace are baked in at creation, so none of them appear.
fn run_argv(name: &str, agent: &str, options: &ResolvedSandboxOptions) -> Vec<String> {
    let mut argv = vec!["run".to_string(), name.to_string()];
    let args = agent_args(agent, options);
    if !args.is_empty() {
        argv.push("--".to_string());
        argv.extend(args);
    }
    argv
}

/// Append the workspace dir as a positional path after the agent. Skipped
/// when unset — sbx then defaults to the invoking process's cwd.
fn push_workspace_positional(argv: &mut Vec<String>, options: &ResolvedSandboxOptions) {
    if !options.workspace_dir.as_os_str().is_empty() {
        argv.push(options.workspace_dir.display().to_string());
    }
}

/// Agent args appended after the `--` delimiter, mirroring the argv the
/// container path builds after the entrypoint: headless
/// subcommand/flag, mode flags (yolo/auto/plan), model flag, user `agentFlags`,
/// then the seeded prompt (a bare positional would be parsed by sbx as a workspace PATH).
/// Only `kind: agent` kits take agent args — awman owns their entrypoint,
/// and sbx forwards everything after `--` to it. `kind: mixin` kits launch
/// through Docker's built-in template (no argv channel): their dynamic
/// config flows through `session.json` and their prompt via stdin
/// ([`stdin_seed`]), so they get no args here.
fn agent_args(agent: &str, options: &ResolvedSandboxOptions) -> Vec<String> {
    let Some(def) = first_party(agent) else {
        return Vec::new();
    };
    agent_args_for(&def, options)
}

fn agent_args_for(
    def: &crate::engine::agent::AgentDefinition,
    options: &ResolvedSandboxOptions,
) -> Vec<String> {
    use crate::engine::container::options::ModelFlagForm;

    if !matches!(def.sbx_kit_kind, SbxKitKind::Agent) {
        return Vec::new();
    }
    let mut args = Vec::new();
    if options.interactive {
        args.extend(def.interactive_only_flags.iter().cloned());
    } else {
        if let Some(flag) = &def.non_interactive_flag {
            args.push(flag.clone());
        }
    }
    args.extend(options.mode_flags.iter().cloned());
    if let Some(model) = &options.model {
        match model {
            ModelFlagForm::Argument(name) => {
                args.push("--model".to_string());
                args.push(name.clone());
            }
            ModelFlagForm::Shorthand(s) => args.push(s.clone()),
        }
    }
    args.extend(options.agent_flags.iter().cloned());
    if let Some(prompt) = &options.seeded_prompt {
        if options.interactive {
            if let crate::engine::agent::InteractiveSeedDelivery::Flag(flag) =
                &def.interactive_seed_delivery
            {
                args.push(flag.clone());
            }
        }
        args.push(prompt.clone());
    }
    args
}

struct LaunchContext {
    env: crate::data::config::env::EnvSnapshot,
    name: String,
}

fn spawn_pty_bridged(
    context: LaunchContext,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    let LaunchContext {
        env: snapshot,
        name,
    } = context;
    use portable_pty::{native_pty_system, PtySize};

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
    let (remove, set) = agent_client_env(snapshot.spawn_env().map(|(k, v)| {
        (
            k.to_string_lossy().into_owned(),
            v.to_string_lossy().into_owned(),
        )
    }));
    for name in remove {
        cmd.env_remove(name);
    }
    for (name, value) in set {
        cmd.env(name, value);
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Sandbox(format!("spawn sbx via pty: {e}")))?;

    // Mixin seeded prompt: queue it on the stdin channel before the bridge
    // starts. The PTY delivers it as type-ahead input — the agent reads it
    // (with the trailing \r as Enter) once it starts accepting input.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\r".to_vec());
    }

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
    };
    // Sandbox-class runtimes have no container output tail; failure-log capture
    // is a container-paradigm feature.
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
    seeded: Option<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    let LaunchContext {
        env: snapshot,
        name,
    } = context;
    use std::process::Stdio;

    let mut cmd = crate::engine::host_cli::command(&snapshot, SBX_BIN);
    cmd.args(&argv);
    let (remove, set) = agent_client_env(snapshot.spawn_env().map(|(k, v)| {
        (
            k.to_string_lossy().into_owned(),
            v.to_string_lossy().into_owned(),
        )
    }));
    for name in remove {
        cmd.env_remove(name);
    }
    cmd.envs(set);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::Sandbox(
                "`sbx` binary not found on PATH; install Docker Sandboxes \
                 (`brew install docker/tap/sbx`)"
                    .into(),
            )
        } else {
            EngineError::Sandbox(format!("spawn sbx: {e}"))
        }
    })?;

    // Write the seeded prompt into stdin before the writer task starts.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\n".to_vec());
    }

    let bridge = io_bridge::bridge_piped(io, &mut child, frontend.program_status_config());
    // Drop the injector so the writer task closes the child's stdin after the
    // seeded prompt drains (matches the container piped path).
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
}

impl DSbxExecution {
    /// On a non-zero exit, replay the captured tail of the agent's output
    /// into the message sink — `sbx run` launch failures (kit compose
    /// errors, login problems) otherwise vanish with the PTY, leaving only
    /// an exit code and a by-hand sbx rerun to diagnose.
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
        let _ = SbxCommand::new(["stop", &self.sandbox_name])
            .in_env(&self.env)
            .run_quiet();
        Ok(())
    }

    fn cancel_handle(&self) -> crate::engine::agent_runtime::execution::CancelHandle {
        let name = self.sandbox_name.clone();
        let snapshot = self.env.clone();
        crate::engine::agent_runtime::execution::CancelHandle::new(move || {
            let _ = SbxCommand::new(["stop", &name])
                .in_env(&snapshot)
                .run_quiet();
            Ok(())
        })
    }
}
