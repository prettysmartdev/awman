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

use crate::engine::container::process::ContainerCli;
use crate::engine::container::runtime::wait_with_timeout;
use crate::engine::error::EngineError;

/// How long a probe (`info`, `image inspect`) may take before the runtime is
/// treated as unreachable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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
pub(super) fn cli_build_image(
    cli: ContainerCli,
    tag: &str,
    dockerfile: &Path,
    context: &Path,
    no_cache: bool,
    on_line: &mut dyn FnMut(&str),
) -> Result<(), EngineError> {
    let cli_bin = cli.bin;
    let mut child = Command::new(crate::engine::host_cli::program(cli_bin))
        .args(build_image_argv(tag, dockerfile, context, no_cache))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| EngineError::Container(format!("spawn {cli_bin} build: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Combine stdout + stderr into a single sequenced stream by spawning two
    // threads that funnel into a channel.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
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
    for line in rx {
        on_line(&line);
    }
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    let status = child
        .wait()
        .map_err(|e| EngineError::Container(format!("wait {cli_bin} build: {e}")))?;
    if !status.success() {
        return Err(EngineError::ImageBuildExitNonzero {
            tag: tag.to_string(),
            exit_code: status.code().unwrap_or(-1),
        });
    }
    Ok(())
}

/// The `build` argv, without the leading CLI binary.
fn build_image_argv(tag: &str, dockerfile: &Path, context: &Path, no_cache: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["build".into()];
    if no_cache {
        args.push("--no-cache".into());
    }
    args.extend([
        "-t".into(),
        tag.to_string(),
        "-f".into(),
        dockerfile.display().to_string(),
        context.display().to_string(),
    ]);
    args
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
