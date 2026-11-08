//! Docker backend — `pub(super)`. Concrete type is invisible outside
//! `src/engine/container/`.
//!
//! Builds a `docker run` argv from `ResolvedContainerOptions`, spawns the
//! subprocess, and captures the exit code.
//!
//! All container I/O is mediated through the `AgentIo` channels
//! provided by the frontend. When the frontend provides PTY fields
//! (`initial_size`/`resize` are `Some`), the engine opens a PTY via
//! `portable-pty`. Otherwise it uses `Stdio::piped()`.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::data::message::UserMessageSink;
use crate::engine::agent_runtime::background::ExecOutput;
use crate::engine::agent_runtime::{Capabilities, ImageImportRequest, ImportedImage};
use crate::engine::container::runtime::CONTAINER_CAPABILITIES;
use crate::engine::container::{background, host_cli_backend};

use std::process::{Command, Stdio};

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, AgentStats, ExecutionBackend,
};
use crate::engine::container::agent_argv::AgentArgv;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::host_env_denylist::{is_host_terminal_var, scrub_run_env};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::container::process::{ContainerCli, ContainerInstance};
use crate::engine::credential_refresh::register_container_leases;
use crate::engine::error::EngineError;

const AGENT_LIST_FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";

// The fixture overrides only listing calls on its own thread.
fn listing_command(cli: ContainerCli) -> Command {
    #[cfg(all(test, unix))]
    if let Some(path) = docker_listing_tests::CLI.with(|cli| cli.borrow().clone()) {
        // The fixture is a freshly written shell script. Concurrent test
        // forks can briefly inherit its write descriptor and make execve
        // return ETXTBSY; run it through the stable system shell instead.
        let mut command = Command::new("/bin/sh");
        command.arg(path);
        return command;
    }
    cli.command()
}

/// Query Docker once for each ownership route, then merge by id. The name
/// query keeps compatibility with legacy awman containers created before
/// the ownership label was added.
fn list_owned_containers(
    cli: ContainerCli,
    stopped: bool,
) -> Result<Vec<AgentHandle>, EngineError> {
    let mut handles = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ownership_filter in ["label=awman=true", "name=awman-"] {
        let mut args = vec!["ps", "--no-trunc"];
        if stopped {
            args.push("-a");
        }
        args.extend(["--filter", ownership_filter]);
        if stopped {
            args.extend(["--filter", "status=exited", "--filter", "status=dead"]);
        }
        args.extend(["--format", AGENT_LIST_FORMAT]);
        let output = crate::engine::host_cli::checked_output(
            listing_command(cli.clone())
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
            "docker",
        )?;
        handles.extend(parse_agent_listing(&output.stdout, &mut seen));
    }
    Ok(handles)
}

/// Parse Docker's tab-separated rows for every agent listing. Incomplete
/// rows and rows without an id or name are malformed and ignored.
fn parse_agent_listing(
    output: &[u8],
    seen: &mut std::collections::HashSet<String>,
) -> Vec<AgentHandle> {
    let stdout = String::from_utf8_lossy(output);
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let id = parts.next()?;
            let name = parts.next()?;
            let image_tag = parts.next()?;
            let created = parts.next()?;
            if id.is_empty() || name.is_empty() || !seen.insert(id.to_string()) {
                return None;
            }
            let started_at = chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|error| {
                    tracing::warn!(container_id = id, created_at = created, %error,
                        "invalid Docker creation timestamp; using observation time");
                    chrono::Utc::now()
                });
            Some(AgentHandle {
                id: id.to_string(),
                image_tag: image_tag.to_string(),
                name: name.to_string(),
                started_at,
            })
        })
        .collect()
}

fn query_agent_listing(cli: ContainerCli, args: &[&str]) -> Result<Vec<AgentHandle>, EngineError> {
    let output = crate::engine::host_cli::checked_output(
        listing_command(cli.clone())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        "docker",
    )?;
    Ok(parse_agent_listing(
        &output.stdout,
        &mut std::collections::HashSet::new(),
    ))
}

/// Docker label applied to every awman-spawned container so `list_running`
/// can filter to ours. Lives on `ContainerCli` so the shared process module
/// and this backend cannot drift apart.
const AWMAN_LABEL: &str = ContainerCli::DOCKER.label;

#[derive(Debug, Default)]
pub(super) struct DockerBackend {
    env: Option<crate::data::config::env::EnvSnapshot>,
}
#[cfg(test)]
#[allow(non_upper_case_globals)]
pub(super) const DockerBackend: DockerBackend = DockerBackend { env: None };
impl DockerBackend {
    pub(super) fn new(env: crate::data::config::env::EnvSnapshot) -> Self {
        Self { env: Some(env) }
    }
    fn cli(&self) -> ContainerCli {
        let cli = ContainerCli::DOCKER;
        match &self.env {
            Some(env) => cli.in_env(env.clone()),
            None => cli,
        }
    }
}

impl ContainerBackend for DockerBackend {
    fn build(
        &self,
        mut options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        if options.host_env.is_none() {
            options.host_env = self.env.clone();
        }
        let cli = options
            .host_env
            .as_ref()
            .map(|env| self.cli().in_env(env.clone()))
            .unwrap_or_else(|| self.cli());
        let image = options
            .image
            .clone()
            .ok_or_else(|| EngineError::MissingRequiredOption("Image".into()))?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        // Refuse a launch whose agent argv cannot be built (a tool list the
        // agent has no flag for) before any lease or child exists.
        AgentArgv::from_options(&options)?;
        // Register a refresh lease for every file-delivered credential BEFORE
        // the container is built and its child spawned — the single choke point
        // (INV-6). No-op for every launch that carries no such credential.
        let leases = register_container_leases(&options, &name.0);
        // Docker's CLI has a native `attach` verb, so no attach-rendezvous
        // hook is needed (`ContainerBackend::attach` below opens
        // `docker attach`). Apple, which has no such verb, passes one.
        Ok(Box::new(ContainerInstance::new(
            cli, image, name, options, leases, None,
        )))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        list_owned_containers(self.cli(), false)
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        list_owned_containers(self.cli(), false)
    }

    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        // Keep the ownership label and the legacy name-prefix route in sync.
        list_owned_containers(self.cli(), true)
    }

    fn list_dangling_images(
        &self,
    ) -> Result<Vec<crate::engine::container::runtime::ContainerImageInfo>, EngineError> {
        use crate::engine::container::runtime::ContainerImageInfo;
        let format = "{{.ID}}\t{{.Repository}}:{{.Tag}}\t{{.Size}}";
        let output = self
            .cli()
            .command()
            .args([
                "images",
                "--filter",
                "label=awman=true",
                "--filter",
                "dangling=true",
                "--format",
                format,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            // Docker missing or query failed: return an empty list. Callers use
            // `is_available()` to decide whether Docker is reachable at all.
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut images: Vec<ContainerImageInfo> = Vec::new();
        for line in stdout.lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let id = parts[0].to_string();
            if id.is_empty() {
                continue;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            images.push(ContainerImageInfo {
                id,
                repo_tag: parts[1].to_string(),
                size: parts[2].to_string(),
            });
        }
        Ok(images)
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        let output = crate::engine::host_cli::checked_output(
            self.cli()
                .command()
                .args([
                    "stats",
                    "--no-stream",
                    "--format",
                    "{{.Name}}|{{.CPUPerc}}|{{.MemUsage}}",
                    &handle.id,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
            "docker",
        )?;
        if !output.status.success() {
            return Err(EngineError::Container(format!(
                "docker stats failed for container {}",
                handle.name
            )));
        }
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        parse_stats_line(&line, &handle.name)
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        Ok(Box::new(AttachInstance {
            cli: self.cli(),
            handle: handle.clone(),
        }))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        query_agent_listing(
            self.cli(),
            &[
                "ps",
                "--no-trunc",
                "--filter",
                &format!("name={prefix}"),
                "--format",
                AGENT_LIST_FORMAT,
            ],
        )
    }

    fn name(&self) -> &'static str {
        "docker"
    }

    fn display_name(&self) -> &'static str {
        "Docker"
    }

    fn capabilities(&self) -> &'static Capabilities {
        &CONTAINER_CAPABILITIES
    }

    fn reattach_after_owner_exit(&self) -> bool {
        true
    }

    fn is_available(&self) -> Result<(), EngineError> {
        host_cli_backend::cli_is_available(self.cli(), &["info", "--format", "{{.ServerVersion}}"])
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        host_cli_backend::cli_stop_checked(self.cli(), &handle.id)
    }

    fn remove_agent(&self, id: &str) -> Result<(), EngineError> {
        host_cli_backend::cli_remove(self.cli(), "rm", id)
    }

    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        host_cli_backend::cli_image_exists(self.cli(), tag)
    }

    fn run_probe(
        &self,
        request: &crate::engine::container::backend::ProbeRequest<'_>,
    ) -> Result<String, EngineError> {
        host_cli_backend::cli_run_probe(self.cli(), request)
    }

    fn image_id(&self, tag: &str) -> Result<Option<String>, EngineError> {
        host_cli_backend::cli_image_id(self.cli(), tag)
    }

    fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        request.require_runtime_build()?;
        let tag = request.reference.as_str();
        let dockerfile = request.dockerfile.as_path();
        let context = request.context.as_path();
        let no_cache = request.no_cache;

        host_cli_backend::cli_build_image(self.cli(), tag, dockerfile, context, no_cache, on_line)
    }

    fn import_image(
        &self,
        _request: &ImageImportRequest,
        _sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.name(),
            operation: "image import",
        })
    }

    fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        host_cli_backend::cli_remove(self.cli(), "rmi", id)
    }

    /// Docker and Apple Containers have no builtin launch settings; the
    /// request's `builtin_launch` is ignored.
    fn start_background(
        &self,
        request: &background::BackgroundLaunchRequest<'_>,
    ) -> Result<String, EngineError> {
        background::default_start_background(
            self.cli(),
            request.name,
            request.image,
            request.workdir,
            request.env,
            request.overlays,
            request.secret_env,
        )
    }

    fn exec_in_background(
        &self,
        id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        background::default_exec_in_background(self.cli(), id, command, working_dir, env)
    }

    fn exec_in_background_streaming(
        &self,
        request: &crate::engine::container::background::BackgroundExecRequest<'_>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        let id = request.id;
        let command = request.command;
        let working_dir = request.working_dir;
        let env = request.env;

        background::default_exec_in_background_streaming(
            self.cli(),
            id,
            command,
            working_dir,
            env,
            on_line,
        )
    }

    fn stop_and_remove(&self, id: &str) -> Result<(), EngineError> {
        background::default_stop_and_remove(self.cli(), id);
        Ok(())
    }

    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError> {
        Ok(docker_image_home_dir(self.cli(), tag))
    }
}

/// Read `HOME` from `tag`'s image config via `docker image inspect`. `None` when
/// the image is missing, the CLI is unreachable, or no `HOME` is declared.
fn docker_image_home_dir(cli: ContainerCli, tag: &str) -> Option<String> {
    // Print one env entry per line so we can scan for `HOME=…` without
    // parsing JSON. `docker image inspect` exits 0 even when User/Env are
    // empty; the format expansion just produces nothing then.
    let output = cli
        .command()
        .args([
            "image",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            tag,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(rest) = line.strip_prefix("HOME=") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

// ─── Attach (re-attach into a foreign, already-running container) ───────────

/// `BridgeConfig` for an attach session. Identical to `bridge_config_for`
/// except `cancel_on_grace_expired` is `None`: the grace-expiry cancel issues
/// `docker stop <name>`, which would be wrong for a container this process
/// does not own. An attach session's own exit is authoritative.
pub(super) fn attach_bridge_config(
    grace_timeout: std::time::Duration,
    stuck_timeout: std::time::Duration,
) -> crate::engine::container::io_bridge::BridgeConfig {
    crate::engine::container::io_bridge::BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: std::time::Duration::ZERO,
        cancel_on_grace_expired: None,
        output_tail: std::sync::Arc::new(
            crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
        ),
        output_broadcast: None,
        terminal: None,
        program_status: Default::default(),
    }
}

/// Kill only the local runtime attach client process. Never
/// touches the target container — attach must never stop a container another
/// process owns.
pub(super) fn kill_local_exec(pid: Option<u32>) {
    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(not(unix))]
        {
            let _ = pid; // best-effort no-op; the container is never touched
        }
    }
}

/// Configured-but-not-running attach handle. `run_with_frontend` opens a
/// `docker attach` session to PID 1's existing terminal and bridges it through
/// the same PTY/piped machinery a fresh `docker run` uses.
struct AttachInstance {
    cli: ContainerCli,
    handle: AgentHandle,
}

impl AgentInstance for AttachInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        let started_at = chrono::Utc::now();
        let handle = self.handle.clone();

        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: handle.name.clone(),
            },
        );

        let grace_timeout = frontend.grace_timeout();
        let stuck_timeout = frontend.stuck_timeout();
        let io = frontend.take_io();
        let mut bridge_cfg = attach_bridge_config(grace_timeout, stuck_timeout);
        bridge_cfg.program_status = frontend.program_status_config();

        // `docker attach` reconnects to the container's primary process (the
        // agent launched by `docker run -it`), rather than creating a sibling
        // shell with `docker exec`. The outer bridge PTY carries terminal size
        // and resize signals to the attach client.
        //
        // No `--sig-proxy=false`: every awman agent container has a TTY, where
        // signal proxying is inapplicable — and newer Docker CLIs reject the
        // flag outright for TTY containers, which made the attach client exit
        // immediately and the TUI attach session collapse on arrival. Session
        // teardown never relied on it either (`kill_local_exec` SIGKILLs the
        // local client, and SIGKILL is never proxied).
        let argv = vec!["attach".to_string(), handle.id.clone()];
        if io.initial_size.is_some() {
            return spawn_pty_bridged_attach(
                self.cli.clone(),
                io,
                argv,
                started_at,
                handle,
                bridge_cfg,
            );
        }

        spawn_piped_attach(self.cli.clone(), io, argv, started_at, handle, bridge_cfg)
    }
}

/// Spawn `docker attach` via `portable-pty` and bridge the PTY master to the
/// frontend's `AgentIo`. Mirrors `spawn_pty_bridged_docker` but substitutes
/// the exec argv and produces an `AttachExecution`.
fn spawn_pty_bridged_attach(
    cli: ContainerCli,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
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
        .map_err(|e| EngineError::Container(format!("openpty: {e}")))?;

    let snapshot = cli.snapshot();
    let mut cmd = crate::engine::host_cli::pty_command(&snapshot, "docker");
    for arg in &argv {
        cmd.arg(arg);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn docker attach via pty: {e}")))?;
    let child_pid = child.process_id();

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let backend = AttachExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        child_pid,
        started_at,
        output_tail: bridge.output_tail.clone(),
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    )
    .with_program_status(bridge.program_status))
}

/// Spawn `docker attach` with piped stdio and bridge through `AgentIo`. Mirrors
/// `spawn_piped_docker` but substitutes the exec argv and produces an
/// `AttachExecution`.
fn spawn_piped_attach(
    cli: ContainerCli,
    io: crate::engine::agent_runtime::frontend::AgentIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::AgentHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<AgentExecution, EngineError> {
    let mut cmd = cli.command();
    cmd.args(&argv);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: "docker".into(),
            }
        } else {
            EngineError::Container(format!("spawn docker attach: {e}"))
        }
    })?;
    let child_pid = Some(child.id());

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);
    // Headless attach: close the child's stdin after the bridge wires up
    // so a shell that reads to EOF exits cleanly (matches the run piped path).
    drop(bridge.stdin_injector);

    let backend = AttachExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        child_pid,
        started_at,
        output_tail: bridge.output_tail.clone(),
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    )
    .with_program_status(bridge.program_status))
}

/// Execution backend for an attach session. Identical to `DockerExecution`
/// except cancellation kills only the local `docker attach` client — never
/// `docker stop <name>`, because the target container belongs to another
/// process.
struct AttachExecution {
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// PID of the local `docker attach` client, captured at spawn.
    child_pid: Option<u32>,
    started_at: chrono::DateTime<chrono::Utc>,
    output_tail: std::sync::Arc<crate::engine::agent_runtime::OutputTail>,
}

impl AttachExecution {
    fn check_establishment(&self, exit_code: i32) -> Result<(), EngineError> {
        if exit_code != 0 {
            self.output_tail
                .wait_for_writers(std::time::Duration::from_secs(1));
            let output = self.output_tail.snapshot_text();
            if output.lines().any(|line| {
                let line = line.to_ascii_lowercase();
                line.contains("error response from daemon:")
                    || line.contains("no such container")
                    || line.starts_with("cannot connect to the docker daemon")
            }) {
                return Err(EngineError::Container(format!(
                    "docker attach failed: {}",
                    output.trim()
                )));
            }
        }
        Ok(())
    }
}

impl ExecutionBackend for AttachExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait docker attach (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            self.check_establishment(exit_code)?;
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }

        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Container("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait docker attach: {e}")))?;

        clear_stdio_nonblocking();

        let exit_code = status.code().unwrap_or(-1);
        self.check_establishment(exit_code)?;
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;

        Ok(AgentExitInfo {
            exit_code,
            signal,
            started_at: self.started_at,
            ended_at: chrono::Utc::now(),
        })
    }

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.stdin_injector {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        kill_local_exec(self.child_pid);
        Ok(())
    }

    fn cancel_handle(&self) -> crate::engine::agent_runtime::execution::CancelHandle {
        let pid = self.child_pid;
        crate::engine::agent_runtime::execution::CancelHandle::new(move || {
            kill_local_exec(pid);
            Ok(())
        })
    }
}

/// The declared `env()` passthrough names that actually resolve on this host,
/// paired with their values.
///
/// This is the one place the passthrough gate is decided, so `build_run_argv`
/// (which emits the name-only `-e NAME`) and the spawn paths in `process.rs`
/// (which set the value on the child's environment) can never disagree about
/// which names are being passed through.
///
/// Resolution goes through [`host_var`](crate::data::config::env::host_var), so
/// a squad daemon sees values pushed into its in-memory overlay as well as its
/// own process environment. A name that resolves to `Some("")` is still
/// included: that is bit-for-bit today's `std::env::var(..).is_ok()` gate, and
/// changing it would silently alter CLI/TUI behaviour for a variable that is
/// deliberately set to the empty string.
fn resolve_env_passthrough_raw(options: &ResolvedContainerOptions) -> Vec<(String, String)> {
    let mut values: Vec<_> = options
        .env_passthrough
        .iter()
        .filter(|envvar| is_env_var_name(&envvar.0))
        .filter_map(|envvar| {
            options
                .host_env
                .as_ref()
                .map_or_else(
                    || crate::data::config::env::host_var(&envvar.0),
                    |env| env.get(&envvar.0).map(str::to_owned),
                )
                .map(|value| (envvar.0.clone(), value))
        })
        .collect();
    for (key, value) in &options.env_overrides {
        if crate::data::config::workflows::valid_env_name(key) {
            values.retain(|(name, _)| name != key);
            values.push((key.clone(), value.expose().to_string()));
        }
    }
    values
}

/// [`resolve_env_passthrough_raw`] minus host-terminal variables (WI 0141 §3).
///
/// `TERM` and `COLORTERM` are also removed here: the container gets them from
/// [`terminal_env_literals`], never from the host.
pub(super) fn resolve_env_passthrough(options: &ResolvedContainerOptions) -> Vec<(String, String)> {
    resolve_env_passthrough_raw(options)
        .into_iter()
        .filter(|(name, _)| !is_screened_run_var(name))
        .collect()
}

/// A name the run must never set from the host or an option source: a
/// host-terminal variable, or `TERM`/`COLORTERM`, which only the normalised
/// [`scrubbed_env_literals`] may set (WI 0141 §3).
fn is_screened_run_var(name: &str) -> bool {
    is_host_terminal_var(name) || name == "TERM" || name == "COLORTERM"
}

/// The agent credentials minus screened names. Credential env vars come from
/// agent definitions and run options, so they pass the same deny-list as
/// every other source; ordinary credential names are unaffected. Used by the
/// argv builder and by every spawn path that sets their values.
pub(super) fn screened_agent_credentials(
    options: &ResolvedContainerOptions,
) -> Vec<(String, String)> {
    options
        .agent_credentials
        .iter()
        .filter(|(name, _)| !is_screened_run_var(name))
        .cloned()
        .collect()
}

/// The `KEY=VALUE` literals for a run: the options' `env_literal`s plus the
/// terminal variables, scrubbed by the one deny-list call for this backend.
pub(super) fn scrubbed_env_literals(options: &ResolvedContainerOptions) -> Vec<(String, String)> {
    let literals = options
        .env_literal
        .iter()
        .map(|l| (l.key.clone(), l.value.clone()))
        .collect();
    scrub_run_env(
        literals,
        options
            .host_env
            .as_ref()
            .map_or_else(
                || crate::data::config::env::host_var("COLORTERM"),
                |env| env.get("COLORTERM").map(str::to_owned),
            )
            .as_deref(),
    )
    .kept
}

/// The user-facing notice for host-terminal variables an overlay or the agent
/// asked for that were not passed, or `None`. The run path writes it to the
/// frontend; [`build_run_argv`] also logs it.
pub fn host_terminal_env_notice(options: &ResolvedContainerOptions) -> Option<String> {
    let mut requested = resolve_env_passthrough_raw(options);
    requested.extend(
        options
            .env_literal
            .iter()
            .map(|l| (l.key.clone(), l.value.clone())),
    );
    requested.extend(options.agent_credentials.iter().cloned());
    if let Some((env_var, _, container_path)) = &options.system_prompt_env_file {
        requested.push((env_var.clone(), container_path.display().to_string()));
    }
    scrub_run_env(requested, None).notice()
}

/// Whether a passthrough name is a usable environment variable name.
///
/// `env()` validates this at the front door, but a task created before that
/// validation existed still carries whatever it was given, and these names now
/// reach `Command::env` on the spawn paths: a `=` there produces a malformed
/// environment entry rather than a passthrough, and a NUL fails the spawn
/// outright. Such a name could never have worked, so dropping it is not a
/// behaviour change anyone can depend on.
fn is_env_var_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Translate `ResolvedContainerOptions` into a `docker run` argv (without the
/// leading `docker` binary).
pub(super) fn build_run_argv(
    name: &ContainerName,
    image: &ImageRef,
    options: &ResolvedContainerOptions,
) -> Result<Vec<String>, EngineError> {
    let mut args: Vec<String> = vec!["run".into()];
    if options.remove_on_exit {
        args.push("--rm".into());
    }
    if options.acp {
        // ACP launch: piped stdio, never a PTY — even for an interactive run.
        // A newline-delimited JSON-RPC 2.0 channel must never pass through a
        // PTY's cooked-mode / echo / ANSI layer, so we allocate `-i` (stdin
        // attached, no `-t`). This adds NO new host exposure — no ports, no
        // `--network`, no new mounts (aspec/architecture/security.md): the
        // JSON-RPC bytes ride the exact stdio pipes `-i` already wires up.
        args.push("-i".into());
    } else if options.interactive {
        // Interactive runs always allocate a PTY. When a seeded prompt is also
        // present, the prompt is appended as a positional argv arg below so the
        // agent receives it without piping; stdin stays inherited for the user.
        args.push("-it".into());
    } else if options.seeded_prompt.is_some() {
        // Headless with a seeded prompt: pipe stdin so we can write the
        // prompt, then close it. No PTY — allocating one fails when there is no
        // host TTY (ENOTTY / "Inappropriate ioctl for device").
        args.push("-i".into());
    }

    args.push("--name".into());
    args.push(name.0.clone());

    // Standard awman label so `list_running` can filter.
    args.push("--label".into());
    args.push(AWMAN_LABEL.into());

    // Caller-supplied labels — emitted one `--label key=value` each, in the
    // order they were ingested, immediately after the hardcoded `awman=true`.
    // Lets `list_running` attribute containers to a specific awman session
    // (`awman.session=<id>`) and squad mark its background agents
    // (`awman.squad.task=<name>`). These are for human `docker ps`
    // inspection only — awman never reads a label back.
    for (key, value) in &options.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}"));
    }

    // Working dir.
    if let Some(wd) = &options.working_dir {
        args.push("-w".into());
        args.push(wd.display().to_string());
    }

    // Overlays / volume mounts.
    for overlay in &options.overlays {
        args.push("-v".into());
        let suffix = match overlay.permission {
            crate::engine::container::options::OverlayPermission::ReadOnly => ":ro",
            crate::engine::container::options::OverlayPermission::ReadWrite => "",
        };
        args.push(format!(
            "{}:{}{}",
            overlay.host_path.display(),
            overlay.container_path.display(),
            suffix,
        ));
    }

    // Env passthrough — only emit when the variable resolves on the host. Like
    // the credential block just below, this is name-only (`-e NAME`): argv is
    // world-readable through `/proc/<pid>/cmdline` (or `ps`), so a host value
    // that may be a secret must never be written into it. The value reaches the
    // container the way an agent credential's does — it is set on the spawned
    // CLI child's own environment (see [`resolve_env_passthrough`] and the
    // spawn paths in `process.rs`), and the container-runtime CLI resolves a
    // name-only `-e` from its own process env.
    //
    // The gate is `host_var`, not `std::env::var`: inside the squad daemon a
    // task's `env()` value arrives over the authenticated socket and lives in
    // the Layer 0 daemon overlay, never in the daemon's real process
    // environment. Gating on `std::env::var` there emits no `-e` at all and the
    // container starts silently without the variable — the regression WI 0116
    // exists to end. Injecting the resolved value onto the child is what makes
    // the overlay case work, because ambient inheritance cannot.
    for (name, _) in resolve_env_passthrough(options) {
        args.push("-e".into());
        args.push(name);
    }
    // Env literals — unlike env_passthrough above, these keep the `KEY=VALUE`
    // form. Literal values are constants awman itself supplies, never a user secret, so there is nothing here
    // that argv's world-readability could expose.
    // Host-terminal variables are dropped and TERM/COLORTERM normalised here
    // (WI 0141 §3). Every env source (passthrough, literals, credentials,
    // the system-prompt variable) passes the same screen.
    if let Some(notice) = host_terminal_env_notice(options) {
        tracing::warn!("{notice}");
    }
    for (key, value) in scrubbed_env_literals(options) {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    // Agent credentials are env-vars by another name. Emit the NAME ONLY
    // (`-e KEY`); the value is set on the spawned CLI child's own environment
    // (see the spawn paths below) and the container-runtime CLI resolves a
    // name-only `-e` from its process env. This keeps the secret value out of
    // the argument vector, so it never appears in `ps` / `/proc/<pid>/cmdline`
    // while the client process runs. Both the docker CLI and the Apple
    // `container` CLI (which shares this argv builder) support this form.
    for (k, _v) in screened_agent_credentials(options) {
        args.push("-e".into());
        args.push(k);
    }

    // Allow Docker socket: mount and add docker group.
    if options.allow_docker {
        let socket = docker_socket_path();
        let s = socket.to_string_lossy().to_string();
        #[cfg(target_os = "windows")]
        {
            args.push("--mount".into());
            args.push(format!("type=npipe,source={},target={}", s, s));
        }
        #[cfg(not(target_os = "windows"))]
        {
            args.push("-v".into());
            args.push(format!("{s}:{s}"));
            // Add the host's docker group GID so the container user can talk
            // to the daemon. Best-effort: skip when the group can't be found.
            if let Some(gid) = host_docker_group_gid() {
                args.push("--group-add".into());
                args.push(gid.to_string());
            }
        }
    }

    // Container CPU/memory limits.
    if let Some(cpu) = options.cpu {
        args.push("--cpus".into());
        args.push(format!("{}", cpu.0));
    }
    if let Some(mem) = options.memory {
        args.push("--memory".into());
        args.push(format!("{}m", mem.0));
    }

    // System prompt file: Docker-side mount (before the image arg).
    if let Some((host_path, container_path, _flag)) = &options.system_prompt_file {
        args.push("-v".into());
        args.push(format!(
            "{}:{}:ro",
            host_path.display(),
            container_path.display(),
        ));
    }

    // System prompt env file: Docker-side mount + env var (before the image arg).
    if let Some((env_var, host_path, container_path)) = &options.system_prompt_env_file {
        args.push("-v".into());
        args.push(format!(
            "{}:{}:ro",
            host_path.display(),
            container_path.display(),
        ));
        // The variable name comes from an agent definition: screened like
        // every other env source (WI 0141 §3).
        if !is_screened_run_var(env_var) {
            args.push("-e".into());
            args.push(format!("{}={}", env_var, container_path.display()));
        }
    }

    // The image is the final positional arg.
    args.push(image.0.clone());

    // Entrypoint / agent argv at the end.
    if let Some(ep) = &options.entrypoint {
        for piece in &ep.0 {
            args.push(piece.clone());
        }
    }

    // The agent's own argv tail, shared with Apple (same builder) and builtin.
    // ACP launches are complete at the entrypoint (`cline --acp`): everything an
    // ACP agent needs travels over the JSON-RPC channel, so the tail carries
    // only the user's own `agentFlags`.
    // A tool list the agent has no flag for refuses the launch (fail closed).
    Ok(AgentArgv::from_options(options)?.append_to(args))
}

fn parse_stats_line(line: &str, fallback_name: &str) -> Result<AgentStats, EngineError> {
    // Format: "name|cpu%|memUsage" e.g. "awman-x|2.31%|123MiB / 4GiB"
    let parts: Vec<&str> = line.splitn(3, '|').collect();
    if parts.len() < 3 {
        return Err(EngineError::Container(format!(
            "unparseable docker stats line: {line:?}"
        )));
    }
    let name = if parts[0].is_empty() {
        fallback_name.to_string()
    } else {
        parts[0].to_string()
    };
    let cpu_percent = parse_cpu_percent(parts[1]);
    let memory_mb = parse_memory_mb(parts[2]);
    Ok(AgentStats {
        name,
        cpu_percent,
        memory_mb,
    })
}

fn parse_cpu_percent(s: &str) -> f64 {
    s.trim().trim_end_matches('%').parse::<f64>().unwrap_or(0.0)
}

fn parse_memory_mb(s: &str) -> f64 {
    let raw = s.split('/').next().unwrap_or("").trim();
    let (num_str, unit) = raw
        .find(|c: char| c.is_alphabetic())
        .map(|i| raw.split_at(i))
        .unwrap_or((raw, ""));
    let n: f64 = num_str.trim().parse().unwrap_or(0.0);
    match unit.trim().to_ascii_uppercase().as_str() {
        "B" => n / 1_048_576.0,
        "KB" | "KIB" => n / 1024.0,
        "MB" | "MIB" => n,
        "GB" | "GIB" => n * 1024.0,
        "TB" | "TIB" => n * 1024.0 * 1024.0,
        _ => n,
    }
}

fn docker_socket_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"\\.\pipe\docker_engine")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/var/run/docker.sock")
    }
}

/// Best-effort lookup of the host's `docker` group GID by parsing
/// `/etc/group`. Returns `None` when the group is absent (rootless docker,
/// macOS Docker Desktop where the socket is owned by the user, etc.).
#[cfg(not(target_os = "windows"))]
fn host_docker_group_gid() -> Option<u32> {
    let contents = std::fs::read_to_string("/etc/group").ok()?;
    for line in contents.lines() {
        // Format: name:passwd:gid:user_list
        let mut parts = line.splitn(4, ':');
        let name = parts.next()?;
        if name != "docker" {
            continue;
        }
        let _passwd = parts.next()?;
        let gid_str = parts.next()?;
        if let Ok(gid) = gid_str.parse::<u32>() {
            return Some(gid);
        }
    }
    None
}

/// Clear O_NONBLOCK from stdin/stdout/stderr after an interactive Docker run.
///
/// Docker's `-it` flag sets O_NONBLOCK on the inherited stdio fds and does not
/// reliably restore them on exit. Without this, the next read/write returns
/// EAGAIN ("Resource temporarily unavailable", os error 35 on macOS / 11 on
/// Linux).
///
/// This is the Docker backend's `process::PostWaitHook`: `ContainerCli::DOCKER`
/// names it, and `ContainerExecution::wait_blocking` runs it after every piped
/// child exits. Apple's `container` leaves the fds alone and uses
/// `process::no_post_wait`. A no-op off Unix, so the hook type stays a plain
/// `fn()` on every platform.
pub(super) fn clear_stdio_nonblocking() {
    #[cfg(unix)]
    {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        fn clear_fd(fd: impl std::os::fd::AsFd) {
            if let Ok(flags) = fcntl(&fd, FcntlArg::F_GETFL) {
                let mut o = OFlag::from_bits_truncate(flags);
                if o.contains(OFlag::O_NONBLOCK) {
                    o.remove(OFlag::O_NONBLOCK);
                    let _ = fcntl(&fd, FcntlArg::F_SETFL(o));
                }
            }
        }
        clear_fd(std::io::stdin());
        clear_fd(std::io::stdout());
        clear_fd(std::io::stderr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{
        ContainerOption, EnvVar, ImageRef, OverlayPermission, OverlaySpec, ResolvedContainerOptions,
    };
    use std::path::PathBuf;

    fn resolve(opts: Vec<ContainerOption>) -> ResolvedContainerOptions {
        ResolvedContainerOptions::resolve(opts).unwrap()
    }

    #[test]
    fn build_run_argv_minimal() {
        let resolved = resolve(vec![ContainerOption::Image(ImageRef::new("img:latest"))]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"--rm".to_string()));
        assert!(argv.contains(&"--label".to_string()));
        assert!(argv.contains(&AWMAN_LABEL.to_string()));
        // Image is the final positional arg.
        assert_eq!(argv.last().map(String::as_str), Some("img:latest"));
    }

    /// WI 0101 §2.2: `ContainerOption::SessionLabel` became the general
    /// `Label { key, value }`. `awman=true` must still come first, then exactly
    /// one `--label k=v` per accumulated entry, in ingest order.
    #[test]
    fn build_run_argv_emits_the_awman_label_then_one_flag_per_supplied_label() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: "sid-1".into(),
            },
            ContainerOption::Label {
                key: "awman.squad.task".into(),
                value: "issue-triage".into(),
            },
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let labels: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(i, a)| a.as_str() == "--label" && *i + 1 < argv.len())
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert_eq!(
            labels,
            vec![
                &AWMAN_LABEL.to_string(),
                &"awman.session=sid-1".to_string(),
                &"awman.squad.task=issue-triage".to_string(),
            ],
            "argv was: {argv:?}"
        );
        assert_eq!(
            argv.iter().filter(|a| a.as_str() == "--label").count(),
            3,
            "one --label flag per label, no more"
        );
    }

    /// Regression guard for the pre-refactor behaviour: a lone session label
    /// still renders exactly as it did when it had its own dedicated field.
    #[test]
    fn build_run_argv_renders_a_lone_session_label_exactly_as_before_the_refactor() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: "abc-123".into(),
            },
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let awman = argv
            .iter()
            .position(|a| a == AWMAN_LABEL)
            .expect("the hardcoded awman=true label must be present");
        assert_eq!(argv[awman + 1], "--label");
        assert_eq!(
            argv[awman + 2],
            "awman.session=abc-123",
            "the session label must immediately follow awman=true"
        );
    }

    #[test]
    fn build_run_argv_emits_only_the_awman_label_when_no_labels_are_supplied() {
        let resolved = resolve(vec![ContainerOption::Image(ImageRef::new("img:latest"))]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert_eq!(argv.iter().filter(|a| a.as_str() == "--label").count(), 1);
    }

    #[test]
    fn build_run_argv_includes_overlay_volumes() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Overlay(OverlaySpec {
                host_path: PathBuf::from("/h/p"),
                container_path: PathBuf::from("/c/p"),
                permission: OverlayPermission::ReadOnly,
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-v" && w[1] == "/h/p:/c/p:ro"));
    }

    #[test]
    fn build_run_argv_env_passthrough_only_when_set() {
        use crate::engine::container::options::EnvLiteral;

        std::env::set_var("AWMAN_TEST_ENV_DOCKER", "v1");
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar("AWMAN_TEST_ENV_DOCKER".into())),
            ContainerOption::EnvPassthrough(EnvVar("AWMAN_TEST_NEVER_SET_DOCKER".into())),
            ContainerOption::EnvLiteral(EnvLiteral {
                key: "MY_KEY".into(),
                value: "my_value".into(),
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "AWMAN_TEST_ENV_DOCKER"),
            "passthrough must be emitted as name-only `-e AWMAN_TEST_ENV_DOCKER`; argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "AWMAN_TEST_ENV_DOCKER=v1"),
            "the passthrough value must never ride argv as NAME=VALUE; argv: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|a| a.contains("AWMAN_TEST_NEVER_SET_DOCKER")),
            "an unset passthrough name must emit nothing at all; argv: {argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "MY_KEY=my_value"),
            "a literal must still be emitted as -e KEY=VALUE, unlike passthrough; argv: {argv:?}"
        );
        std::env::remove_var("AWMAN_TEST_ENV_DOCKER");
    }

    /// WI 0116 §1/D1 — the regression this work item exists to end.
    ///
    /// A squad daemon holds a task's `env()` value in the Layer 0 overlay, not
    /// in its own process environment. Gating the `-e` on `std::env::var`
    /// emitted nothing at all there, so the container started silently without
    /// the variable. The gate is `host_var`, so the name must be emitted; the
    /// value must be reachable through `resolve_env_passthrough` (which the
    /// spawn paths set on the child) and must never appear in argv.
    #[test]
    fn build_run_argv_passes_through_a_name_held_only_in_the_daemon_overlay() {
        use crate::data::config::env::{
            set_daemon_overlay, DaemonEnvMap, DAEMON_OVERLAY_TEST_LOCK,
        };

        let _guard = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Unique name, and explicitly *not* in the process environment: this is
        // the daemon's situation exactly.
        let name = "AWMAN_TEST_DOCKER_OVERLAY_ONLY";
        std::env::remove_var(name);
        set_daemon_overlay(DaemonEnvMap::from_pairs([(name, "overlay-secret")]));

        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar(name.into())),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();

        assert!(
            argv.windows(2).any(|w| w[0] == "-e" && w[1] == name),
            "a name held only in the daemon overlay must still emit `-e {name}`; argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("overlay-secret")),
            "the overlay value must never reach argv; argv: {argv:?}"
        );
        assert_eq!(
            resolve_env_passthrough(&resolved),
            vec![(name.to_string(), "overlay-secret".to_string())],
            "the spawn paths take the value from here and set it on the child",
        );

        set_daemon_overlay(DaemonEnvMap::new());
    }

    /// A task created before `env()` validated its argument still carries
    /// whatever it was given, and these names now reach `Command::env` on the
    /// spawn paths (review-security F11).
    #[test]
    fn resolve_env_passthrough_drops_a_name_that_is_not_an_environment_variable_name() {
        use crate::data::config::env::{
            set_daemon_overlay, DaemonEnvMap, DAEMON_OVERLAY_TEST_LOCK,
        };

        let _guard = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The overlay, not the process environment: the OS itself refuses to
        // set a variable whose name contains `=`, which is part of why such a
        // name must never become a `Command::env` key.
        let bad = "AWMAN_TEST_DOCKER=INVALID";
        set_daemon_overlay(DaemonEnvMap::from_pairs([(bad, "v")]));
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar(bad.into())),
        ]);
        assert!(
            resolve_env_passthrough(&resolved).is_empty(),
            "a malformed key must never reach Command::env"
        );
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(!argv.iter().any(|a| a.contains(bad)), "argv: {argv:?}");
        set_daemon_overlay(DaemonEnvMap::new());
    }

    /// D15 — `Some("")` still emits, for bit-for-bit parity with the old
    /// `std::env::var(..).is_ok()` gate. Only the *push* path treats an empty
    /// value as absent.
    #[test]
    fn resolve_env_passthrough_keeps_a_variable_set_to_the_empty_string() {
        let name = "AWMAN_TEST_DOCKER_EMPTY_PASSTHROUGH";
        std::env::set_var(name, "");
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar(name.into())),
        ]);
        assert_eq!(
            resolve_env_passthrough(&resolved),
            vec![(name.to_string(), String::new())]
        );
        std::env::remove_var(name);
    }

    #[test]
    fn build_run_argv_allow_docker_mounts_socket() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::AllowDocker(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(argv
            .iter()
            .any(|a| a.contains("docker.sock") || a.contains("docker_engine")));
    }

    #[test]
    fn build_run_argv_entrypoint_appended_after_image() {
        use crate::engine::container::options::Entrypoint;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Entrypoint(Entrypoint::new(["claude", "--print"])),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let img_pos = argv.iter().position(|a| a == "img:latest").unwrap();
        let claude_pos = argv.iter().position(|a| a == "claude").unwrap();
        let print_pos = argv.iter().position(|a| a == "--print").unwrap();
        assert!(img_pos < claude_pos, "entrypoint must come after image");
        assert!(claude_pos < print_pos, "entrypoint args must be in order");
    }

    #[test]
    fn build_run_argv_rw_overlay_has_no_ro_suffix() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Overlay(OverlaySpec {
                host_path: PathBuf::from("/h/rw"),
                container_path: PathBuf::from("/c/rw"),
                permission: OverlayPermission::ReadWrite,
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let vol_arg = argv
            .windows(2)
            .find(|w| w[0] == "-v")
            .map(|w| w[1].clone())
            .unwrap();
        assert_eq!(
            vol_arg, "/h/rw:/c/rw",
            "RW overlay must not have :ro suffix"
        );
    }

    #[test]
    fn build_run_argv_env_literal_always_included() {
        use crate::engine::container::options::EnvLiteral;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvLiteral(EnvLiteral {
                key: "MY_KEY".into(),
                value: "my_value".into(),
            }),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-e" && w[1] == "MY_KEY=my_value"));
    }

    /// WI 0141 §3: no deny-listed host-terminal variable reaches the argv or
    /// the spawn environment, whether it came from host passthrough or a
    /// literal, and the container always sees `TERM=xterm-256color`.
    #[test]
    fn build_run_argv_drops_host_terminal_vars_and_normalises_term() {
        use crate::engine::container::host_env_denylist::is_host_terminal_var;
        use crate::engine::container::options::EnvLiteral;
        let denied = [
            "TMUX",
            "TMUX_PANE",
            "REX_PANE_ID",
            "ZELLIJ",
            "STY",
            "TERM_PROGRAM",
            "KITTY_WINDOW_ID",
            "WT_SESSION",
        ];
        let mut opts = vec![ContainerOption::Image(ImageRef::new("img:latest"))];
        for n in denied {
            std::env::set_var(n, "host");
            opts.push(ContainerOption::EnvPassthrough(EnvVar(n.into())));
        }
        opts.push(ContainerOption::EnvPassthrough(EnvVar("TERM".into())));
        opts.push(ContainerOption::EnvLiteral(EnvLiteral {
            key: "ITERM_SESSION_ID".into(),
            value: "x".into(),
        }));
        opts.push(ContainerOption::EnvLiteral(EnvLiteral {
            key: "TERM".into(),
            value: "xterm-kitty".into(),
        }));
        let resolved = resolve(opts);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        for n in denied {
            std::env::remove_var(n);
        }
        let names: Vec<&str> = argv
            .windows(2)
            .filter(|w| w[0] == "-e")
            .map(|w| w[1].split('=').next().unwrap())
            .collect();
        assert!(
            names.iter().all(|n| !is_host_terminal_var(n)),
            "argv: {argv:?}"
        );
        assert!(resolve_env_passthrough(&resolved)
            .iter()
            .all(|(n, _)| !is_host_terminal_var(n) && n != "TERM"));
        assert_eq!(
            argv.iter().filter(|a| a.starts_with("TERM=")).count(),
            1,
            "argv: {argv:?}"
        );
        assert!(argv.iter().any(|a| a == "TERM=xterm-256color"));
        assert!(!argv.iter().any(|a| a == "TERM=xterm-kitty"));
    }

    /// An overlay `env()` asking for a denied name yields a notice, no variable.
    #[test]
    fn overlay_requesting_tmux_yields_notice_and_no_variable() {
        std::env::set_var("TMUX", "/tmp/tmux-1000/default,1,0");
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar("TMUX".into())),
        ]);
        let notice = host_terminal_env_notice(&resolved);
        let passed = resolve_env_passthrough(&resolved);
        std::env::remove_var("TMUX");
        assert!(notice.unwrap().contains("TMUX"));
        assert!(passed.is_empty());
    }

    #[test]
    fn build_run_argv_seeded_prompt_adds_i_flag_not_it() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::SeededPrompt("hello world".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.contains(&"-i".to_string()),
            "seeded prompt needs -i flag"
        );
        assert!(
            !argv.contains(&"-it".to_string()),
            "seeded prompt must NOT add -it"
        );
    }

    #[test]
    fn build_run_argv_seeded_prompt_with_interactive_uses_it_and_positional_arg() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::SeededPrompt("hello".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.contains(&"-it".to_string()),
            "interactive+seeded must use -it for PTY"
        );
        assert!(
            !argv.contains(&"-i".to_string()),
            "interactive+seeded must NOT use bare -i"
        );
        assert_eq!(
            argv.last().map(|s| s.as_str()),
            Some("hello"),
            "seeded prompt must be last positional arg"
        );
    }

    #[test]
    fn build_run_argv_interactive_seed_flag_delivers_prompt_via_flag_not_positional() {
        // opencode-shaped delivery: a seed flag is present, so the prompt must
        // be emitted as `<flag> <text>` rather than a bare positional (which
        // opencode would treat as a project dir and open() → ENAMETOOLONG).
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::SeededPrompt("do the task".into()),
            ContainerOption::InteractiveSeedFlag("--prompt".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--prompt" && w[1] == "do the task"),
            "seed flag must deliver the prompt as `--prompt <text>`; got {argv:?}"
        );
        // The prompt must never appear as a lone positional immediately after
        // the image (which is what breaks opencode).
        let img_idx = argv.iter().position(|a| a == "img:latest").unwrap();
        assert_ne!(
            argv.get(img_idx + 1).map(|s| s.as_str()),
            Some("do the task"),
            "prompt must not be a bare positional after the image; got {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_interactive_acp_emits_i_not_it() {
        // ACP framing must never pass through a PTY, so an interactive ACP run
        // allocates `-i` (piped stdin, no `-t`) rather than `-it`.
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::Acp(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.contains(&"-i".to_string()),
            "interactive ACP run needs -i; argv: {argv:?}"
        );
        assert!(
            !argv.contains(&"-it".to_string()),
            "interactive ACP run must NOT allocate a PTY via -it; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_non_interactive_acp_still_emits_i() {
        // A headless (`-n`) ACP run still needs stdin piped open for the
        // bidirectional JSON-RPC exchange.
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Acp(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.contains(&"-i".to_string()),
            "headless ACP run still needs -i; argv: {argv:?}"
        );
        assert!(
            !argv.contains(&"-it".to_string()),
            "ACP run must never use -it; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_acp_does_not_deliver_seeded_prompt_as_positional() {
        // The ACP prompt travels over JSON-RPC (`session/prompt`), never argv.
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::Acp(true),
            ContainerOption::SeededPrompt("do the task".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            !argv.iter().any(|a| a == "do the task"),
            "ACP must not append the seeded prompt as an argv positional; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_acp_emits_nothing_after_the_entrypoint() {
        // Regression for the "corrupted ACP argv" blocker: an ACP launch that
        // also carries a headless subcommand flag and agent mode flags
        // (as every `exec workflow` / `--headless` / `--yolo` ACP launch
        // does) must NOT graft any of them onto `cline --acp` — the ACP argv is
        // complete at the entrypoint. Before the fix this produced
        // `cline --acp task --yolo`, silently corrupting the JSON-RPC launch.
        use crate::engine::container::options::Entrypoint;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Entrypoint(Entrypoint::new(["cline", "--acp"])),
            ContainerOption::Acp(true),
            ContainerOption::AgentModeFlags(vec!["--yolo".into()]),
            ContainerOption::SeededPrompt("do the task".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let img_pos = argv.iter().position(|a| a == "img:latest").unwrap();
        assert_eq!(
            &argv[img_pos + 1..],
            &["cline".to_string(), "--acp".to_string()],
            "ACP argv must end exactly at the entrypoint, with no stdio flags \
             (task/--yolo/prompt) appended; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_agent_flags_follow_awman_flags_and_precede_the_prompt() {
        use crate::engine::container::options::{Entrypoint, ModelFlagForm};
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Entrypoint(Entrypoint::new(["codex"])),
            ContainerOption::Interactive(true),
            ContainerOption::AgentModeFlags(vec![
                "--dangerously-bypass-approvals-and-sandbox".into()
            ]),
            ContainerOption::Model {
                flag: ModelFlagForm::Argument("gpt-5".into()),
            },
            ContainerOption::AgentFlags(vec![
                "--no-daemon".into(),
                "--config".into(),
                "a=b".into(),
            ]),
            ContainerOption::SeededPrompt("do the task".into()),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let img_pos = argv.iter().position(|a| a == "img:latest").unwrap();
        assert_eq!(
            &argv[img_pos + 1..],
            &[
                "codex",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5",
                "--no-daemon",
                "--config",
                "a=b",
                "do the task",
            ]
            .map(String::from),
        );
    }

    #[test]
    fn build_run_argv_acp_keeps_only_agent_flags_after_the_entrypoint() {
        use crate::engine::container::options::Entrypoint;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Entrypoint(Entrypoint::new(["cline", "--acp"])),
            ContainerOption::Acp(true),
            ContainerOption::AgentModeFlags(vec!["--yolo".into()]),
            ContainerOption::AgentFlags(vec!["--verbose".into()]),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        let img_pos = argv.iter().position(|a| a == "img:latest").unwrap();
        assert_eq!(
            &argv[img_pos + 1..],
            &["cline", "--acp", "--verbose"].map(String::from),
        );
    }

    /// Regression guard for the security constraint (aspec/architecture/
    /// security.md): the ACP argv path must introduce NO new host exposure —
    /// no published ports, no host/custom `--network`, no added mounts beyond
    /// what a non-ACP run of the same options would already emit.
    #[test]
    fn build_run_argv_acp_introduces_no_ports_or_network_flags() {
        let acp = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
            ContainerOption::Acp(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &acp,
        )
        .unwrap();
        for banned in ["-p", "--publish", "--network", "--net", "--add-host"] {
            assert!(
                !argv.iter().any(|a| a == banned),
                "ACP argv must never contain {banned}; argv: {argv:?}"
            );
        }

        // And the ACP flag must not add any `-v` mount that a plain
        // interactive run of the same options wouldn't already produce.
        let plain = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
        ]);
        let plain_argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &plain,
        )
        .unwrap();
        let count_v = |v: &[String]| v.iter().filter(|a| a.as_str() == "-v").count();
        assert_eq!(
            count_v(&argv),
            count_v(&plain_argv),
            "ACP must not introduce any new -v mount; acp: {argv:?} plain: {plain_argv:?}"
        );
    }

    #[test]
    fn build_run_argv_interactive_adds_it_flag() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.contains(&"-it".to_string()),
            "interactive run needs -it flag"
        );
    }

    #[test]
    fn build_run_argv_working_dir_adds_w_flag() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::WorkingDir(PathBuf::from("/workspace")),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-w" && w[1] == "/workspace"));
    }

    #[test]
    fn build_run_argv_container_name_present_in_argv() {
        use crate::engine::container::options::ContainerName as CN;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Name(CN::new("my-container")),
        ]);
        let argv = build_run_argv(
            &CN::new("my-container"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--name" && w[1] == "my-container"),
            "container name must appear as --name <name>"
        );
    }

    #[test]
    fn build_run_argv_yolo_does_not_add_extra_docker_flag() {
        // Yolo mode is encoded in the agent's overlay settings (settings.json),
        // NOT as a docker run flag. The argv builder must not add any flag for it.
        use crate::engine::container::options::YoloMode;
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Yolo(YoloMode::Enabled),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            !argv.iter().any(|a| a.contains("yolo")),
            "yolo must not add a docker flag"
        );
        assert!(
            !argv.iter().any(|a| a.contains("bypass")),
            "yolo must not add a bypass flag"
        );
    }

    #[test]
    fn image_home_dir_returns_none_for_unknown_image() {
        // Missing image: `docker image inspect` exits non-zero so the helper
        // must surface `None` rather than panic. Works regardless of whether
        // the docker daemon is reachable, because `Command::output` errors
        // collapse to `None` too.
        let backend = DockerBackend;
        let bogus = "awman-test-image-that-does-not-exist:tag-xyz123";
        assert!(backend.image_home_dir(bogus).unwrap().is_none());
    }

    #[test]
    fn parse_memory_mb_handles_various_units() {
        assert!((parse_memory_mb("200MiB / 1GiB") - 200.0).abs() < 0.1);
        assert!((parse_memory_mb("1.5GiB / 4GiB") - 1536.0).abs() < 0.1);
    }

    #[test]
    fn parse_cpu_percent_strips_percent() {
        assert!((parse_cpu_percent("5.23%") - 5.23).abs() < 0.001);
    }

    // ── WI-0098 Finding A: agent credential values must never enter argv ──────
    //
    // `build_run_argv` emits credentials as the NAME-ONLY form `-e KEY`; the
    // value is set on the spawned CLI child's own environment (see the spawn
    // paths) so nothing secret is visible via `ps` / `/proc/<pid>/cmdline`.
    // These tests assert the value is absent from the built argv while still
    // being carried out-of-band on the resolved options (the source the spawn
    // code feeds to `Command::env`). `build_run_argv` is shared verbatim by the
    // Apple backend, so this coverage applies to both container backends.

    fn credential_opts(pairs: &[(&str, &str)]) -> ResolvedContainerOptions {
        resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::AgentCredentials {
                env_vars: pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
        ])
    }

    /// Review finding: credentials and the system-prompt variable are env
    /// sources too; a denied name or a raw `TERM` from them must not reach
    /// the container or override the normalised `TERM`, and the user is told.
    #[test]
    fn all_env_sources_go_through_the_host_terminal_scrub() {
        let mut options = credential_opts(&[
            ("TMUX", "/tmp/host.sock"),
            ("TERM", "xterm-kitty"),
            ("ANTHROPIC_API_KEY", "sk"),
        ]);
        options.system_prompt_env_file =
            Some(("REX_PANE".into(), "/tmp/prompt".into(), "/prompt".into()));
        let argv = build_run_argv(
            &ContainerName::new("review"),
            &ImageRef::new("img:latest"),
            &options,
        )
        .unwrap();
        let envs: Vec<_> = argv
            .windows(2)
            .filter(|w| w[0] == "-e")
            .map(|w| w[1].as_str())
            .collect();
        assert!(
            !envs.contains(&"TMUX"),
            "credentials bypassed scrub: {envs:?}"
        );
        assert!(!envs.contains(&"TERM"), "late TERM overrides: {envs:?}");
        assert!(envs.contains(&"TERM=xterm-256color"), "{envs:?}");
        assert!(
            envs.contains(&"ANTHROPIC_API_KEY"),
            "ordinary credentials kept"
        );
        assert!(
            !envs.iter().any(|v| v.starts_with("REX_PANE=")),
            "system prompt env bypassed scrub: {envs:?}"
        );
        let names: Vec<_> = screened_agent_credentials(&options)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(names, ["ANTHROPIC_API_KEY"], "spawn env is screened too");
        let notice = host_terminal_env_notice(&options).expect("notice");
        assert!(
            notice.contains("TMUX") && notice.contains("REX_PANE"),
            "{notice}"
        );
    }

    #[test]
    fn build_run_argv_agent_credentials_use_name_only_form() {
        let resolved = credential_opts(&[("ANTHROPIC_API_KEY", "sk-secret-value")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        // The name-only `-e KEY` pair must be present.
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "ANTHROPIC_API_KEY"),
            "credential must be emitted as name-only `-e ANTHROPIC_API_KEY`; argv: {argv:?}"
        );
        // The secret value must appear nowhere in argv, in any form.
        assert!(
            !argv.iter().any(|a| a.contains("sk-secret-value")),
            "credential VALUE must never appear in argv; argv: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|a| a == "ANTHROPIC_API_KEY=sk-secret-value"),
            "the `KEY=VALUE` argv form must not be used for credentials; argv: {argv:?}"
        );
    }

    #[test]
    fn build_run_argv_credential_value_with_equals_stays_out_of_argv() {
        // A value containing `=` must survive the env-inheritance path and must
        // never leak into argv.
        let resolved = credential_opts(&[("TOKEN", "a=b=c")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.windows(2).any(|w| w[0] == "-e" && w[1] == "TOKEN"),
            "credential name must still be present as `-e TOKEN`; argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("a=b=c")),
            "credential value containing `=` must not appear in argv; argv: {argv:?}"
        );
        // Carried out-of-band, verbatim, for the child-process env map.
        assert_eq!(
            resolved.agent_credentials,
            vec![("TOKEN".to_string(), "a=b=c".to_string())],
            "the value must be preserved verbatim on the options for `Command::env`"
        );
    }

    #[test]
    fn build_run_argv_credential_value_with_newline_stays_out_of_argv() {
        let resolved = credential_opts(&[("MULTILINE", "line1\nline2")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        assert!(
            argv.windows(2).any(|w| w[0] == "-e" && w[1] == "MULTILINE"),
            "credential name must be present as `-e MULTILINE`; argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains('\n')),
            "no argv element may contain a newline from the credential value; argv: {argv:?}"
        );
        assert_eq!(
            resolved.agent_credentials,
            vec![("MULTILINE".to_string(), "line1\nline2".to_string())],
            "the newline-bearing value must be preserved verbatim for `Command::env`"
        );
    }

    #[test]
    fn build_run_argv_multiple_credentials_all_name_only() {
        let resolved = credential_opts(&[("KEY_A", "aaa"), ("KEY_B", "bbb")]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        )
        .unwrap();
        for name in ["KEY_A", "KEY_B"] {
            assert!(
                argv.windows(2).any(|w| w[0] == "-e" && w[1] == name),
                "{name} must be emitted name-only; argv: {argv:?}"
            );
        }
        for value in ["aaa", "bbb"] {
            assert!(
                !argv.iter().any(|a| a.contains(value)),
                "credential value {value} must never appear in argv; argv: {argv:?}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod docker_listing_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    thread_local! {
        pub(super) static CLI: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    struct FakeDocker {
        dir: tempfile::TempDir,
    }

    impl FakeDocker {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cli = dir.path().join("docker");
            fs::write(
                &cli,
                r##"#!/bin/sh
printf '%s\n' "$@" >> "$(dirname "$0")/argv"
mode=$(cat "$(dirname "$0")/mode")
case "$mode" in
  unavailable) echo "cannot connect to the docker daemon" >&2; exit 1 ;;
  nonzero) echo "query rejected" >&2; exit 17 ;;
  second-fails)
    case " $* " in *" name=awman- "*) echo "legacy query failed" >&2; exit 9 ;; esac
    printf 'first-id\tawman-current\timage:v1\tbad-time\n'
    exit 0
    ;;
  empty) exit 0 ;;
  rows)
    case " $* " in
      *" label=awman=true "*)
        printf 'row-one\tawman-current\timage:v1\tbad-time\n'
        printf 'row-one\tawman-current\timage:v1\tbad-time\n'
        printf 'short-row\tmissing-fields\n'
        printf '\tawman-empty-id\timage:v1\tbad-time\n'
        printf 'row-empty-name\t\timage:v1\tbad-time\n'
        ;;
      *)
        printf 'row-one\tawman-current\timage:v1\tbad-time\n'
        printf 'legacy-id\tawman-legacy\timage:v0\tbad-time\n'
        ;;
    esac
    ;;
esac
"##,
            )
            .unwrap();
            fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(dir.path().join("mode"), "empty").unwrap();
            CLI.with(|slot| assert!(slot.replace(Some(cli)).is_none()));
            Self { dir }
        }

        fn mode(&self, mode: &str) {
            fs::write(self.dir.path().join("mode"), mode).unwrap();
        }
    }

    impl Drop for FakeDocker {
        fn drop(&mut self) {
            CLI.with(|slot| slot.replace(None));
        }
    }

    #[test]
    fn listing_override_is_local_to_its_thread() {
        let fake = FakeDocker::new();
        let command = listing_command(ContainerCli::DOCKER);
        assert_eq!(command.get_program(), "/bin/sh");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [fake.dir.path().join("docker").as_os_str()]
        );
        let other = std::thread::spawn(|| {
            listing_command(ContainerCli::DOCKER)
                .get_program()
                .to_os_string()
        })
        .join()
        .unwrap();
        assert_ne!(other, "/bin/sh");
    }

    #[test]
    fn ownership_listing_handles_empty_errors_partial_failure_malformed_rows_and_dedup() {
        let fake = FakeDocker::new();
        let backend = DockerBackend;

        fake.mode("empty");
        assert!(backend.list_running_all().unwrap().is_empty());

        fake.mode("unavailable");
        assert!(matches!(
            backend.list_running_all(),
            Err(EngineError::Container(message)) if message.contains("cannot connect")
        ));

        fake.mode("nonzero");
        assert!(matches!(
            backend.list_running_all(),
            Err(EngineError::Container(message)) if message.contains("query rejected")
        ));

        fake.mode("second-fails");
        assert!(matches!(
            backend.list_running_all(),
            Err(EngineError::Container(message)) if message.contains("legacy query failed")
        ));

        fake.mode("rows");
        let listed = backend.list_running_all().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["row-one", "legacy-id"]
        );
        assert_eq!(listed[1].name, "awman-legacy");
    }

    #[test]
    fn all_listing_routes_preserve_query_filters_and_failures() {
        let fake = FakeDocker::new();
        let backend = DockerBackend;
        for route in 0..3 {
            let list = || match route {
                0 => backend.list_running_all(),
                1 => backend.list_stopped(),
                _ => backend.list_running_with_name_prefix("awman-squad-task-"),
            };
            for mode in ["empty", "unavailable", "nonzero", "rows", "second-fails"] {
                fake.mode(mode);
                fs::write(fake.dir.path().join("argv"), "").unwrap();
                let result = list();
                match mode {
                    "empty" => assert!(result.unwrap().is_empty()),
                    "unavailable" | "nonzero" => assert!(result.is_err()),
                    "second-fails" if route < 2 => assert!(result.is_err()),
                    _ => assert!(!result.unwrap().is_empty()),
                }
                let argv = fs::read_to_string(fake.dir.path().join("argv")).unwrap();
                let args: Vec<&str> = argv.lines().collect();
                assert_eq!(args[0], "ps");
                assert!(args.contains(&"--no-trunc"));
                assert_eq!(args.contains(&"-a"), route == 1);
                assert_eq!(args.contains(&"status=exited"), route == 1);
                assert_eq!(args.contains(&"status=dead"), route == 1);
                if route == 2 {
                    assert!(args.contains(&"name=awman-squad-task-"));
                } else {
                    assert!(args.contains(&"label=awman=true"));
                    if !matches!(mode, "unavailable" | "nonzero") {
                        assert!(args.contains(&"name=awman-"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod explicit_phase_env_review_tests {
    use super::*;
    use crate::engine::auth::credential::SecretString;
    use crate::engine::container::options::{ContainerOption, EnvVar};
    #[test]
    fn explicit_values_win_over_different_or_absent_ambient_values_without_entering_argv() {
        use crate::data::config::env::{
            set_daemon_overlay, DaemonEnvMap, DAEMON_OVERLAY_TEST_LOCK,
        };
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = "AWMAN_WI0149_EXPLICIT_OVERRIDE";
        for ambient in [None, Some("ambient_token")] {
            set_daemon_overlay(DaemonEnvMap::from_pairs(ambient.map(|v| (key, v))));
            let options = ResolvedContainerOptions::resolve(vec![
                ContainerOption::Image(ImageRef::new("fixture")),
                ContainerOption::EnvPassthrough(EnvVar(key.into())),
                ContainerOption::EnvOverride {
                    key: key.into(),
                    value: SecretString::new("selected_token"),
                },
            ])
            .unwrap();
            assert_eq!(
                resolve_env_passthrough(&options),
                [(key.into(), "selected_token".into())]
            );
            let argv = build_run_argv(
                &ContainerName::new("fixture"),
                &ImageRef::new("fixture"),
                &options,
            )
            .unwrap();
            assert!(argv.windows(2).any(|w| w == ["-e", key]));
            assert!(!format!("{argv:?} {options:?}").contains("selected_token"));
        }
        set_daemon_overlay(DaemonEnvMap::new());
    }
}

#[cfg(test)]
mod timestamp_review_tests {
    use super::*;

    #[test]
    fn malformed_creation_time_keeps_the_legacy_observation_time_fallback() {
        let before = chrono::Utc::now();
        let handles = parse_agent_listing(
            b"id\tawman-agent\timage:v1\tbad-time\n",
            &mut Default::default(),
        );
        let after = chrono::Utc::now();
        assert_eq!(handles.len(), 1);
        assert!((before..=after).contains(&handles[0].started_at));
    }
}
