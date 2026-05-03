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
        _frontend: Box<dyn crate::engine::container::frontend::ContainerFrontend>,
    ) -> Result<ContainerExecution, EngineError> {
        // The Apple `container` CLI honours the same `run` argv shape; reuse
        // the Docker assembler.
        let argv = build_run_argv(&self.name, &self.image, &self.options);
        let started_at = chrono::Utc::now();
        let interactive = self.options.interactive;
        let seeded = self.options.seeded_prompt.clone();
        let handle = handle_now(&self.id, &self.name, &self.image);

        let mut cmd = Command::new("container");
        cmd.args(&argv);
        if interactive && seeded.is_none() {
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
            container_name: self.name.0.clone(),
            started_at,
        };
        Ok(ContainerExecution::new(handle, Box::new(backend)))
    }
}

struct AppleExecution {
    child: Option<std::process::Child>,
    container_name: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for AppleExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<ContainerExitInfo, EngineError> {
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
