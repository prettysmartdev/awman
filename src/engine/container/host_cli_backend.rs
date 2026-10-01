//! The CLI-shaped backend operations Docker and Apple Containers share —
//! `pub(super)`.
//!
//! `ContainerBackend` has no default that shells out: every backend states
//! each operation explicitly. The two backends that drive a host CLI (`docker`,
//! Apple's `container`) spell these operations with identical argv, so their
//! implementations delegate here, parameterised by the backend's
//! [`ContainerCli`]. A backend that drives no CLI (builtin) never calls into
//! this module.
//!
//! Every spawn resolves its program through `host_cli::program`, so test
//! isolation applies exactly as it did when these bodies lived in
//! `backend.rs` and `runtime.rs`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use std::time::Instant;

use crate::data::oci_identity::{Digest, OciPlatform};
use crate::engine::oci::CancelToken;

use crate::engine::container::process::ContainerCli;
use crate::engine::container::runtime::wait_with_timeout;
use crate::engine::error::EngineError;

/// How long a probe (`info`, `image inspect`) may take before the runtime is
/// treated as unreachable.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `<cli> <probe_args>`; `Ok` when it exits zero within the probe timeout.
///
/// The error says why: the CLI is missing, the probe timed out, or it exited
/// non-zero (daemon down, logged out, …).
pub(super) fn cli_is_available(cli: ContainerCli, probe_args: &[&str]) -> Result<(), EngineError> {
    let child = Command::new(crate::engine::host_cli::program(cli.bin))
        .args(probe_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| spawn_error(cli, &probe_args.join(" "), e))?;
    match wait_with_timeout(child, PROBE_TIMEOUT) {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(EngineError::Container(format!(
            "`{} {}` exited with {}",
            cli.bin,
            probe_args.join(" "),
            status.code().unwrap_or(-1)
        ))),
        None => Err(EngineError::Container(format!(
            "`{} {}` did not answer within {}s",
            cli.bin,
            probe_args.join(" "),
            PROBE_TIMEOUT.as_secs()
        ))),
    }
}

/// Whether `tag` exists in the CLI's local image store
/// (`<cli> image inspect <tag>`). Best-effort: a missing CLI, an unreachable
/// daemon or a timeout all answer `Ok(false)`, as they always have.
pub(super) fn cli_image_exists(cli: ContainerCli, tag: &str) -> Result<bool, EngineError> {
    let child = Command::new(crate::engine::host_cli::program(cli.bin))
        .args(["image", "inspect", tag])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    Ok(match child {
        Ok(child) => wait_with_timeout(child, PROBE_TIMEOUT)
            .map(|s| s.success())
            .unwrap_or(false),
        Err(_) => false,
    })
}

/// `<cli> build [--no-cache] -t <tag> -f <dockerfile> <context>`, streaming
/// stdout and stderr line-by-line through `on_line`. Both CLIs share the
/// `build` argv shape.
///
/// The ordinary Docker/Apple runtime build: no deadline (large images may
/// take hours), a missing CLI is `ContainerRuntimeUnavailable`, and a zero
/// exit status is success even if a descendant keeps the output pipes open.
pub(super) fn cli_build_image(
    cli: ContainerCli,
    tag: &str,
    dockerfile: &Path,
    context: &Path,
    no_cache: bool,
    on_line: &mut dyn FnMut(&str),
) -> Result<(), EngineError> {
    let cli_bin = cli.bin;
    let mut command = Command::new(crate::engine::host_cli::program(cli_bin));
    command.args(build_image_argv(tag, dockerfile, context, no_cache));
    let status = ProcessHostCommandRunner
        .run(
            command,
            &BuildControl::unbounded(CancelToken::new()),
            on_line,
        )
        .map_err(|e| match e {
            EngineError::ContainerRuntimeUnavailable { .. } => {
                EngineError::ContainerRuntimeUnavailable {
                    binary: cli_bin.to_string(),
                }
            }
            other => other,
        })?;
    if !status.success() {
        return Err(EngineError::ImageBuildExitNonzero {
            tag: tag.to_string(),
            exit_code: status.code().unwrap_or(-1),
        });
    }
    Ok(())
}

/// A cancellation token and optional wall-clock bound for host CLI calls.
#[derive(Clone)]
pub struct BuildControl {
    cancel: CancelToken,
    deadline: Option<Instant>,
}

impl BuildControl {
    /// Cancellable and bounded by `timeout` from now.
    pub fn new(cancel: CancelToken, timeout: Duration) -> Self {
        Self {
            cancel,
            deadline: Instant::now().checked_add(timeout),
        }
    }

    /// Cancellable, with no wall-clock bound.
    pub fn unbounded(cancel: CancelToken) -> Self {
        Self {
            cancel,
            deadline: None,
        }
    }

    /// The same cancellation, bounded by at most `timeout` from now (or this
    /// control's own deadline, if sooner). Probes and inspections inside a
    /// long build use it so a hung daemon cannot stall them for the build's
    /// whole budget.
    pub fn bounded(&self, timeout: Duration) -> Self {
        let mine = Instant::now().checked_add(timeout);
        let deadline = match (self.deadline, mine) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Self {
            cancel: self.cancel.clone(),
            deadline,
        }
    }

    pub fn check(&self) -> Result<(), EngineError> {
        self.cancel.check()?;
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(EngineError::Container("image build timed out".into()));
        }
        Ok(())
    }

    /// When waiting must stop. An unbounded control reports a distant
    /// instant, for APIs that need one.
    pub fn deadline(&self) -> Instant {
        self.deadline.unwrap_or_else(|| {
            let now = Instant::now();
            now.checked_add(Duration::from_secs(365 * 24 * 3600))
                .unwrap_or(now)
        })
    }
}

/// Process seam for daemon-free ready tests. The caller builds `Command` so a
/// recording runner can examine argv, environment removals and executable.
pub trait HostCommandRunner: Send + Sync {
    fn run(
        &self,
        command: Command,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<std::process::ExitStatus, EngineError>;
}

pub struct ProcessHostCommandRunner;

impl HostCommandRunner for ProcessHostCommandRunner {
    fn run(
        &self,
        mut command: Command,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<std::process::ExitStatus, EngineError> {
        control.check()?;
        let program = command.get_program().to_string_lossy().to_string();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    EngineError::ContainerRuntimeUnavailable {
                        binary: program.clone(),
                    }
                } else {
                    EngineError::Container(format!("{program}: {e}"))
                }
            })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Bounded line queue; cancellation is polled even when the child emits
        // nothing. Readers are joined after the child is reaped.
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(64);
        let tx_out = tx.clone();
        let stdout_handle = std::thread::spawn(move || {
            if let Some(out) = stdout {
                let r = BufReader::new(out);
                for line in r.lines().map_while(Result::ok) {
                    let _ = tx_out.send(line);
                }
            }
        });
        let stderr_handle = std::thread::spawn(move || {
            if let Some(err) = stderr {
                let r = BufReader::new(err);
                for line in r.lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            }
        });
        let result = loop {
            while let Ok(line) = rx.try_recv() {
                on_line(&line);
            }
            if let Err(e) = control.check() {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e);
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(EngineError::Container(format!("wait for {program}: {e}")));
                }
            }
        };
        // Deliver the output still buffered after exit. A drain that is
        // making progress (a slow `on_line`) keeps going; only a quiet pipe
        // held open by a descendant ends it early.
        let mut quiet_since = Instant::now();
        let mut disconnected = false;
        while quiet_since.elapsed() < DRAIN_QUIET {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => {
                    on_line(&line);
                    quiet_since = Instant::now();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        // A child that leaves descendants holding its pipes must not stall
        // ready forever. Closing the receiver releases blocked senders; the
        // reader threads then detach if an inherited pipe remains open. The
        // exit status still decides success: losing trailing output is not a
        // build failure.
        drop(rx);
        if disconnected {
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
        } else {
            tracing::warn!(
                program = %program,
                "host command exited but its output pipes stayed open; detaching readers"
            );
        }
        result
    }
}

/// How long the post-exit drain waits without receiving a line.
const DRAIN_QUIET: Duration = Duration::from_secs(2);

/// Environment that could redirect a Docker command away from the endpoint
/// it is bound to.
const DOCKER_AMBIENT_ENV: &[&str] = &[
    "DOCKER_CONTEXT",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
    "BUILDX_BUILDER",
];

/// A `docker` command bound to `endpoint`: `--host` plus a matching
/// `DOCKER_HOST` (which CLI plugins such as buildx also read), with every
/// ambient context/TLS/builder selector removed. The one place awman binds
/// a source-matched Docker command, for probes, inspection and builds alike.
pub(crate) fn bound_docker_command(endpoint: &str) -> Command {
    let mut command = Command::new(crate::engine::host_cli::program("docker"));
    command.args(["--host", endpoint]);
    for key in DOCKER_AMBIENT_ENV {
        command.env_remove(key);
    }
    command.env("DOCKER_HOST", endpoint);
    command
}

/// Source-bound Docker build. Runs the same `build` argv as the Docker
/// runtime ([`BuildArgv`]), plus an explicit platform and an iidfile.
pub(crate) struct HostBuildRequest<'a> {
    pub endpoint: &'a str,
    pub tag: &'a str,
    pub dockerfile: &'a Path,
    pub context: &'a Path,
    pub platform: &'a OciPlatform,
    pub build_args: &'a [(String, String)],
    pub no_cache: bool,
}

/// How long a source-matched build may run.
pub(crate) const SOURCE_BUILD_TIMEOUT: Duration = Duration::from_secs(3600);

/// Which Docker build path a bound endpoint uses, decided the way the
/// `docker` CLI decides it: BuildKit through buildx when buildx is
/// installed, the legacy builder otherwise. awman never downgrades a
/// buildx-capable CLI to the legacy builder.
enum DockerBuildPath {
    /// buildx is installed and the endpoint's `default` builder uses the
    /// `docker` driver, so its output is stored in that same Engine.
    BuildKit,
    /// No buildx: `docker build` uses the legacy builder, as it would for
    /// the Docker runtime on this host.
    Legacy,
}

fn docker_build_path(
    runner: &dyn HostCommandRunner,
    endpoint: &str,
    control: &BuildControl,
) -> Result<DockerBuildPath, EngineError> {
    let probe = control.bounded(PROBE_TIMEOUT);
    let mut version = bound_docker_command(endpoint);
    version.args(["buildx", "version"]);
    if !runner.run(version, &probe, &mut |_| {})?.success() {
        return Ok(DockerBuildPath::Legacy);
    }
    let mut inspect = bound_docker_command(endpoint);
    inspect.args(["buildx", "inspect", "default"]);
    let mut lines = Vec::new();
    let status = runner.run(inspect, &probe, &mut |line| lines.push(line.to_string()))?;
    let driver = lines.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "Driver").then(|| value.trim().to_string())
    });
    match (status.success(), driver.as_deref()) {
        (true, Some("docker")) => Ok(DockerBuildPath::BuildKit),
        (true, other) => Err(EngineError::Container(format!(
            "the Docker builder `default` for {endpoint} uses the `{}` driver, which does not \
             store images in that Engine; awman builds only with the `docker` driver",
            other.unwrap_or("unknown")
        ))),
        (false, _) => Err(EngineError::Container(format!(
            "`docker buildx inspect default` failed for {endpoint} (exit {}): {}",
            status.code().unwrap_or(-1),
            tail(&lines)
        ))),
    }
}

/// Run a source-bound Docker build and return the image ID the build itself
/// wrote to its iidfile. The caller classifies that ID by inspecting it.
pub(crate) fn build_local_docker(
    runner: &dyn HostCommandRunner,
    request: &HostBuildRequest<'_>,
    control: &BuildControl,
    on_line: &mut dyn FnMut(&str),
) -> Result<Digest, EngineError> {
    let path = docker_build_path(runner, request.endpoint, control)?;
    let iid = tempfile::NamedTempFile::new()
        .map_err(|e| EngineError::Container(format!("create build receipt: {e}")))?;
    let platform = request.platform.to_string();
    let argv = BuildArgv {
        buildx: matches!(path, DockerBuildPath::BuildKit),
        platform: Some(&platform),
        iidfile: Some(iid.path()),
        build_args: request.build_args,
        ..BuildArgv::new(
            request.tag,
            request.dockerfile,
            request.context,
            request.no_cache,
        )
    };
    let mut command = bound_docker_command(request.endpoint);
    command.args(argv.to_args());
    let status = runner.run(command, control, on_line)?;
    if !status.success() {
        return Err(EngineError::ImageBuildExitNonzero {
            tag: request.tag.into(),
            exit_code: status.code().unwrap_or(-1),
        });
    }
    let raw = std::fs::read_to_string(iid.path())
        .map_err(|e| EngineError::Container(format!("read build receipt: {e}")))?;
    Digest::parse(raw.trim())
        .map_err(|e| EngineError::Container(format!("invalid Docker build receipt: {e}")))
}

/// The last few lines of a command's output, for error messages.
pub(crate) fn tail(lines: &[String]) -> String {
    let start = lines.len().saturating_sub(3);
    let text = lines[start..]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    if text.is_empty() {
        "no output".to_string()
    } else {
        text.chars().take(400).collect()
    }
}

/// The one `build` argv shared by the Docker/Apple runtimes and the
/// source-matched builder. Options the ordinary runtime build does not use
/// are absent from its argv, which is unchanged.
pub(crate) struct BuildArgv<'a> {
    pub tag: &'a str,
    pub dockerfile: &'a Path,
    pub context: &'a Path,
    pub no_cache: bool,
    /// `docker buildx build` with the endpoint's `docker`-driver builder,
    /// without attestation manifests (so the stored image is one platform
    /// manifest, not an index), instead of plain `build`.
    pub buildx: bool,
    pub platform: Option<&'a str>,
    pub iidfile: Option<&'a Path>,
    pub build_args: &'a [(String, String)],
}

impl<'a> BuildArgv<'a> {
    pub fn new(tag: &'a str, dockerfile: &'a Path, context: &'a Path, no_cache: bool) -> Self {
        Self {
            tag,
            dockerfile,
            context,
            no_cache,
            buildx: false,
            platform: None,
            iidfile: None,
            build_args: &[],
        }
    }

    /// The argv, without the leading CLI binary (or its global flags).
    pub fn to_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        if self.buildx {
            args.extend([
                "buildx".into(),
                "build".into(),
                "--builder".into(),
                "default".into(),
                "--provenance=false".into(),
                "--sbom=false".into(),
            ]);
        } else {
            args.push("build".into());
        }
        if let Some(platform) = self.platform {
            args.extend(["--platform".into(), platform.to_string()]);
        }
        if let Some(iidfile) = self.iidfile {
            args.extend(["--iidfile".into(), iidfile.display().to_string()]);
        }
        if self.no_cache {
            args.push("--no-cache".into());
        }
        for (key, value) in self.build_args {
            args.extend(["--build-arg".into(), format!("{key}={value}")]);
        }
        args.extend([
            "-t".into(),
            self.tag.to_string(),
            "-f".into(),
            self.dockerfile.display().to_string(),
            self.context.display().to_string(),
        ]);
        args
    }
}

/// The ordinary runtime `build` argv, without the leading CLI binary.
fn build_image_argv(tag: &str, dockerfile: &Path, context: &Path, no_cache: bool) -> Vec<String> {
    BuildArgv::new(tag, dockerfile, context, no_cache).to_args()
}

/// `<cli> <subcommand> <target>` — container removal (`rm`) or image removal
/// (`rmi`). Errors on a non-zero exit so callers can count per-item failures.
pub(super) fn cli_remove(
    cli: ContainerCli,
    subcommand: &'static str,
    target: &str,
) -> Result<(), EngineError> {
    let cli_bin = cli.bin;
    let output = Command::new(crate::engine::host_cli::program(cli_bin))
        .args([subcommand, target])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| spawn_error(cli, &format!("{subcommand} {target}"), e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(EngineError::Container(format!(
            "{cli_bin} {subcommand} {target} failed: {stderr}"
        )));
    }
    Ok(())
}

/// Argv (after the CLI name) for `exec -it` into a running container. Used by
/// TUI re-attach. Docker and Apple accept the identical argv.
pub(super) fn cli_exec_args(
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

/// A spawn failure: `NotFound` means the CLI is not installed.
fn spawn_error(cli: ContainerCli, what: &str, e: std::io::Error) -> EngineError {
    if e.kind() == std::io::ErrorKind::NotFound {
        EngineError::ContainerRuntimeUnavailable {
            binary: cli.bin.to_string(),
        }
    } else {
        EngineError::Container(format!("{} {what}: {e}", cli.bin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_cli() -> ContainerCli {
        ContainerCli {
            bin: "/nonexistent/awman-host-cli-backend-test/cli",
            ..ContainerCli::DOCKER
        }
    }

    #[test]
    fn exec_args_shape_is_unchanged() {
        let args = cli_exec_args("ctr", "/w", &["claude", "--x"], &[("COLUMNS", "80")]);
        assert_eq!(
            args,
            vec![
                "exec",
                "-it",
                "-w",
                "/w",
                "-e",
                "COLUMNS=80",
                "ctr",
                "claude",
                "--x"
            ]
        );
    }

    #[test]
    fn build_argv_shape_is_unchanged() {
        let args = build_image_argv(
            "awman-x:latest",
            Path::new("/r/Dockerfile.dev"),
            Path::new("/r"),
            true,
        );
        assert_eq!(
            args,
            vec![
                "build",
                "--no-cache",
                "-t",
                "awman-x:latest",
                "-f",
                "/r/Dockerfile.dev",
                "/r"
            ]
        );
        assert!(
            !build_image_argv("t", Path::new("f"), Path::new("c"), false)
                .contains(&"--no-cache".to_string())
        );
    }

    #[test]
    fn missing_cli_is_reported_precisely() {
        let cli = missing_cli();
        match cli_is_available(cli, &["info"]) {
            Err(EngineError::ContainerRuntimeUnavailable { binary }) => {
                assert_eq!(binary, cli.bin)
            }
            other => panic!("expected ContainerRuntimeUnavailable, got {other:?}"),
        }
        assert!(matches!(
            cli_remove(cli, "rm", "x"),
            Err(EngineError::ContainerRuntimeUnavailable { .. })
        ));
        // Best-effort probe: a missing CLI is "no such image", not an error.
        assert!(!cli_image_exists(cli, "x").unwrap());
    }
}
