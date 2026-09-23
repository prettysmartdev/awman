//! Apple Containers backend — `pub(super)`. Same shape as Docker; the Apple
//! `container` CLI is a near-drop-in replacement (it shares the docker `run`
//! / `list` / `stats` / `stop` surface).

use std::process::{Command, Stdio};

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::execution::{AgentInstance, AgentStats};
use crate::engine::container::attach_socket::AttachSocketGuard;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::options::{ContainerName, ResolvedContainerOptions};
use crate::engine::container::process::{AttachHookCtx, ContainerCli, ContainerInstance};
use crate::engine::credential_refresh::register_container_leases;
use crate::engine::error::EngineError;

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

/// Check whether the row's name matches awman container patterns.
fn is_awman_container(name: &str) -> bool {
    name.starts_with("awman-") || name.contains("nanoclaw")
}

/// Parse the JSON output of `container list --format json` into container
/// handles, filtering for running awman containers. When `name_prefix` is
/// `Some`, additionally require the container name to start with it — Apple
/// has no server-side name filter, so name-prefix discovery is done here,
/// client-side.
fn parse_apple_list_output(stdout: &str, name_prefix: Option<&str>) -> Vec<AgentHandle> {
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
        if !is_awman_container(&name) {
            continue;
        }
        if let Some(prefix) = name_prefix {
            if !name.starts_with(prefix) {
                continue;
            }
        }
        let id = name.clone();
        let image_tag = extract_apple_image(&row);
        let started_at = extract_apple_started_at(&row);
        if id.is_empty() && name.is_empty() {
            continue;
        }
        handles.push(AgentHandle {
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

impl ContainerBackend for AppleBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        let image = options.image.clone().ok_or_else(|| {
            EngineError::ConflictingOptions("missing required Image option".into())
        })?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        // Register a refresh lease for every file-delivered credential BEFORE
        // the container is built and its child spawned — the single choke point
        // (INV-6). No-op for every launch that carries no such credential.
        let leases = register_container_leases(&options, &name.0);
        // Apple's CLI has no `attach` verb, so the launching process must
        // serve the container's PTY itself; `serve_attach_socket` is that hook.
        Ok(Box::new(ContainerInstance::new(
            ContainerCli::APPLE,
            image,
            name,
            options,
            leases,
            Some(serve_attach_socket),
        )))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
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
        Ok(parse_apple_list_output(&stdout, None))
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
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
        Ok(parse_apple_list_output(&stdout, None))
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
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

        Ok(AgentStats {
            name: handle.name.clone(),
            cpu_percent,
            memory_mb,
        })
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        // Apple's `container` CLI has no `attach` subcommand, so reattach
        // goes through the awman-owned rendezvous instead: the process that
        // launched the container serves its live PTY on a per-container unix
        // socket (see `attach_socket.rs`), and this instance connects to it.
        // Docker keeps its native `docker attach`; the semantics are the same
        // either way — the real agent TTY, never a sibling shell.
        let path = crate::engine::container::attach_socket::attach_socket_path(&handle.name)
            .ok_or_else(|| {
                EngineError::Container(
                    "cannot resolve the attach socket directory (no home directory)".into(),
                )
            })?;
        Ok(Box::new(
            crate::engine::container::attach_socket::SocketAttachInstance {
                handle: handle.clone(),
                path,
            },
        ))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        // Apple has no server-side name filter; list everything and filter by
        // the prefix client-side.
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
        Ok(parse_apple_list_output(&stdout, Some(prefix)))
    }

    fn name(&self) -> &'static str {
        "apple-containers"
    }

    fn display_name(&self) -> &'static str {
        "Apple Containers"
    }

    fn cli_binary(&self) -> &'static str {
        "container"
    }

    fn availability_probe_args(&self) -> &'static [&'static str] {
        &["system", "status"]
    }

    fn image_home_dir(&self, tag: &str) -> Option<String> {
        // `container image inspect` emits a JSON array of variants; the env
        // list lives at `[0].variants[*].config.config.Env`. We pick the
        // first variant whose env contains a non-empty `HOME=…` entry, which
        // matches the runtime selection for single-platform images.
        let output = Command::new("container")
            .args(["image", "inspect", tag])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: serde_json::Value = serde_json::from_str(&stdout).ok()?;
        let variants = arr.get(0)?.get("variants")?.as_array()?;
        for variant in variants {
            let env = variant
                .get("config")
                .and_then(|c| c.get("config"))
                .and_then(|c| c.get("Env"))
                .and_then(|v| v.as_array());
            let Some(env) = env else { continue };
            for entry in env {
                if let Some(rest) = entry.as_str().and_then(|s| s.strip_prefix("HOME=")) {
                    let v = rest.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
        None
    }
}

/// Resize the PTY behind an attach client's resize request, guaranteeing the
/// agent receives a SIGWINCH even when the requested size equals the PTY's
/// current size.
///
/// The attach rendezvous relies on the client's initial resize as its repaint
/// trigger (see `attach_socket.rs`): the agent gets WINCH, redraws, and the
/// fresh client's screen fills. But the kernel only delivers SIGWINCH when the
/// size actually *changes* — so a client reattaching at the same terminal
/// dimensions as a previous session (the ordinary detach → reattach flow)
/// would otherwise get a silent no-op resize, no repaint, and a blank screen
/// until the agent spontaneously produced output. Bounce through an
/// off-by-one-row size first so every attach-client resize repaints.
fn resize_pty_forcing_winch(master: &dyn portable_pty::MasterPty, cols: u16, rows: u16) {
    let unchanged = master
        .get_size()
        .map(|size| size.cols == cols && size.rows == rows)
        .unwrap_or(false);
    if unchanged {
        let _ = master.resize(portable_pty::PtySize {
            rows: if rows > 1 { rows - 1 } else { rows + 1 },
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    let _ = master.resize(portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    });
}

/// The Apple backend's `process::AttachHook`, run once the PTY bridge is up.
///
/// Apple's `container` CLI has no attach verb, so this process — the sole
/// holder of the container's PTY — is the attach rendezvous: it serves the
/// PTY over a per-container unix socket that `AppleBackend::attach` clients
/// connect to. The shared spawn path installs the output tap on the bridge
/// (`AttachHookCtx::output_broadcast`) and hands us the stdin injector and a
/// weak PTY master; everything below this line is Apple's alone.
///
/// Returns `None` — after a warning — when the socket cannot be created. The
/// container still runs; it simply cannot be attached to.
fn serve_attach_socket(ctx: AttachHookCtx<'_>) -> Option<AttachSocketGuard> {
    let AttachHookCtx {
        container_name,
        output_broadcast,
        stdin_injector,
        pty_master,
    } = ctx;

    // The master reference is weak: the attach server must never keep the PTY
    // alive past the execution backend that owns it.
    let resize: std::sync::Arc<dyn Fn(u16, u16) + Send + Sync> =
        std::sync::Arc::new(move |cols, rows| {
            if let Some(master) = pty_master.upgrade() {
                if let Ok(master) = master.lock() {
                    resize_pty_forcing_winch(master.as_ref(), cols, rows);
                }
            }
        });

    let path = match crate::engine::container::attach_socket::attach_socket_path(container_name) {
        Some(path) => path,
        None => {
            tracing::warn!(
                container = %container_name,
                "no home directory to place the attach socket in; the container \
                 runs but cannot be attached to"
            );
            return None;
        }
    };

    crate::engine::container::attach_socket::spawn_attach_socket_server(
        &path,
        crate::engine::container::attach_socket::AttachHooks {
            output: output_broadcast,
            stdin: stdin_injector,
            resize,
        },
    )
    .map_err(|error| {
        tracing::warn!(
            container = %container_name,
            error = %error,
            "attach socket unavailable; the container runs but cannot be attached to"
        );
    })
    .ok()
}

#[cfg(test)]
mod apple_tests {
    use super::*;

    #[test]
    fn parse_apple_list_picks_up_running_awman_containers() {
        let json = r#"[
            {
                "status": "running",
                "configuration": {
                    "id": "awman-12345-999",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1715000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "awman-claws-controller",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1715000100.5
            },
            {
                "status": "stopped",
                "configuration": {
                    "id": "awman-old-stopped",
                    "image": {"repository": "awman/dev", "tag": "latest"}
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
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].name, "awman-12345-999");
        assert_eq!(handles[0].id, "awman-12345-999");
        assert_eq!(handles[1].name, "awman-claws-controller");
    }

    #[test]
    fn parse_apple_list_handles_nanoclaw_containers() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "nanoclaw-worker-1",
                "image": {"repository": "awman/dev"}
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].name, "nanoclaw-worker-1");
    }

    #[test]
    fn parse_apple_list_empty_array() {
        let handles = parse_apple_list_output("[]", None);
        assert!(handles.is_empty());
    }

    #[test]
    fn parse_apple_list_skips_non_running() {
        let json = r#"[{
            "status": "stopping",
            "configuration": { "id": "awman-dying" },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert!(handles.is_empty());
    }

    #[test]
    fn extract_apple_image_formats_repo_and_tag() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"repository": "awman/dev", "tag": "latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_repo_only_without_tag() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": {"repository": "awman/dev"}}}"#)
                .unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev");
    }

    #[test]
    fn extract_apple_image_plain_string() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": "awman/dev:latest"}}"#).unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_reference_field() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"reference": "ghcr.io/awman/dev:latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "ghcr.io/awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_descriptor_annotations() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{
            "configuration": {
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "awman-myproj-claude:latest"
                        }
                    }
                }
            }
        }"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "awman-myproj-claude:latest");
    }

    #[test]
    fn parse_apple_list_formats_image_correctly() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "awman-test",
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "awman-myproj-claude:latest"
                        }
                    }
                }
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].image_tag, "awman-myproj-claude:latest");
    }

    #[test]
    fn image_home_dir_returns_none_for_unknown_image() {
        // `container image inspect` exits non-zero for an unknown tag, and
        // the helper must collapse that to `None` rather than panic — also
        // covers the case where the `container` CLI itself isn't installed.
        let backend = AppleBackend;
        let bogus = "awman-test-image-that-does-not-exist:tag-xyz123";
        assert!(backend.image_home_dir(bogus).is_none());
    }

    /// A same-size attach resize must still land on the requested size after
    /// its WINCH-forcing bounce, and a changed size must apply directly.
    #[test]
    #[cfg(unix)]
    fn resize_forcing_winch_always_lands_on_the_requested_size() {
        use portable_pty::{native_pty_system, PtySize};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // Unchanged size: the reattach case. The bounce must be invisible in
        // the final state.
        resize_pty_forcing_winch(pair.master.as_ref(), 80, 24);
        let size = pair.master.get_size().expect("get_size");
        assert_eq!((size.cols, size.rows), (80, 24));

        // Changed size: the ordinary case.
        resize_pty_forcing_winch(pair.master.as_ref(), 132, 50);
        let size = pair.master.get_size().expect("get_size");
        assert_eq!((size.cols, size.rows), (132, 50));
    }

    #[test]
    fn extract_apple_started_at_from_float() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"startedDate": 1715000000.5}"#).unwrap();
        let dt = extract_apple_started_at(&row);
        assert_eq!(dt.timestamp(), 1715000000);
    }
}
