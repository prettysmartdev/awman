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

/// Extract the container name from an Apple Containers JSON row.
///
/// Apple's schema uses `configuration.id` as the container name/identifier
/// (there is no separate short hex ID). Falls back to Docker-style fields
/// for forward-compatibility.
fn extract_apple_name(row: &serde_json::Value) -> String {
    if let Some(id) = row
        .get("configuration")
        .and_then(|c| c.get("id"))
        .and_then(|v| v.as_str())
    {
        return id.to_string();
    }
    let val = row
        .get("Names")
        .or_else(|| row.get("Name"))
        .or_else(|| row.get("name"));
    match val {
        Some(v) if v.is_array() => v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or_default()
            .to_string(),
        Some(v) => v
            .as_str()
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or_default()
            .to_string(),
        None => String::new(),
    }
}

/// Extract the image reference from an Apple Containers JSON row.
///
/// Apple stores the image as `configuration.image` (an object); we serialize
/// it for display. Falls back to Docker-style string `Image`/`image` fields.
fn extract_apple_image(row: &serde_json::Value) -> String {
    if let Some(img_obj) = row.get("configuration").and_then(|c| c.get("image")) {
        if let Some(s) = img_obj.as_str() {
            return s.to_string();
        }
        // Apple Containers stores the image name in descriptor.annotations.
        if let Some(s) = img_obj
            .get("descriptor")
            .and_then(|d| d.get("annotations"))
            .and_then(|a| a.get("com.apple.containerization.image.name"))
            .and_then(|v| v.as_str())
        {
            return s.to_string();
        }
        // Apple Containers uses "reference" for a full OCI image reference string.
        if let Some(s) = img_obj.get("reference").and_then(|v| v.as_str()) {
            return s.to_string();
        }
        if let Some(repo) = img_obj.get("repository").and_then(|v| v.as_str()) {
            return match img_obj.get("tag").and_then(|v| v.as_str()) {
                Some(tag) if !tag.is_empty() => format!("{repo}:{tag}"),
                _ => repo.to_string(),
            };
        }
        return serde_json::to_string(img_obj).unwrap_or_default();
    }
    row.get("Image")
        .or_else(|| row.get("image"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Extract the started-at timestamp from an Apple Containers JSON row.
///
/// Apple uses `startedDate` (float epoch seconds). Falls back to
/// Docker-style `CreatedAt`/`Created` RFC3339 strings.
fn extract_apple_started_at(row: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    if let Some(ts) = row.get("startedDate").and_then(|v| v.as_f64()) {
        let secs = ts as i64;
        let nanos = ((ts - secs as f64) * 1_000_000_000.0) as u32;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
            return dt;
        }
    }
    row.get("CreatedAt")
        .or_else(|| row.get("Created"))
        .or_else(|| row.get("created"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now)
}

/// Check whether the row represents a running container.
///
/// Apple uses a `status` field: "running" | "stopped" | "stopping" | "unknown".
/// If absent (Docker-style output from `ps`), assume running.
fn is_apple_running(row: &serde_json::Value) -> bool {
    match row.get("status").and_then(|v| v.as_str()) {
        Some(s) => s == "running",
        None => true,
    }
}

/// Check whether the row's name matches amux container patterns.
fn is_amux_container(name: &str) -> bool {
    name.starts_with("amux-") || name.contains("nanoclaw")
}

/// Parse the JSON output of `container list --format json` into container
/// handles, filtering for running amux containers.
fn parse_apple_list_output(stdout: &str) -> Vec<ContainerHandle> {
    let arr: Result<Vec<serde_json::Value>, _> = serde_json::from_str(stdout);
    let rows: Vec<serde_json::Value> = match arr {
        Ok(v) => v,
        Err(_) => stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    };
    let mut handles = Vec::new();
    for row in rows {
        if !is_apple_running(&row) {
            continue;
        }
        let name = extract_apple_name(&row);
        if !is_amux_container(&name) {
            continue;
        }
        let id = name.clone();
        let image_tag = extract_apple_image(&row);
        let started_at = extract_apple_started_at(&row);
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
    handles
}

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
        let image = options.image.clone().ok_or_else(|| {
            EngineError::ConflictingOptions("missing required Image option".into())
        })?;
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
        Ok(parse_apple_list_output(&stdout))
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
        Ok(parse_apple_list_output(&stdout))
    }

    fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError> {
        // Apple's `container stats --no-stream --format json` emits raw
        // counters: `cpuUsageUsec` (cumulative CPU time) and
        // `memoryUsageBytes`. Computing CPU% from a single sample isn't
        // possible, so take two samples ~200ms apart and divide the delta
        // by elapsed wall-clock time. Mirrors old-amux behavior.
        let take_sample = |name: &str| -> Result<(u64, u64), EngineError> {
            let out = Command::new("container")
                .args(["stats", "--no-stream", "--format", "json", name])
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
            if !out.status.success() {
                return Err(EngineError::Container(format!(
                    "container stats failed for {}",
                    name
                )));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Apple emits a JSON array; some other versions emit per-line.
            let value: serde_json::Value = serde_json::from_str(stdout.trim())
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
            let entry = match &value {
                serde_json::Value::Array(arr) => {
                    arr.first().cloned().unwrap_or(serde_json::Value::Null)
                }
                _ => value,
            };
            let cpu = entry
                .get("cpuUsageUsec")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mem = entry
                .get("memoryUsageBytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Ok((cpu, mem))
        };

        let (cpu1, _) = take_sample(&handle.name)?;
        let t0 = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (cpu2, mem) = take_sample(&handle.name)?;
        let elapsed_usec = t0.elapsed().as_micros() as u64;

        let cpu_delta = cpu2.saturating_sub(cpu1);
        let cpu_percent = if elapsed_usec > 0 {
            (cpu_delta as f64 / elapsed_usec as f64) * 100.0
        } else {
            0.0
        };
        let memory_mb = (mem as f64) / (1024.0 * 1024.0);

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
        frontend.report_status(
            crate::engine::container::frontend::ContainerStatus::Running {
                container_name: self.name.0.clone(),
            },
        );

        let pty_io = if interactive {
            frontend.take_container_io()
        } else {
            None
        };
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
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
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
    let master_arc = std::sync::Arc::new(std::sync::Mutex::new(pair.master));
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

    fn cancel_handle(&self) -> Option<super::instance::CancelHandle> {
        let name = self.container_name.clone();
        Some(super::instance::CancelHandle::new(move || {
            let _ = Command::new("container")
                .args(["stop", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = Command::new("container")
                .args(["rm", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            Ok(())
        }))
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
        assert!((parse_memory_mb("64") - 64.0).abs() < 0.001);
    }

    #[test]
    fn parse_memory_mb_unknown_unit_assumes_mb() {
        assert!((parse_memory_mb("128wat") - 128.0).abs() < 0.001);
    }

    #[test]
    fn parse_apple_list_picks_up_running_amux_containers() {
        let json = r#"[
            {
                "status": "running",
                "configuration": {
                    "id": "amux-12345-999",
                    "image": {"repository": "amux/dev", "tag": "latest"}
                },
                "startedDate": 1715000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "amux-claws-controller",
                    "image": {"repository": "amux/dev", "tag": "latest"}
                },
                "startedDate": 1715000100.5
            },
            {
                "status": "stopped",
                "configuration": {
                    "id": "amux-old-stopped",
                    "image": {"repository": "amux/dev", "tag": "latest"}
                },
                "startedDate": 1714000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "unrelated-container",
                    "image": {"repository": "nginx", "tag": "latest"}
                },
                "startedDate": 1715000200.0
            }
        ]"#;
        let handles = parse_apple_list_output(json);
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].name, "amux-12345-999");
        assert_eq!(handles[0].id, "amux-12345-999");
        assert_eq!(handles[1].name, "amux-claws-controller");
    }

    #[test]
    fn parse_apple_list_handles_nanoclaw_containers() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "nanoclaw-worker-1",
                "image": {"repository": "amux/dev"}
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].name, "nanoclaw-worker-1");
    }

    #[test]
    fn parse_apple_list_empty_array() {
        let handles = parse_apple_list_output("[]");
        assert!(handles.is_empty());
    }

    #[test]
    fn parse_apple_list_skips_non_running() {
        let json = r#"[{
            "status": "stopping",
            "configuration": { "id": "amux-dying" },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json);
        assert!(handles.is_empty());
    }

    #[test]
    fn extract_apple_image_formats_repo_and_tag() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"repository": "amux/dev", "tag": "latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "amux/dev:latest");
    }

    #[test]
    fn extract_apple_image_repo_only_without_tag() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": {"repository": "amux/dev"}}}"#)
                .unwrap();
        assert_eq!(extract_apple_image(&row), "amux/dev");
    }

    #[test]
    fn extract_apple_image_plain_string() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": "amux/dev:latest"}}"#).unwrap();
        assert_eq!(extract_apple_image(&row), "amux/dev:latest");
    }

    #[test]
    fn extract_apple_image_reference_field() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"reference": "ghcr.io/amux/dev:latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "ghcr.io/amux/dev:latest");
    }

    #[test]
    fn extract_apple_image_descriptor_annotations() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{
            "configuration": {
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "amux-amux-claude:latest"
                        }
                    }
                }
            }
        }"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "amux-amux-claude:latest");
    }

    #[test]
    fn parse_apple_list_formats_image_correctly() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "amux-test",
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "amux-amux-claude:latest"
                        }
                    }
                }
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].image_tag, "amux-amux-claude:latest");
    }

    #[test]
    fn extract_apple_started_at_from_float() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"startedDate": 1715000000.5}"#).unwrap();
        let dt = extract_apple_started_at(&row);
        assert_eq!(dt.timestamp(), 1715000000);
    }
}
