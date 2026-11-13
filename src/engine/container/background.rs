//! `BackgroundContainer` — a long-running container that accepts `exec` calls.
//!
//! Used by setup and teardown phases to run sequential shell commands inside a
//! single container instance. The container is started with an idle entrypoint
//! (`sleep infinity`) and kept alive for the duration of the phase.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use crate::data::config::builtin_launch::BuiltinLaunchSettings;
use crate::engine::agent_runtime::background::{AgentExec, ExecOutput};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::options::OverlaySpec;
use crate::engine::container::process::ContainerCli;
use crate::engine::error::EngineError;

/// One command executed inside an existing background container.
#[derive(Debug, Clone, Copy)]
pub struct BackgroundExecRequest<'a> {
    pub id: &'a str,
    pub command: &'a str,
    pub working_dir: &'a str,
    pub env: Option<&'a HashMap<String, String>>,
    pub private_stdin: Option<&'a crate::data::secret::SecretString>,
}

/// An idle setup/teardown container with per-session builtin settings.
#[derive(Clone, Copy)]
pub struct BackgroundLaunchRequest<'a> {
    pub secret_env: &'a [(String, crate::engine::auth::credential::SecretString)],
    /// Explicit ownership name, when the caller needs restart discovery.
    pub name: Option<&'a str>,
    /// The project's base image.
    pub image: &'a str,
    /// The workspace, mounted read-write at its own path.
    pub workdir: &'a Path,
    /// Host-resolved `env()` overlay values, passed through by name.
    pub env: &'a HashMap<String, String>,
    /// The phase's resolved overlays.
    pub overlays: &'a [OverlaySpec],
    /// The launching session's builtin launch settings (from
    /// `AgentEngine::builtin_launch_settings`). Only the builtin backend reads
    /// them, and it refuses to start without them; every other backend
    /// ignores them.
    pub builtin_launch: Option<&'a BuiltinLaunchSettings>,
}

impl std::fmt::Debug for BackgroundLaunchRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundLaunchRequest")
            .field("image", &self.image)
            .field("workdir", &self.workdir)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("secret_env", &self.secret_env)
            .finish_non_exhaustive()
    }
}

/// A running background container that accepts `exec` calls.
///
/// Created by `ContainerRuntime::launch_background`. Must be explicitly killed
/// via `kill()` when the phase is complete. Implements `Drop` as a safety net
/// for best-effort cleanup.
pub struct BackgroundContainer {
    container_id: String,
    backend: Arc<dyn ContainerBackend>,
    working_dir: String,
    killed: bool,
}

impl BackgroundContainer {
    pub(super) fn new(
        container_id: String,
        backend: Arc<dyn ContainerBackend>,
        working_dir: String,
    ) -> Self {
        Self {
            container_id,
            backend,
            working_dir,
            killed: false,
        }
    }

    /// Execute a shell command inside the running container. Routes through
    /// the backend so a future runtime with diverging exec syntax can override.
    pub fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.backend
            .exec_in_background(&self.container_id, command, &self.working_dir, env)
    }

    /// Stop and remove the background container. Must always be called, even if
    /// exec steps failed.
    pub fn kill(mut self) -> Result<(), EngineError> {
        self.do_kill()
    }

    fn do_kill(&mut self) -> Result<(), EngineError> {
        if self.killed {
            return Ok(());
        }
        self.killed = true;
        self.backend.stop_and_remove(&self.container_id)
    }

    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// Seed a container-local credential cache without argv, env or disk delivery.
    pub fn seed_git_credentials(
        &self,
        seed: &crate::data::secret::SecretString,
    ) -> Result<(), EngineError> {
        let result = self.backend.exec_in_background_streaming(
            &BackgroundExecRequest {
                id: &self.container_id,
                command: crate::engine::git::credential_socket::SEED_CONTAINER_CACHE,
                working_dir: &self.working_dir,
                env: None,
                private_stdin: Some(seed),
            },
            &mut |_| {},
        )?;
        if result.exit_code != 0 {
            return Err(EngineError::Container(
                "could not initialize the session Git credential helper".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for BackgroundContainer {
    fn drop(&mut self) {
        if !self.killed {
            if let Err(e) = self.do_kill() {
                tracing::warn!(
                    container_id = %self.container_id,
                    error = %e,
                    "failed to kill background container"
                );
            }
        }
    }
}

impl AgentExec for BackgroundContainer {
    fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        // Delegates to the inherent method; inherent takes priority in method
        // resolution so this does not recurse.
        BackgroundContainer::exec(self, command, env)
    }

    fn exec_streaming(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        self.backend.exec_in_background_streaming(
            &crate::engine::container::background::BackgroundExecRequest {
                id: &self.container_id,
                command,
                working_dir: &self.working_dir,
                env,
                private_stdin: None,
            },
            on_line,
        )
    }
}

// ─── Default backend implementations ─────────────────────────────────────────
//
// The Docker and Apple Containers CLIs share identical argv for these
// operations. Both backends' explicit `ContainerBackend` implementations
// delegate here with their own `ContainerCli`.

pub(super) fn default_start_background(
    cli: ContainerCli,
    name: Option<&str>,
    image: &str,
    workdir: &Path,
    env: &HashMap<String, String>,
    overlays: &[OverlaySpec],
    secret_env: &[(String, crate::engine::auth::credential::SecretString)],
) -> Result<String, EngineError> {
    let name = name
        .map(str::to_owned)
        .unwrap_or_else(crate::engine::container::naming::generate_container_name);
    let mut args = build_start_background_argv(&name, image, workdir, env, overlays);
    // Insert names before the image; values exist only in the CLI child env.
    for (key, _) in secret_env {
        args.splice(args.len() - 2..args.len() - 2, ["-e".into(), key.clone()]);
    }

    let mut command = cli.command();
    command.args(&args);
    for (k, v) in env {
        command.env(k, v);
    }
    for (key, value) in secret_env {
        command.env(key, value.expose());
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::ContainerRuntimeUnavailable {
                    binary: cli.bin.into(),
                }
            } else {
                EngineError::Container(format!("spawn {} run -d: {e}", cli.bin))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such image")
            || stderr.contains("not found")
            || stderr.contains("pull access denied")
            || stderr.contains("manifest unknown")
        {
            return Err(EngineError::ContainerImageNotFound {
                image: image.to_string(),
            });
        }
        return Err(EngineError::Container(format!(
            "failed to start background container: {}",
            stderr.trim()
        )));
    }

    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if container_id.is_empty() {
        Ok(name)
    } else {
        Ok(container_id)
    }
}

/// The `run -d` argv for a background container, without the leading CLI
/// binary. Split out from [`default_start_background`] so the "no value in
/// argv" rule is unit-testable without a container runtime.
pub(super) fn build_start_background_argv(
    name: &str,
    image: &str,
    workdir: &Path,
    env: &HashMap<String, String>,
    overlays: &[OverlaySpec],
) -> Vec<String> {
    let workdir_str = workdir.display().to_string();

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.to_string(),
        "--label".into(),
        "awman=true".into(),
        "-w".into(),
        workdir_str.clone(),
    ];

    // Mount the working directory as read-write.
    args.push("-v".into());
    args.push(format!("{ws}:{ws}", ws = workdir_str));

    // Apply overlay mounts.
    for overlay in overlays {
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

    // Env vars set at start time — inherited by all subsequent exec calls.
    //
    // Name-only (`-e NAME`), never `-e NAME=VALUE`: this map is resolved
    // through `host_var`, so inside the squad daemon it carries the values a
    // client pushed for a task's `env()` overlays. argv is world-readable via
    // `/proc/<pid>/cmdline` and `ps`, so a value that may be a secret must
    // never be written into it. The value is set on the spawned CLI child's own
    // environment instead, and both the docker CLI and the Apple `container`
    // CLI resolve a name-only `-e` from their own process env — the same rule
    // `build_run_argv` applies to `env_passthrough` and agent credentials.
    let mut env_names: Vec<&String> = env.keys().collect();
    env_names.sort();
    for k in env_names {
        args.push("-e".into());
        args.push(k.clone());
    }

    // Override image entrypoints (for example alpine/git's `git`): the
    // background container must stay alive to accept subsequent exec calls.
    args.extend(["--entrypoint".into(), "sleep".into()]);
    args.push(image.to_string());
    args.push("infinity".into());

    args
}

pub(super) fn default_exec_in_background(
    cli: ContainerCli,
    container_id: &str,
    command: &str,
    working_dir: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<ExecOutput, EngineError> {
    let mut args = vec!["exec".to_string()];
    args.extend(["-w".to_string(), working_dir.to_string()]);

    // Unlike `build_start_background_argv`, this keeps the `KEY=VALUE` form.
    // The only values that reach here are step-declared workflow env
    // (`setup_step_to_shell`), which are literals from the repo's own workflow
    // file, never a host-supplied `env()` value. If a caller ever needs to pass
    // a host value through here, it must switch to the name-only form and set
    // the value on the spawned CLI child's environment the way
    // `default_start_background` does — argv is world-readable.
    if let Some(env_map) = env {
        for (k, v) in env_map {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
    }

    args.push(container_id.to_string());
    args.extend(["sh".to_string(), "-c".to_string(), command.to_string()]);

    let output = cli
        .command()
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            EngineError::Container(format!("exec in background container {container_id}: {e}"))
        })?;

    let exit_code = output.status.code().unwrap_or(-1);
    Ok(ExecOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code,
    })
}

pub(super) fn default_exec_in_background_streaming(
    cli: ContainerCli,
    request: &BackgroundExecRequest<'_>,
    on_line: &mut dyn FnMut(&str),
) -> Result<ExecOutput, EngineError> {
    let BackgroundExecRequest {
        id: container_id,
        command,
        working_dir,
        env,
        private_stdin,
    } = *request;
    let mut args = vec!["exec".to_string()];
    if private_stdin.is_some() {
        args.push("-i".into());
    }
    args.extend(["-w".to_string(), working_dir.to_string()]);

    // Unlike `build_start_background_argv`, this keeps the `KEY=VALUE` form.
    // The only values that reach here are step-declared workflow env
    // (`setup_step_to_shell`), which are literals from the repo's own workflow
    // file, never a host-supplied `env()` value. If a caller ever needs to pass
    // a host value through here, it must switch to the name-only form and set
    // the value on the spawned CLI child's environment the way
    // `default_start_background` does — argv is world-readable.
    if let Some(env_map) = env {
        for (k, v) in env_map {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
    }

    args.push(container_id.to_string());
    args.extend(["sh".to_string(), "-c".to_string(), command.to_string()]);

    let mut child = cli
        .command()
        .args(&args)
        .stdin(if private_stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            EngineError::Container(format!("exec in background container {container_id}: {e}"))
        })?;

    // Write concurrently with draining output so a runtime cannot deadlock on
    // full pipes. No PTY: credentials cannot be echoed by terminal line discipline.
    let writer = private_stdin.map(|secret| {
        let bytes = secret.clone();
        let mut input = child.stdin.take().expect("piped private stdin");
        std::thread::spawn(move || std::io::Write::write_all(&mut input, bytes.expose().as_bytes()))
    });

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stderr_thread = std::thread::spawn(move || {
        let mut stderr_buf = String::new();
        if let Some(pipe) = stderr_pipe {
            let reader = std::io::BufReader::new(pipe);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        stderr_buf.push_str(&l);
                        stderr_buf.push('\n');
                    }
                    Err(_) => break,
                }
            }
        }
        stderr_buf
    });

    let mut stdout_buf = String::new();
    if let Some(pipe) = stdout_pipe {
        let reader = std::io::BufReader::new(pipe);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    on_line(&l);
                    stdout_buf.push_str(&l);
                    stdout_buf.push('\n');
                }
                Err(_) => break,
            }
        }
    }

    let stderr_buf = stderr_thread.join().unwrap_or_default();
    for line in stderr_buf.lines() {
        on_line(line);
    }

    let status = child.wait().map_err(|e| {
        EngineError::Container(format!(
            "waiting for exec in background container {container_id}: {e}"
        ))
    })?;
    if let Some(writer) = writer {
        writer
            .join()
            .map_err(|_| EngineError::Container("private stdin writer failed".into()))?
            .map_err(|_| EngineError::Container("private stdin delivery failed".into()))?;
    }

    let exit_code = status.code().unwrap_or(-1);
    Ok(ExecOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code,
    })
}

pub(super) fn default_stop_and_remove(cli: ContainerCli, container_id: &str) {
    let _ = cli
        .command()
        .args(["stop", container_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = cli
        .command()
        .args(["rm", container_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
