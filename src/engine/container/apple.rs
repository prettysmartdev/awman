//! Apple Containers backend — `pub(super)`. Same shape as Docker; the Apple
//! `container` CLI is a near-drop-in replacement (it shares the docker `run`
//! / `list` / `stats` / `stop` surface).

use std::process::{Command, Stdio};

use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::docker::build_run_argv;
use crate::engine::container::instance::{
    handle_now, ContainerExecution, ContainerExitInfo, ContainerId, ContainerInstance,
    ContainerStats, ExecutionBackend,
};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::error::EngineError;

const AMUX_LABEL: &str = "amux=true";

#[derive(Debug, Default)]
pub(super) struct AppleBackend;

impl AppleBackend {
    pub(super) fn new() -> Self {
        Self
    }
}

impl ContainerBackend for AppleBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn ContainerInstance>, EngineError> {
        let image = options
            .image
            .clone()
            .ok_or_else(|| EngineError::ConflictingOptions("missing required Image option".into()))?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        Ok(Box::new(AppleContainerInstance {
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
        }))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<ContainerHandle>, EngineError> {
        // Apple Containers uses `container list`, not `container ps`.
        // It does not support `--filter` for label filtering, so we list all
        // containers and filter client-side by name pattern.
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) => o,
            Err(_) => return Ok(Vec::new()),
        };
        if !output.status.success() {
            return Ok(Vec::new());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut handles = Vec::new();
        // Parse either a JSON array (the documented Apple shape) or one JSON
        // object per line (the format other CLIs sometimes emit).
        let arr: Result<Vec<serde_json::Value>, _> = serde_json::from_str(&stdout);
        let rows: Vec<serde_json::Value> = match arr {
            Ok(v) => v,
            Err(_) => stdout
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect(),
        };
        for row in rows {
            // Client-side filtering: only include containers that have the
            // amux label or whose name starts with "amux-".
            let labels = row
                .get("Labels")
                .or_else(|| row.get("labels"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // Apple `container list` outputs Names as a JSON array ["name"],
            // not a string. Handle both array and string forms.
            let row_name = {
                let val = row.get("Names")
                    .or_else(|| row.get("Name"))
                    .or_else(|| row.get("name"));
                match val {
                    Some(v) if v.is_array() => v.as_array()
                        .and_then(|a| a.first())
                        .and_then(|s| s.as_str())
                        .map(|s| s.trim_start_matches('/'))
                        .unwrap_or_default()
                        .to_string(),
                    Some(v) => v.as_str()
                        .map(|s| s.trim_start_matches('/'))
                        .unwrap_or_default()
                        .to_string(),
                    None => String::new(),
                }
            };
            if !labels.contains("amux")
                && !row_name.starts_with("amux-")
                && !row_name.contains("nanoclaw")
            {
                continue;
            }

            let id = row
                .get("ID")
                .or_else(|| row.get("Id"))
                .or_else(|| row.get("id"))
                .or_else(|| row.get("ContainerID"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = row_name;
            let image_tag = row
                .get("Image")
                .or_else(|| row.get("image"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            // Started/Created timestamp — try multiple keys in order of
            // likelihood. RFC3339-parsed when present; falls back to now().
            let started_at = row
                .get("CreatedAt")
                .or_else(|| row.get("Created"))
                .or_else(|| row.get("created"))
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now);
            if id.is_empty() && name.is_empty() {
                continue;
            }
            handles.push(ContainerHandle {
                id,
                image_tag,
                name,
                started_at,
            });
        }
        Ok(handles)
    }

    fn list_running_all(&self) -> Result<Vec<ContainerHandle>, EngineError> {
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut handles = Vec::new();
        let arr: Result<Vec<serde_json::Value>, _> = serde_json::from_str(&stdout);
        let rows: Vec<serde_json::Value> = match arr {
            Ok(v) => v,
            Err(_) => stdout
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect(),
        };
        for row in rows {
            let labels = row
                .get("Labels")
                .or_else(|| row.get("labels"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let row_name = {
                let val = row.get("Names")
                    .or_else(|| row.get("Name"))
                    .or_else(|| row.get("name"));
                match val {
                    Some(v) if v.is_array() => v.as_array()
                        .and_then(|a| a.first())
                        .and_then(|s| s.as_str())
                        .map(|s| s.trim_start_matches('/'))
                        .unwrap_or_default()
                        .to_string(),
                    Some(v) => v.as_str()
                        .map(|s| s.trim_start_matches('/'))
                        .unwrap_or_default()
                        .to_string(),
                    None => String::new(),
                }
            };
            if !labels.contains("amux")
                && !row_name.starts_with("amux-")
                && !row_name.contains("nanoclaw")
            {
                continue;
            }
            let id = row
                .get("ID")
                .or_else(|| row.get("Id"))
                .or_else(|| row.get("id"))
                .or_else(|| row.get("ContainerID"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = row_name;
            let image_tag = row
                .get("Image")
                .or_else(|| row.get("image"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let started_at = row
                .get("CreatedAt")
                .or_else(|| row.get("Created"))
                .or_else(|| row.get("created"))
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now);
            if id.is_empty() && name.is_empty() {
                continue;
            }
            handles.push(ContainerHandle {
                id,
                image_tag,
                name,
                started_at,
            });
        }
        Ok(handles)
    }

    fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError> {
        let output = Command::new("container")
            .args([
                "stats",
                "--no-stream",
                "--format",
                "json",
                &handle.name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    EngineError::ContainerRuntimeUnavailable {
                        binary: "container".into(),
                    }
                } else {
                    EngineError::Container(format!("container stats: {e}"))
                }
            })?;
        if !output.status.success() {
            return Err(EngineError::Container(format!(
                "container stats failed for {}",
                handle.name
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Same defensive JSON parsing as `list_running`: array or per-line.
        let row: serde_json::Value = serde_json::from_str(stdout.trim())
            .or_else(|_| {
                stdout
                    .lines()
                    .next()
                    .ok_or_else(|| serde_json::Error::io(std::io::Error::other("empty")))
                    .and_then(serde_json::from_str)
            })
            .map_err(|e| {
                EngineError::Container(format!("unparseable container stats output: {e}"))
            })?;

        let cpu_str = row
            .get("CPUPerc")
            .or_else(|| row.get("CPU"))
            .or_else(|| row.get("cpu"))
            .and_then(|v| v.as_str())
            .unwrap_or("0");
        let cpu_percent = cpu_str.trim().trim_end_matches('%').parse::<f64>().unwrap_or(0.0);

        let mem_str = row
            .get("MemUsage")
            .or_else(|| row.get("Memory"))
            .or_else(|| row.get("memory"))
            .and_then(|v| v.as_str())
            .unwrap_or("0");
        // Take just the "used" half of "X / Y" and unit-aware parse.
        let mem_used = mem_str.split('/').next().unwrap_or(mem_str).trim();
        let memory_mb = parse_memory_mb(mem_used);

        Ok(ContainerStats {
            name: handle.name.clone(),
            cpu_percent,
            memory_mb,
        })
    }

    fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError> {
        let _ = Command::new("container")
            .args(["stop", &handle.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("container")
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
        "apple-containers"
    }
}

struct AppleContainerInstance {
    id: ContainerId,
    name: ContainerName,
    image: ImageRef,
    options: ResolvedContainerOptions,
}

impl ContainerInstance for AppleContainerInstance {
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
        // The Apple `container` CLI honours the same `run` argv shape; reuse
        // the Docker assembler.
        let argv = build_run_argv(&self.name, &self.image, &self.options);
        let started_at = chrono::Utc::now();
        let interactive = self.options.interactive;
        let seeded = self.options.seeded_prompt.clone();
        let handle = handle_now(&self.id, &self.name, &self.image);

        // PTY-bridged path: the TUI frontend exposes a `ContainerIo`. We
        // spawn the Apple `container run -it` binary via portable-pty so the
        // PTY master is bridged into the frontend's vt100 parser.
        let pty_io = if interactive { frontend.take_container_io() } else { None };
        if let Some(io) = pty_io {
            return spawn_pty_bridged_apple(self, frontend, io, argv, started_at, handle);
        }

        let mut cmd = Command::new("container");
        cmd.args(&argv);
        if interactive {
            // Interactive (no PTY bridge): open /dev/tty directly so Apple
            // Containers gets a fresh terminal fd for PTY setup. After CLI
            // prompts have consumed buffered reads on fd 0, inheriting stdin
            // can fail with ENOTTY because Apple Containers calls
            // ioctl(TIOCGWINSZ) on the fd.
            #[cfg(unix)]
            {
                let tty_stdin = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/tty")
                    .map(std::process::Stdio::from)
                    .unwrap_or_else(|_| Stdio::inherit());
                cmd.stdin(tty_stdin);
            }
            #[cfg(not(unix))]
            cmd.stdin(Stdio::inherit());
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        } else if seeded.is_some() {
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        } else {
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        }

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::ContainerRuntimeUnavailable {
                    binary: "container".into(),
                }
            } else {
                EngineError::Container(format!("spawn container: {e}"))
            }
        })?;

        if let Some(prompt) = seeded {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(prompt.as_bytes());
                let _ = stdin.write_all(b"\n");
                drop(stdin);
            }
        }

        let backend = AppleExecution {
            child: Some(child),
            pty_child: None,
            pty_master: None,
            stdin_injector: None,
            container_name: self.name.0.clone(),
            started_at,
        };
        Ok(ContainerExecution::new(handle, Box::new(backend)))
    }
}

/// Spawn the Apple `container run -it` binary via `portable-pty` and bridge
/// the PTY master to the frontend's `ContainerIo` channels. Mirrors
/// `docker.rs::spawn_pty_bridged_docker` exactly — same reader thread,
/// writer task, and resize task — but talks to the Apple `container` CLI
/// instead of `docker`.
fn spawn_pty_bridged_apple(
    instance: Box<AppleContainerInstance>,
    _frontend: Box<dyn crate::engine::container::frontend::ContainerFrontend>,
    io: crate::engine::container::frontend::ContainerIo,
    argv: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    handle: crate::data::session::ContainerHandle,
) -> Result<ContainerExecution, EngineError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let (cols, rows) = io.initial_size;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| EngineError::Container(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new("container");
    for arg in &argv {
        cmd.arg(arg);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn container via pty: {e}")))?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Container(format!("clone pty reader: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Container(format!("take pty writer: {e}")))?;

    // Reader thread: PTY → frontend stdout channel.
    let stdout_tx = io.stdout;
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Writer task: stdin channel → PTY. Same channel feeds keystrokes from
    // the frontend AND `inject_prompt`.
    let mut stdin_rx = io.stdin_rx;
    tokio::spawn(async move {
        use std::io::Write;
        while let Some(bytes) = stdin_rx.recv().await {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    // Resize task: forward terminal resizes to the PTY master.
    let master_arc =
        std::sync::Arc::new(std::sync::Mutex::new(pair.master));
    let master_for_resize = std::sync::Arc::clone(&master_arc);
    let mut resize_rx = io.resize;
    tokio::spawn(async move {
        while let Some((cols, rows)) = resize_rx.recv().await {
            if let Ok(master) = master_for_resize.lock() {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    });

    let backend = AppleExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(io.stdin_tx),
        container_name: instance.name.0.clone(),
        started_at,
    };
    Ok(ContainerExecution::new(handle, Box::new(backend)))
}

struct AppleExecution {
    /// Set when running with inherit-stdio.
    child: Option<std::process::Child>,
    /// Set when running PTY-bridged via portable-pty.
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Held alive so the resize task and PTY writer keep working until exit.
    pty_master: Option<std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    /// Sender side of the stdin channel — used by `try_inject_stdin` to push
    /// a workflow continue-in-current prompt into the running PTY.
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    container_name: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for AppleExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<ContainerExitInfo, EngineError> {
        // PTY-bridged path: wait on the portable-pty child.
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait container (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(ContainerExitInfo {
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
            .map_err(|e| EngineError::Container(format!("wait container: {e}")))?;
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
        let _ = Command::new("container")
            .args(["stop", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("container")
            .args(["rm", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }
}

/// Parse a memory-usage string like `"123.4MiB"`, `"1.2GB"`, `"512KB"` into
/// megabytes. Unrecognized units fall back to assuming MB (consistent with
/// the legacy parser at `oldsrc/runtime/docker.rs`).
fn parse_memory_mb(s: &str) -> f64 {
    let trimmed = s.trim();
    let split_at = trimmed
        .find(|c: char| c.is_alphabetic())
        .unwrap_or(trimmed.len());
    let (num, unit) = trimmed.split_at(split_at);
    let value: f64 = num.parse().unwrap_or(0.0);
    let unit_norm: String = unit.trim().to_ascii_lowercase();
    let factor_to_mb: f64 = match unit_norm.as_str() {
        "b" => 1.0 / (1024.0 * 1024.0),
        "k" | "kb" | "kib" => 1.0 / 1024.0,
        "m" | "mb" | "mib" | "" => 1.0,
        "g" | "gb" | "gib" => 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0,
        _ => 1.0,
    };
    value * factor_to_mb
}

#[cfg(test)]
mod apple_tests {
    use super::*;

    #[test]
    fn parse_memory_mb_handles_common_units() {
        assert!((parse_memory_mb("128MiB") - 128.0).abs() < 0.001);
        assert!((parse_memory_mb("128MB") - 128.0).abs() < 0.001);
        assert!((parse_memory_mb("1.5GB") - 1536.0).abs() < 0.001);
        assert!((parse_memory_mb("512KB") - 0.5).abs() < 0.001);
        assert!((parse_memory_mb("1024B") - (1024.0 / (1024.0 * 1024.0))).abs() < 0.001);
        // No unit -> default MB
        assert!((parse_memory_mb("64") - 64.0).abs() < 0.001);
    }

    #[test]
    fn parse_memory_mb_unknown_unit_assumes_mb() {
        assert!((parse_memory_mb("128wat") - 128.0).abs() < 0.001);
    }
}
