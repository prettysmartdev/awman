//! `DSbxBackend` — the Docker Sandboxes (`sbx`) driver.
//!
//! Replaces the WI 0089 stub with the real `sbx`-driven implementation:
//! lifecycle (`run`/`create`/`stop`/`rm`), listing (`sbx ls --json`),
//! exec, and the interactive PTY-bridged launch ([`run_interactive`]).
//!
//! Every non-interactive `sbx` invocation goes through [`super::spawn`]; the
//! interactive `sbx run` PTY session is bridged by [`super::io_bridge`] after
//! its argv is announced on the sink.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::data::fs::SandboxKitPaths;
use crate::data::session::AgentHandle;
use crate::engine::agent::agent_matrix::{matrix_for, SbxKitKind};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentStats, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentStatus};
use crate::engine::agent_runtime::ExecOutput;
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::{SandboxBackend, SandboxId};
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
pub(in crate::engine::sandbox) struct DSbxBackend;

impl DSbxBackend {
    pub(in crate::engine::sandbox) fn new() -> Self {
        Self
    }
}

impl SandboxBackend for DSbxBackend {
    fn start_sandbox(&self, opts: &ResolvedSandboxOptions) -> Result<SandboxId, EngineError> {
        let agent = require_agent(opts)?;
        let name = resolve_sandbox_name(opts);
        let kit_dir = kit_dir_for(&agent)?;
        // Background create writes the per-launch config but cannot inject
        // credentials (no sink to report through); the interactive launch path
        // owns all credential registration.
        DSbxSessionConfig::write_for(opts, &opts.workspace_dir)?;
        SbxCommand::new(create_argv(&name, &agent, &kit_dir, opts)).run_checked()?;
        Ok(SandboxId::new(name))
    }

    fn restart_sandbox(&self, id: &SandboxId) -> Result<(), EngineError> {
        // `--name` is creation-only; an existing sandbox is run by passing its
        // name as the positional: `sbx run SANDBOX [-- AGENT_ARGS...]`.
        SbxCommand::new(["run", id.as_str()]).run_checked()?;
        Ok(())
    }

    fn exec_in_sandbox(
        &self,
        id: &SandboxId,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        let mut argv = vec!["exec".to_string()];
        if let Some(env) = env {
            for (k, v) in env {
                argv.push("--env".to_string());
                argv.push(format!("{k}={v}"));
            }
        }
        argv.push(id.0.clone());
        argv.push("sh".to_string());
        argv.push("-lc".to_string());
        argv.push(format!("cd {} && {command}", shell_quote(working_dir)));
        let out = SbxCommand::new(argv).run_quiet()?;
        Ok(ExecOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
        })
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        // `sbx stop` pauses the VM and preserves its persistent volume.
        // Best-effort: a non-zero exit (already stopped) is not an error.
        let _ = SbxCommand::new(["stop", &handle.name]).run_quiet();
        Ok(())
    }

    fn remove(&self, id: &SandboxId) -> Result<(), EngineError> {
        // `sbx rm` deletes the VM and its persistent volume.
        let _ = SbxCommand::new(["rm", id.as_str()]).run_quiet();
        Ok(())
    }

    fn list_running(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(list_awman_sandboxes())
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

/// Single-quote a string for `sh -c`, escaping embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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

fn kit_dir_for(agent: &str) -> Result<PathBuf, EngineError> {
    Ok(SandboxKitPaths::from_process_env()?.kit_dir(agent))
}

fn kit_kind_for(agent: &str) -> SbxKitKind {
    matrix_for(agent)
        .map(|m| m.sbx_kit_kind)
        .unwrap_or(SbxKitKind::Mixin)
}

// ─── Listing ────────────────────────────────────────────────────────────────

/// All awman-owned sandbox handles from `sbx ls`.
fn list_awman_sandboxes() -> Vec<AgentHandle> {
    list_all_sandbox_names()
        .into_iter()
        .filter(|n| n.starts_with("awman-"))
        .map(|name| AgentHandle {
            id: name.clone(),
            image_tag: String::new(),
            name,
            started_at: Utc::now(),
        })
        .collect()
}

/// Every sandbox name `sbx ls` reports (awman-owned or not). Tolerant of both
/// the `--json` mode and the plain table fallback; returns an empty list when
/// `sbx` is unavailable or not logged in.
pub(super) fn list_all_sandbox_names() -> Vec<String> {
    if let Ok(out) = SbxCommand::new(["ls", "--json"]).run_quiet() {
        if out.success() {
            let names = parse_ls_json(&out.stdout);
            if !names.is_empty() || out.stdout.trim() == "[]" {
                return names;
            }
        }
    }
    match SbxCommand::new(["ls"]).run_quiet() {
        Ok(out) if out.success() => parse_ls_table(&out.stdout),
        _ => Vec::new(),
    }
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

fn sandbox_name_exists(name: &str) -> bool {
    list_all_sandbox_names().iter().any(|n| n == name)
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
    let agent = require_agent(&options)?;
    let name = resolve_sandbox_name(&options);
    let kit_dir = kit_dir_for(&agent)?;

    // 1. Per-launch session config (no credential values).
    DSbxSessionConfig::write_for(&options, &options.workspace_dir)?;

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
    let created = if sandbox_name_exists(&name) {
        false
    } else {
        SbxCommand::new(create_argv(&name, &agent, &kit_dir, &options))
            .run_announced(&mut *frontend)?;
        true
    };

    // 4. Launch-time auto-auth: register `env(VAR)` overlay credentials for
    //    supported provider auth vars with sandbox-scoped `sbx secret set`
    //    (scoped secrets apply immediately; global ones only at creation —
    //    awman never sets global secrets), then keychain-resolved credentials
    //    (announced + redacted throughout).
    let auth_result = auth::auto_auth_env_overlays(
        &agent,
        &name,
        &options.env_passthrough,
        &options.env_literal,
        &|key| std::env::var(key).ok(),
        &auth::secret_registered_for_service,
        &mut *frontend,
    )
    .and_then(|_overlay_registered| {
        auth::inject_credentials(&options.agent_credentials, &name, &mut *frontend)
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
        match SbxCommand::new(argv).run_quiet() {
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

    let io = frontend.take_io();
    let seed = stdin_seed(&agent, &options);
    if io.initial_size.is_some() {
        spawn_pty_bridged(name, argv, seed, started_at, handle, io, frontend)
    } else {
        spawn_piped(name, argv, seed, started_at, handle, io, frontend)
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
/// container path builds after the entrypoint: non-interactive
/// subcommand/flag, mode flags (yolo/auto/plan), model flag, then the seeded
/// prompt (a bare positional would be parsed by sbx as a workspace PATH).
/// Only `kind: agent` kits take agent args — awman owns their entrypoint,
/// and sbx forwards everything after `--` to it. `kind: mixin` kits launch
/// through Docker's built-in template (no argv channel): their dynamic
/// config flows through `session.json` and their prompt via stdin
/// ([`stdin_seed`]), so they get no args here.
fn agent_args(agent: &str, options: &ResolvedSandboxOptions) -> Vec<String> {
    use crate::engine::container::options::ModelFlagForm;

    if !matches!(kit_kind_for(agent), SbxKitKind::Agent) {
        return Vec::new();
    }
    let mut args = Vec::new();
    if !options.interactive {
        if let Ok(matrix) = matrix_for(agent) {
            if let Some(flag) = matrix.non_interactive_flag {
                args.push(flag.to_string());
            }
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
    if let Some(prompt) = &options.seeded_prompt {
        args.push(prompt.clone());
    }
    args
}

fn spawn_pty_bridged(
    name: String,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

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

    let mut cmd = CommandBuilder::new(SBX_BIN);
    for arg in &argv {
        cmd.arg(arg);
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

    let (master_arc, bridge) = io_bridge::bridge_pty(io, pair)?;

    let backend = DSbxExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        sandbox_name: name,
        started_at,
        output: bridge.output,
        sink: frontend,
    };
    // Sandbox-class runtimes have no container output tail; failure-log capture
    // is a container-paradigm feature.
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        None,
    ))
}

fn spawn_piped(
    name: String,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(SBX_BIN);
    cmd.args(&argv);
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

    let bridge = io_bridge::bridge_piped(io, &mut child);
    // Drop the injector so the writer task closes the child's stdin after the
    // seeded prompt drains (matches the container piped path).
    drop(bridge.stdin_injector);

    let backend = DSbxExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        sandbox_name: name,
        started_at,
        output: bridge.output,
        sink: frontend,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        None,
    ))
}

// ─── Execution backend ──────────────────────────────────────────────────────

/// How many captured output lines to replay into the sink when the agent
/// exits non-zero.
const FAILURE_TAIL_LINES: usize = 30;

struct DSbxExecution {
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
    sink: Box<dyn AgentFrontend>,
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
        self.sink.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!("sbx exited with code {exit_code}; last output:"),
        });
        for line in lines {
            self.sink.write_message(UserMessage {
                level: MessageLevel::Error,
                text: line,
            });
        }
    }
}

impl ExecutionBackend for DSbxExecution {
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
        let _ = SbxCommand::new(["stop", &self.sandbox_name]).run_quiet();
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let name = self.sandbox_name.clone();
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                let _ = SbxCommand::new(["stop", &name]).run_quiet();
                Ok(())
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sandbox::naming::worktree_hash;
    use crate::engine::sandbox::options::SandboxOption;

    fn opts(list: Vec<SandboxOption>) -> ResolvedSandboxOptions {
        ResolvedSandboxOptions::resolve(list)
    }

    // ─── Identity ──────────────────────────────────────────────────────────

    #[test]
    fn name_is_correct() {
        assert_eq!(DSbxBackend::new().name(), "docker-sbx-experimental");
    }

    #[test]
    fn cli_binary_is_sbx() {
        assert_eq!(DSbxBackend::new().cli_binary(), "sbx");
    }

    // ─── Naming ────────────────────────────────────────────────────────────

    #[test]
    fn worktree_hash_is_deterministic() {
        let p = Path::new("/work/tree/a");
        assert_eq!(worktree_hash(p), worktree_hash(p));
        assert_ne!(worktree_hash(p), worktree_hash(Path::new("/work/tree/b")));
    }

    #[test]
    fn worktree_hash_is_stable_across_releases() {
        // Pinned FNV-1a value: if this changes, every existing persistent
        // sandbox would be orphaned on upgrade. Do not change the algorithm.
        assert_eq!(worktree_hash(Path::new("/work/tree/a")), "f94dc1c0");
    }

    #[test]
    fn resolve_sandbox_name_prefers_explicit() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::SandboxName("awman-custom-claude".into()),
        ]);
        assert_eq!(resolve_sandbox_name(&o), "awman-custom-claude");
    }

    #[test]
    fn resolve_sandbox_name_generates_when_absent() {
        let o = opts(vec![SandboxOption::AgentId("gemini".into())]);
        let name = resolve_sandbox_name(&o);
        assert!(name.starts_with("awman-"));
        assert!(name.ends_with("-gemini"));
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("/work tree/a"), "'/work tree/a'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    // ─── Outside-workspace overlay rejection ───────────────────────────────

    #[test]
    fn outside_workspace_overlay_is_rejected_with_clear_error() {
        use crate::engine::container::options::{OverlayPermission, OverlaySpec};

        use crate::data::message::UserMessage;
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };

        struct NullFrontend;
        impl crate::data::message::UserMessageSink for NullFrontend {
            fn write_message(&mut self, _: UserMessage) {}
            fn replay_queued(&mut self) {}
        }
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

        let tmp = tempfile::tempdir().unwrap();
        let options = ResolvedSandboxOptions::resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
            SandboxOption::ExtraOverlay(OverlaySpec {
                host_path: "/somewhere/else/reference".into(),
                container_path: "/mnt/reference".into(),
                permission: OverlayPermission::ReadOnly,
            }),
        ]);
        match run_interactive(options, Box::new(NullFrontend)) {
            Err(EngineError::Sandbox(msg)) => {
                assert!(
                    msg.contains("/somewhere/else/reference")
                        && msg.contains("outside the workspace"),
                    "error must name the overlay and the reason: {msg}"
                );
            }
            Err(e) => panic!("expected Sandbox error for outside-workspace overlay, got: {e:?}"),
            Ok(_) => panic!("expected Sandbox error for outside-workspace overlay, got Ok"),
        }
    }

    // ─── Launch without auth overlay warns (auto-auth wiring) ──────────────
    //
    // run_interactive must surface the manual-auth warning for a mixin agent
    // launched with no env(...) auth overlay. Auto-auth runs after the
    // `sbx create` step (scoped secrets need an existing sandbox), so a fake
    // sbx must carry the launch that far.

    #[cfg(unix)]
    #[test]
    fn launch_without_auth_overlay_warns_manual_auth() {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use std::sync::{Arc, Mutex};

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
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

        crate::engine::sandbox::dsbx::test_support::with_fake_sbx(
            "#!/bin/sh\ncase \"$1\" in ls) echo '[]';; *) cat > /dev/null 2>&1;; esac\n",
            || {
                let tmp = tempfile::tempdir().unwrap();
                let frontend = Box::new(RecordingFrontend {
                    messages: messages.clone(),
                });
                let options = ResolvedSandboxOptions::resolve(vec![
                    SandboxOption::AgentId("claude".into()),
                    SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
                ]);
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let _ = run_interactive(options, frontend);
                });
            },
        );

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("no env(...) overlay")
                    && m.text.contains("ANTHROPIC_API_KEY")
                    && m.text.contains("Launching anyway")
            }),
            "mixin launch without auth overlays must warn that auth is manual; \
             messages: {:?}",
            *msgs
        );
    }

    // ─── Argv construction ─────────────────────────────────────────────────

    #[test]
    fn create_argv_has_kit_and_positional_workspace() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
            SandboxOption::MemoryGb(8),
        ]);
        let argv = create_argv("awman-h-claude", "claude", Path::new("/kits/claude"), &o);
        assert_eq!(argv[0], "create");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--kit" && w[1] == "/kits/claude"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--name" && w[1] == "awman-h-claude"));
        assert!(argv.windows(2).any(|w| w[0] == "--memory" && w[1] == "8g"));
        // `sbx create` has no `--workspace-dir` flag: the workspace is a
        // positional path immediately after the agent.
        assert!(
            !argv.iter().any(|a| a == "--workspace-dir"),
            "sbx create has no --workspace-dir flag: {argv:?}"
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "claude" && w[1] == "/wt"),
            "workspace must be a positional after the agent: {argv:?}"
        );
        assert_eq!(argv.last().unwrap(), "/wt");
    }

    #[test]
    fn create_argv_without_workspace_omits_positional() {
        let o = opts(vec![SandboxOption::AgentId("claude".into())]);
        let argv = create_argv("awman-h-claude", "claude", Path::new("/kits/claude"), &o);
        assert_eq!(
            argv.last().unwrap(),
            "claude",
            "no workspace set → agent is the final arg (sbx defaults to cwd): {argv:?}"
        );
    }

    #[test]
    fn run_argv_uses_positional_name_without_kit_or_name_flag() {
        let o = opts(vec![SandboxOption::AgentId("claude".into())]);
        let argv = run_argv("awman-h-claude", "claude", &o);
        // `sbx run SANDBOX [-- AGENT_ARGS...]` — `--name` is creation-only and
        // sbx rejects it when the sandbox already exists; the agent must not
        // be appended either (sbx would parse it as a workspace PATH).
        assert_eq!(argv, vec!["run", "awman-h-claude"]);
    }

    #[test]
    fn seeded_prompt_appended_after_delimiter_only_for_agent_kind() {
        // crush is a `kind: agent` kit → prompt appended after `--` (a bare
        // positional would be parsed by sbx as a workspace PATH).
        let crush = opts(vec![
            SandboxOption::AgentId("crush".into()),
            SandboxOption::Interactive(true),
            SandboxOption::SeededPrompt("do the thing".into()),
        ]);
        let argv = run_argv("awman-h-crush", "crush", &crush);
        assert_eq!(argv.last().unwrap(), "do the thing");
        assert_eq!(
            argv[argv.len() - 2],
            "--",
            "prompt must follow the -- delimiter"
        );

        // claude is a `kind: mixin` kit → prompt delivered via stdin.
        let claude = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::SeededPrompt("do the thing".into()),
        ]);
        let argv = run_argv("awman-h-claude", "claude", &claude);
        assert!(!argv.iter().any(|a| a == "do the thing"));
        assert!(!argv.iter().any(|a| a == "--"));
    }

    // ─── WI follow-up: session config re-applied on existing sandboxes ─────
    //
    // `commands.startup` only runs at VM boot. When the sandbox already
    // exists, run_interactive must exec the apply script before `sbx run` so
    // a changed session.json (e.g. a new --model) reaches the agent even if
    // the VM never reboots between launches.

    #[cfg(unix)]
    #[test]
    fn existing_sandbox_reapplies_session_config_before_run() {
        use crate::data::message::{UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
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

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));
        // `ls` reports the sandbox as already existing → no `sbx create`,
        // and the re-apply exec must run before `sbx run`.
        crate::engine::sandbox::dsbx::test_support::with_fake_sbx(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               ls) echo '[{\"name\":\"awman-test-claude\"}]';;\n\
               *) cat > /dev/null 2>&1; exit 0;;\n\
             esac\n",
            || {
                let tmp = tempfile::tempdir().unwrap();
                let options = ResolvedSandboxOptions::resolve(vec![
                    SandboxOption::AgentId("claude".into()),
                    SandboxOption::SandboxName("awman-test-claude".into()),
                    SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
                ]);
                let frontend = Box::new(RecordingFrontend {
                    messages: messages.clone(),
                });
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let _ = run_interactive(options, frontend);
                });
            },
        );

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| {
                m.text.contains("sbx exec")
                    && m.text.contains(APPLY_SCRIPT_VM_PATH)
                    && m.text.contains("WORKDIR=")
            }),
            "existing sandbox must announce the session-config re-apply exec; \
             messages: {:?}",
            *msgs
        );
        assert!(
            !msgs.iter().any(|m| m.text.contains("sbx create")),
            "existing sandbox must not be re-created; messages: {:?}",
            *msgs
        );
    }

    // ─── ls parsing ────────────────────────────────────────────────────────

    #[test]
    fn parse_ls_json_array_and_object() {
        let arr = r#"[{"name":"awman-h-claude"},{"name":"other"}]"#;
        assert_eq!(parse_ls_json(arr), vec!["awman-h-claude", "other"]);
        let obj = r#"{"sandboxes":[{"Name":"awman-x-codex"}]}"#;
        assert_eq!(parse_ls_json(obj), vec!["awman-x-codex"]);
    }

    #[test]
    fn parse_ls_table_skips_header_and_filters_prefix() {
        let table = "NAME            STATUS\nawman-h-claude  running\nnginx           running\n";
        assert_eq!(parse_ls_table(table), vec!["awman-h-claude"]);
    }

    #[test]
    fn parse_ls_json_empty_array() {
        assert_eq!(parse_ls_json("[]"), Vec::<String>::new());
    }

    #[test]
    fn parse_ls_json_invalid_is_empty() {
        assert_eq!(parse_ls_json("not json"), Vec::<String>::new());
    }

    #[test]
    fn parse_ls_table_empty_output_gives_empty_list() {
        assert_eq!(parse_ls_table(""), Vec::<String>::new());
    }

    // ─── Naming determinism ───────────────────────────────────────────────

    #[test]
    fn sandbox_name_same_inputs_same_output_multiple_calls() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/projects/myapp".into()),
        ]);
        let n1 = resolve_sandbox_name(&o);
        let n2 = resolve_sandbox_name(&o);
        assert_eq!(n1, n2, "sandbox name must be deterministic");
    }

    #[test]
    fn sandbox_name_different_agents_differ() {
        let a = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
        ]);
        let b = opts(vec![
            SandboxOption::AgentId("gemini".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
        ]);
        assert_ne!(
            resolve_sandbox_name(&a),
            resolve_sandbox_name(&b),
            "different agents must produce different sandbox names"
        );
    }

    #[test]
    fn sandbox_name_different_workspaces_differ() {
        let a = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt/a".into()),
        ]);
        let b = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt/b".into()),
        ]);
        assert_ne!(
            resolve_sandbox_name(&a),
            resolve_sandbox_name(&b),
            "different workspaces must produce different sandbox names"
        );
    }

    // ─── Argv — all variants ───────────────────────────────────────────────

    #[test]
    fn create_argv_without_memory_omits_memory_flag() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
        ]);
        let argv = create_argv("awman-h-claude", "claude", Path::new("/kits/claude"), &o);
        assert!(
            !argv.iter().any(|a| a == "--memory"),
            "no --memory when MemoryGb not set"
        );
    }

    #[test]
    fn run_argv_without_seeded_prompt_ends_with_sandbox_name() {
        let o = opts(vec![SandboxOption::AgentId("gemini".into())]);
        let argv = run_argv("awman-h-gemini", "gemini", &o);
        assert_eq!(argv, vec!["run", "awman-h-gemini"]);
    }

    // ─── Agent args: model / mode flags / non-interactive (kind: agent) ────

    #[test]
    fn model_flag_appended_after_delimiter_for_agent_kits() {
        use crate::engine::container::options::ModelFlagForm;
        let o = opts(vec![
            SandboxOption::AgentId("crush".into()),
            SandboxOption::Interactive(true),
            SandboxOption::Model {
                flag: ModelFlagForm::Argument("anthropic/claude-opus-4-6".into()),
            },
        ]);
        let argv = run_argv("awman-h-crush", "crush", &o);
        assert_eq!(
            argv,
            vec![
                "run",
                "awman-h-crush",
                "--",
                "--model",
                "anthropic/claude-opus-4-6"
            ],
            "agent-kit model must be delivered as agent args after `--`"
        );
    }

    #[test]
    fn model_shorthand_appended_as_single_token() {
        use crate::engine::container::options::ModelFlagForm;
        let o = opts(vec![
            SandboxOption::AgentId("maki".into()),
            SandboxOption::Interactive(true),
            SandboxOption::Model {
                flag: ModelFlagForm::Shorthand("--model=sonnet".into()),
            },
        ]);
        let argv = run_argv("awman-h-maki", "maki", &o);
        assert_eq!(argv, vec!["run", "awman-h-maki", "--", "--model=sonnet"]);
    }

    #[test]
    fn model_not_appended_for_mixin_kits() {
        use crate::engine::container::options::ModelFlagForm;
        // Mixins take no agent args — model flows through session.json and
        // the kit's apply script instead.
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::Interactive(true),
            SandboxOption::Model {
                flag: ModelFlagForm::Argument("claude-opus-4-6".into()),
            },
        ]);
        let argv = run_argv("awman-h-claude", "claude", &o);
        assert_eq!(argv, vec!["run", "awman-h-claude"]);
    }

    #[test]
    fn mode_flags_appended_before_model_and_prompt_for_agent_kits() {
        use crate::engine::container::options::ModelFlagForm;
        let o = opts(vec![
            SandboxOption::AgentId("crush".into()),
            SandboxOption::Interactive(true),
            SandboxOption::AgentModeFlags(vec!["--yolo".into()]),
            SandboxOption::Model {
                flag: ModelFlagForm::Argument("gpt-5".into()),
            },
            SandboxOption::SeededPrompt("fix it".into()),
        ]);
        let argv = run_argv("awman-h-crush", "crush", &o);
        assert_eq!(
            argv,
            vec![
                "run",
                "awman-h-crush",
                "--",
                "--yolo",
                "--model",
                "gpt-5",
                "fix it"
            ]
        );
    }

    #[test]
    fn non_interactive_prepends_matrix_flag_for_agent_kits() {
        // crush's non-interactive shape is the `run` subcommand — it must be
        // the first agent arg, before flags and the prompt, mirroring the
        // container path's `entrypoint_for`.
        let o = opts(vec![
            SandboxOption::AgentId("crush".into()),
            SandboxOption::Interactive(false),
            SandboxOption::SeededPrompt("do it".into()),
        ]);
        let argv = run_argv("awman-h-crush", "crush", &o);
        assert_eq!(argv, vec!["run", "awman-h-crush", "--", "run", "do it"]);

        // maki has no non-interactive flag → no extra token.
        let o = opts(vec![
            SandboxOption::AgentId("maki".into()),
            SandboxOption::Interactive(false),
            SandboxOption::SeededPrompt("do it".into()),
        ]);
        let argv = run_argv("awman-h-maki", "maki", &o);
        assert_eq!(argv, vec!["run", "awman-h-maki", "--", "do it"]);
    }

    // Seeded prompts for mixin vs agent kit agents (full table check)
    #[test]
    fn all_mixin_agents_do_not_append_prompt_to_argv() {
        let mixin_agents = ["claude", "codex", "gemini", "copilot", "opencode"];
        for agent in &mixin_agents {
            let o = opts(vec![
                SandboxOption::AgentId((*agent).into()),
                SandboxOption::SeededPrompt("the prompt".into()),
            ]);
            let argv = run_argv("awman-h-x", agent, &o);
            assert!(
                !argv.iter().any(|a| a == "the prompt"),
                "mixin agent {agent}: seeded prompt must NOT be appended to argv"
            );
        }
    }

    #[test]
    fn all_agent_kit_agents_append_prompt_after_delimiter() {
        let agent_kits = ["antigravity", "crush", "maki", "cline"];
        for agent in &agent_kits {
            let o = opts(vec![
                SandboxOption::AgentId((*agent).into()),
                SandboxOption::Interactive(true),
                SandboxOption::SeededPrompt("the prompt".into()),
            ]);
            let argv = run_argv("awman-h-x", agent, &o);
            assert_eq!(
                argv.last().unwrap(),
                "the prompt",
                "agent-kit {agent}: seeded prompt must be the final agent arg"
            );
            assert_eq!(
                argv[argv.len() - 2],
                "--",
                "agent-kit {agent}: prompt must follow the -- delimiter or sbx \
                 parses it as a workspace PATH"
            );
        }
    }

    // ─── CPU limit warning via run_interactive ────────────────────────────
    //
    // `run_interactive` warns about CpuLimit before it tries to spawn `sbx`.
    // The warning must be in the sink even when `sbx` is not installed.
    // We use a thread-safe shared message log via Arc<Mutex<_>> so we can
    // inspect it after run_interactive takes ownership of the frontend.

    #[test]
    fn cpu_limit_produces_warning_in_run_interactive() {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use std::sync::{Arc, Mutex};

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
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

        let tmp = tempfile::tempdir().unwrap();
        let frontend = Box::new(RecordingFrontend {
            messages: messages.clone(),
        });
        let options = ResolvedSandboxOptions::resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
            SandboxOption::CpuLimit(2.0),
        ]);
        // The warning is written before the spawn attempt, whether or not an
        // `sbx` binary is reachable. A runtime is needed because a parallel
        // test's fake sbx can make the launch reach the bridge's task spawns.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = run_interactive(options, frontend);
        });

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("CPU")
                    && m.text.contains("not supported")
            }),
            "must produce a Warning about unsupported CPU limits; messages: {:?}",
            *msgs
        );
    }

    // ─── Failed launch replays sbx output to the sink ──────────────────────
    //
    // When `sbx run` exits non-zero (kit compose error, login failure, …),
    // wait_blocking must replay the captured output into the message sink so
    // the failure is diagnosable from the execution window without re-running
    // sbx by hand.

    #[cfg(unix)]
    #[test]
    fn non_zero_exit_replays_sbx_output_to_sink() {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
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

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));
        // `ls` reports no sandboxes, `create` succeeds, only `run` fails — the
        // failure being replayed must be the agent launch itself.
        crate::engine::sandbox::dsbx::test_support::with_fake_sbx(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               ls) echo '[]';;\n\
               run) echo 'ERROR: failed to run sandbox: compose kits: boom' >&2; exit 1;;\n\
               *) cat > /dev/null 2>&1; exit 0;;\n\
             esac\n",
            || {
                let tmp = tempfile::tempdir().unwrap();
                let options = ResolvedSandboxOptions::resolve(vec![
                    SandboxOption::AgentId("claude".into()),
                    SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
                ]);
                let frontend = Box::new(RecordingFrontend {
                    messages: messages.clone(),
                });
                let rt = tokio::runtime::Runtime::new().unwrap();
                let exit = rt.block_on(async {
                    let mut execution = run_interactive(options, frontend)
                        .expect("piped launch must spawn the fake sbx");
                    execution.wait().await.expect("wait must succeed")
                });
                assert_eq!(exit.exit_code, 1);
            },
        );

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| {
                m.level == MessageLevel::Error && m.text.contains("sbx exited with code 1")
            }),
            "must announce the failed exit with its code; messages: {:?}",
            *msgs
        );
        assert!(
            msgs.iter().any(|m| {
                m.level == MessageLevel::Error
                    && m.text.contains("failed to run sandbox: compose kits: boom")
            }),
            "must replay sbx's own error output; messages: {:?}",
            *msgs
        );
    }

    // ─── WI-0091: stdin seed routing (prompt delivered exactly once) ──────

    #[test]
    fn stdin_seed_present_for_mixin_kits_only() {
        let prompt = "fix the bug";
        for agent in ["claude", "codex", "gemini", "copilot", "opencode"] {
            let o = opts(vec![
                SandboxOption::AgentId(agent.into()),
                SandboxOption::SeededPrompt(prompt.into()),
            ]);
            assert_eq!(
                stdin_seed(agent, &o).as_deref(),
                Some(prompt),
                "mixin {agent}: stdin is the only delivery path, seed must be present"
            );
        }
        for agent in ["antigravity", "crush", "maki", "cline"] {
            let o = opts(vec![
                SandboxOption::AgentId(agent.into()),
                SandboxOption::SeededPrompt(prompt.into()),
            ]);
            assert_eq!(
                stdin_seed(agent, &o),
                None,
                "agent-kit {agent}: prompt is positional, stdin seed would deliver it twice"
            );
        }
    }

    #[test]
    fn stdin_seed_none_without_prompt() {
        let o = opts(vec![SandboxOption::AgentId("claude".into())]);
        assert_eq!(stdin_seed("claude", &o), None);
    }

    // ─── WI-0091: unsupported-feature notes surfaced as warnings ──────────

    #[test]
    fn unsupported_notes_produce_warnings_in_run_interactive() {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use std::sync::{Arc, Mutex};

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
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

        let tmp = tempfile::tempdir().unwrap();
        let frontend = Box::new(RecordingFrontend {
            messages: messages.clone(),
        });
        let options = ResolvedSandboxOptions::resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
            SandboxOption::UnsupportedNote(
                "skill mounts are not supported under the sandbox runtime".into(),
            ),
        ]);
        // The warning is written before the spawn attempt, whether or not an
        // `sbx` binary is reachable. A runtime is needed because a parallel
        // test's fake sbx can make the launch reach the bridge's task spawns.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = run_interactive(options, frontend);
        });

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter()
                .any(|m| { m.level == MessageLevel::Warning && m.text.contains("skill mounts") }),
            "unsupported notes must surface as warnings before launch; messages: {:?}",
            *msgs
        );
    }
}
