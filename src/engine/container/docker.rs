//! Docker backend — `pub(super)`. Concrete type is invisible outside
//! `src/engine/container/`.
//!
//! Builds a `docker run` argv from `ResolvedContainerOptions`, spawns the
//! subprocess, and captures the exit code.
//!
//! All container I/O is mediated through the `ContainerIo` channels
//! provided by the frontend. When the frontend provides PTY fields
//! (`initial_size`/`resize` are `Some`), the engine opens a PTY via
//! `portable-pty`. Otherwise it uses `Stdio::piped()`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::instance::{
    handle_now, ContainerExecution, ContainerExitInfo, ContainerId, ContainerInstance,
    ContainerStats, ExecutionBackend,
};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::error::EngineError;

/// Docker label applied to every amux-spawned container so `list_running`
/// can filter to ours.
const AWMAN_LABEL: &str = "awman=true";

#[derive(Debug, Default)]
pub(super) struct DockerBackend;

impl DockerBackend {
    pub(super) fn new() -> Self {
        Self
    }

    /// Probe whether the docker daemon is reachable. Returns `false` quietly
    /// when the binary is missing, the daemon is down, or the probe times out.
    pub(super) fn is_available() -> bool {
        let child = Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => {
                super::runtime::wait_with_timeout(child, std::time::Duration::from_secs(10))
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }
}

impl ContainerBackend for DockerBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn ContainerInstance>, EngineError> {
        let image = options
            .image
            .clone()
            .ok_or_else(|| EngineError::MissingRequiredOption("Image".into()))?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        Ok(Box::new(DockerContainerInstance {
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
        }))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<ContainerHandle>, EngineError> {
        // Query by label AND by name prefix so old-amux containers (which may
        // lack the label) are included. Results from all queries are merged and
        // deduplicated by container ID.
        let format = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";
        let queries: &[&[&str]] = &[
            &["ps", "--filter", "label=awman=true", "--format", format],
            &["ps", "--filter", "name=awman-", "--format", format],
        ];

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut handles: Vec<ContainerHandle> = Vec::new();

        for args in queries {
            let output = Command::new("docker")
                .args(*args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let output = match output {
                Ok(o) if o.status.success() => o,
                // Docker missing or query failed: skip this filter, try next.
                _ => continue,
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() < 4 {
                    continue;
                }
                let id = parts[0].to_string();
                if !seen.insert(id.clone()) {
                    continue; // already added from a previous query
                }
                let name = parts[1].to_string();
                let image_tag = parts[2].to_string();
                let created = parts[3];
                // Docker's "CreatedAt" format is locale-formatted; fall back to
                // now() when parsing fails — better to surface the row than drop it.
                let started_at =
                    chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                handles.push(ContainerHandle {
                    id,
                    image_tag,
                    name,
                    started_at,
                });
            }
        }

        Ok(handles)
    }

    fn list_running_all(&self) -> Result<Vec<ContainerHandle>, EngineError> {
        let format = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.CreatedAt}}";
        let queries: &[&[&str]] = &[
            &["ps", "--filter", "label=awman=true", "--format", format],
            &["ps", "--filter", "name=awman-", "--format", format],
        ];

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut handles: Vec<ContainerHandle> = Vec::new();

        for args in queries {
            let output = Command::new("docker")
                .args(*args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let output = match output {
                Ok(o) if o.status.success() => o,
                _ => continue,
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() < 4 {
                    continue;
                }
                let id = parts[0].to_string();
                if !seen.insert(id.clone()) {
                    continue;
                }
                let name = parts[1].to_string();
                if id.is_empty() && name.is_empty() {
                    continue;
                }
                let image_tag = parts[2].to_string();
                let created = parts[3];
                let started_at =
                    chrono::DateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S %z %Z")
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                handles.push(ContainerHandle {
                    id,
                    image_tag,
                    name,
                    started_at,
                });
            }
        }

        Ok(handles)
    }

    fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError> {
        let output = Command::new("docker")
            .args([
                "stats",
                "--no-stream",
                "--format",
                "{{.Name}}|{{.CPUPerc}}|{{.MemUsage}}",
                &handle.name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    EngineError::ContainerRuntimeUnavailable {
                        binary: "docker".into(),
                    }
                } else {
                    EngineError::Container(format!("docker stats: {e}"))
                }
            })?;
        if !output.status.success() {
            return Err(EngineError::Container(format!(
                "docker stats failed for container {}",
                handle.name
            )));
        }
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        parse_stats_line(&line, &handle.name)
    }

    fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError> {
        // Best-effort: stop, then rm. A nonzero exit (already gone) is fine.
        let _ = Command::new("docker")
            .args(["stop", &handle.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("docker")
            .args(["rm", &handle.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }

    fn exec_args(
        &self,
        container_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        let mut args = vec!["exec".to_string(), "-it".to_string()];
        args.extend(["-w".to_string(), working_dir.to_string()]);
        for (k, v) in env_vars {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push(container_id.to_string());
        args.extend(entrypoint.iter().map(|s| s.to_string()));
        args
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}

struct DockerContainerInstance {
    id: ContainerId,
    name: ContainerName,
    image: ImageRef,
    options: ResolvedContainerOptions,
}

impl ContainerInstance for DockerContainerInstance {
    fn id(&self) -> &ContainerId {
        &self.id
    }
    fn name(&self) -> &ContainerName {
        &self.name
    }
    fn image(&self) -> &ImageRef {
        &self.image
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn crate::engine::container::frontend::ContainerFrontend>,
    ) -> Result<ContainerExecution, EngineError> {
        let argv = build_run_argv(&self.name, &self.image, &self.options);
        let started_at = chrono::Utc::now();
        let seeded = self.options.seeded_prompt.clone();
        let handle = handle_now(&self.id, &self.name, &self.image);

        frontend.report_status(
            crate::engine::container::frontend::ContainerStatus::Running {
                container_name: self.name.0.clone(),
            },
        );

        // Read per-frontend timeouts before draining `take_container_io`,
        // which leaves the frontend in a state where any further calls are
        // implementation-defined.
        let grace_timeout = frontend.grace_timeout();
        let stuck_timeout = frontend.stuck_timeout();
        let io = frontend.take_container_io();

        let bridge_cfg = bridge_config_for(&self.name, grace_timeout, stuck_timeout);

        // PTY path: frontend requested interactive PTY bridging.
        if io.initial_size.is_some() {
            return spawn_pty_bridged_docker(
                self, io, argv, seeded, started_at, handle, bridge_cfg,
            );
        }

        // Piped path: non-interactive or no PTY.
        spawn_piped_docker(self, io, argv, seeded, started_at, handle, bridge_cfg)
    }
}

/// Build a `BridgeConfig` for this container, including a cancel callback
/// that runs `docker stop <name>` so the startup-grace detector can kill a
/// container that never produced output. We construct the same `docker stop`
/// invocation the backend's `cancel_handle` would issue.
fn bridge_config_for(
    name: &ContainerName,
    grace_timeout: std::time::Duration,
    stuck_timeout: std::time::Duration,
) -> crate::engine::container::io_bridge::BridgeConfig {
    let container_name = name.0.clone();
    let cancel: crate::engine::container::io_bridge::CancelFn = std::sync::Arc::new(move || {
        let _ = Command::new("docker")
            .args(["stop", &container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    crate::engine::container::io_bridge::BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: std::time::Duration::ZERO,
        cancel_on_grace_expired: Some(cancel),
    }
}

/// Spawn `docker run -it` via `portable-pty` and bridge the PTY master to
/// the frontend's `ContainerIo` channels via the shared I/O bridge.
fn spawn_pty_bridged_docker(
    instance: Box<DockerContainerInstance>,
    io: crate::engine::container::frontend::ContainerIo,
    argv: Vec<String>,
    _seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::ContainerHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<ContainerExecution, EngineError> {
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
        .map_err(|e| EngineError::Container(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new("docker");
    for arg in &argv {
        cmd.arg(arg);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn docker via pty: {e}")))?;

    // Interactive PTY runs pass the seeded prompt as a CLI positional arg
    // (appended by `build_run_argv`), so it must NOT also be written to stdin.
    // Writing it here would cause the PTY to echo the prompt text into the
    // terminal output, painting it over the TUI before the agent starts.

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let backend = DockerExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
    };
    Ok(ContainerExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
    ))
}

/// Spawn `docker run` with piped stdio and bridge through `ContainerIo`.
fn spawn_piped_docker(
    instance: Box<DockerContainerInstance>,
    io: crate::engine::container::frontend::ContainerIo,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::ContainerHandle,
    bridge_cfg: crate::engine::container::io_bridge::BridgeConfig,
) -> Result<ContainerExecution, EngineError> {
    let mut cmd = Command::new("docker");
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
            EngineError::Container(format!("spawn docker: {e}"))
        }
    })?;

    // Write seeded prompt into stdin channel before the writer task starts.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\n".to_vec());
    }

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);

    // Non-interactive (piped) path: drop the engine's stdin_injector so the
    // writer task sees EOF after draining the seeded prompt and closes the
    // child's stdin pipe. Without this, an agent that probes stdin for EOF
    // would hang waiting for input that will never come.
    // `try_inject_stdin` falls back to launching a fresh container — which
    // is the correct behaviour for a non-interactive run that has already
    // consumed its single prompt.
    drop(bridge.stdin_injector);

    let backend = DockerExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        container_name: instance.name.0.clone(),
        started_at,
    };
    Ok(ContainerExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
    ))
}

struct DockerExecution {
    /// Set when running with piped stdio.
    child: Option<std::process::Child>,
    /// Set when running PTY-bridged. `portable_pty::Child` has its own wait
    /// API and cannot be unified with `std::process::Child`.
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Master PTY end. Held alive so the resize task can call into it and so
    /// the PTY isn't torn down before the child has finished writing.
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    /// Stdin sender — same channel the writer task drains. Used by
    /// `try_inject_stdin` so workflow `ContinueInCurrentContainer` can push a
    /// fresh prompt into the running container.
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    container_name: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for DockerExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<ContainerExitInfo, EngineError> {
        // PTY-bridged path: wait on the portable-pty child.
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait docker (pty): {e}")))?;
            // Drop the master AFTER the child exits so the reader thread sees
            // EOF cleanly.
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(ContainerExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }

        // Piped path: wait on std::process::Child.
        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Container("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait docker: {e}")))?;

        // After interactive runs, docker may leave stdio in O_NONBLOCK mode
        // on Unix. Restore it.
        #[cfg(unix)]
        clear_stdio_nonblocking();

        let exit_code = status.code().unwrap_or(-1);
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;

        Ok(ContainerExitInfo {
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
        // Best-effort: docker stop will SIGTERM then SIGKILL after a grace
        // period. Then docker rm to clean up.
        let _ = Command::new("docker")
            .args(["stop", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("docker")
            .args(["rm", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }

    fn cancel_handle(&self) -> Option<super::instance::CancelHandle> {
        let name = self.container_name.clone();
        Some(super::instance::CancelHandle::new(move || {
            let _ = Command::new("docker")
                .args(["stop", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = Command::new("docker")
                .args(["rm", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            Ok(())
        }))
    }
}

/// Translate `ResolvedContainerOptions` into a `docker run` argv (without the
/// leading `docker` binary).
pub(super) fn build_run_argv(
    name: &ContainerName,
    image: &ImageRef,
    options: &ResolvedContainerOptions,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into()];
    if options.remove_on_exit {
        args.push("--rm".into());
    }
    if options.interactive {
        // Interactive runs always allocate a PTY. When a seeded prompt is also
        // present, the prompt is appended as a positional argv arg below so the
        // agent receives it without piping; stdin stays inherited for the user.
        args.push("-it".into());
    } else if options.seeded_prompt.is_some() {
        // Non-interactive with a seeded prompt: pipe stdin so we can write the
        // prompt, then close it. No PTY — allocating one fails when there is no
        // host TTY (ENOTTY / "Inappropriate ioctl for device").
        args.push("-i".into());
    }

    args.push("--name".into());
    args.push(name.0.clone());

    // Standard awman label so `list_running` can filter.
    args.push("--label".into());
    args.push(AWMAN_LABEL.into());

    // Session-scoped label — emitted when the option-builder threaded the
    // session id through. Lets `list_running` attribute containers to a
    // specific awman session.
    if let Some(session_id) = &options.session_label {
        args.push("--label".into());
        args.push(format!("awman.session={session_id}"));
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

    // Env passthrough — only emit when the variable is set on the host.
    for envvar in &options.env_passthrough {
        if let Ok(value) = std::env::var(&envvar.0) {
            args.push("-e".into());
            args.push(format!("{}={}", envvar.0, value));
        }
    }
    // Env literals.
    for lit in &options.env_literal {
        args.push("-e".into());
        args.push(format!("{}={}", lit.key, lit.value));
    }
    // Agent credentials are env-vars by another name.
    for (k, v) in &options.agent_credentials {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
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

    // The image is the final positional arg.
    args.push(image.0.clone());

    // Entrypoint / agent argv at the end.
    if let Some(ep) = &options.entrypoint {
        for piece in &ep.0 {
            args.push(piece.clone());
        }
    }

    // Mode flags appended to the agent argv.
    if let Some(flag) = &options.non_interactive_flag {
        // Some agents take a sub-command (e.g. "run") rather than a flag.
        args.push(flag.clone());
    }
    // Per-agent mode flags (yolo, auto, plan) — appended as literal args.
    for flag in &options.agent_mode_flags {
        args.push(flag.clone());
    }

    // Disallowed tools.
    if !options.disallowed_tools.is_empty() {
        if let Some(flag_name) = options.disallowed_tools_flag.as_deref() {
            args.push(flag_name.to_string());
            args.push(options.disallowed_tools.join(","));
        }
    }
    // Allowed tools.
    if !options.allowed_tools.is_empty() {
        if let Some(flag_name) = options.allowed_tools_flag.as_deref() {
            args.push(flag_name.to_string());
            args.push(options.allowed_tools.join(","));
        }
    }

    // Model flag.
    if let Some(model) = &options.model {
        match model {
            crate::engine::container::options::ModelFlagForm::Argument(name) => {
                args.push("--model".into());
                args.push(name.clone());
            }
            crate::engine::container::options::ModelFlagForm::Shorthand(s) => {
                args.push(s.clone());
            }
        }
    }

    // Interactive + seeded prompt: pass the prompt as the final positional arg
    // so the agent receives it as its initial task. Stdin stays inherited.
    // Non-interactive + seeded prompt is handled via stdin piping at spawn time.
    if options.interactive {
        if let Some(prompt) = &options.seeded_prompt {
            args.push(prompt.clone());
        }
    }

    args
}

fn parse_stats_line(line: &str, fallback_name: &str) -> Result<ContainerStats, EngineError> {
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
    Ok(ContainerStats {
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
#[cfg(unix)]
fn clear_stdio_nonblocking() {
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
        );
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"--rm".to_string()));
        assert!(argv.contains(&"--label".to_string()));
        assert!(argv.contains(&AWMAN_LABEL.to_string()));
        // Image is the final positional arg.
        assert_eq!(argv.last().map(String::as_str), Some("img:latest"));
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
        );
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-v" && w[1] == "/h/p:/c/p:ro"));
    }

    #[test]
    fn build_run_argv_env_passthrough_only_when_set() {
        std::env::set_var("AWMAN_TEST_ENV_DOCKER", "v1");
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::EnvPassthrough(EnvVar("AWMAN_TEST_ENV_DOCKER".into())),
            ContainerOption::EnvPassthrough(EnvVar("AWMAN_TEST_NEVER_SET_DOCKER".into())),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
        assert!(argv.contains(&"AWMAN_TEST_ENV_DOCKER=v1".to_string()));
        assert!(!argv
            .iter()
            .any(|a| a.contains("AWMAN_TEST_NEVER_SET_DOCKER")));
        std::env::remove_var("AWMAN_TEST_ENV_DOCKER");
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
        );
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
        );
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
        );
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
        );
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-e" && w[1] == "MY_KEY=my_value"));
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
        );
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
        );
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
    fn build_run_argv_interactive_adds_it_flag() {
        let resolved = resolve(vec![
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Interactive(true),
        ]);
        let argv = build_run_argv(
            &ContainerName::new("ctr"),
            &ImageRef::new("img:latest"),
            &resolved,
        );
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
        );
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
        );
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
        );
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
    fn parse_memory_mb_handles_various_units() {
        assert!((parse_memory_mb("200MiB / 1GiB") - 200.0).abs() < 0.1);
        assert!((parse_memory_mb("1.5GiB / 4GiB") - 1536.0).abs() < 0.1);
    }

    #[test]
    fn parse_cpu_percent_strips_percent() {
        assert!((parse_cpu_percent("5.23%") - 5.23).abs() < 0.001);
    }
}
