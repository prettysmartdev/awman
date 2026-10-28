//! Internal `ContainerBackend` trait — NOT pub outside `src/engine/container/`.
//!
//! Implementations: `docker::DockerBackend`, `apple::AppleBackend`, and the
//! builtin microVM backend under `builtin/`.
//!
//! Every operation is explicit: nothing here shells out by default. The two
//! CLI-driven backends share their argv-shaped implementations through
//! `host_cli_backend`, parameterised by their `ContainerCli`; a backend that
//! drives no host CLI implements each operation natively.

use std::collections::HashMap;
use std::time::Duration;

use crate::data::message::UserMessageSink;
use crate::data::oci_identity::ImageIdentity;
use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::background::ExecOutput;
use crate::engine::agent_runtime::execution::{
    default_detail, AgentDetail, AgentInstance, AgentStats,
};
use crate::engine::agent_runtime::{Capabilities, ImageImportRequest, ImportedImage};
use crate::engine::container::background::BackgroundLaunchRequest;
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::container::runtime::ContainerImageInfo;
use crate::engine::error::EngineError;

/// A bounded, mount-free command against an already available image.
pub(super) struct ProbeRequest<'a> {
    pub tools: &'a [&'a str],
    pub image: &'a str,
    pub script: &'a str,
    pub timeout: Duration,
}

/// What every container backend must support. The concrete type is hidden
/// behind `Arc<dyn ContainerBackend>` and never escapes this module.
pub(super) trait ContainerBackend: Send + Sync {
    // ─── Identity ───────────────────────────────────────────────────────────

    /// Static name used by `ContainerRuntime::runtime_name`.
    fn name(&self) -> &'static str;

    /// User-facing display name for this backend (e.g. `"Docker"`,
    /// `"Apple Containers"`). Surfaced by `ContainerRuntime::display_name`.
    fn display_name(&self) -> &'static str;

    /// What this backend can do. Docker and Apple share
    /// `runtime::CONTAINER_CAPABILITIES`; the builtin backend has its own.
    fn capabilities(&self) -> &'static Capabilities;

    /// Execute a no-mount image probe; importing or building is never part of this call.
    fn run_probe(&self, _request: &ProbeRequest<'_>) -> Result<String, EngineError> {
        Err(EngineError::NotImplemented("no-mount probe"))
    }

    /// Stable image content identity when available.
    fn image_id(&self, _tag: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }

    /// Whether `attach` still reaches an agent after the launching process has
    /// exited. See `AgentRuntimeEngine::reattach_after_owner_exit`.
    fn reattach_after_owner_exit(&self) -> bool;

    /// `Ok` when the runtime is reachable; the error carries the precise
    /// reason it is not (CLI missing, daemon down, hypervisor unavailable, …).
    fn is_available(&self) -> Result<(), EngineError>;

    /// One-line notices from opening this backend (e.g. an in-place builtin
    /// catalog upgrade), drained once. Layer 2 presents them: stderr in
    /// command mode, the status area in the TUI.
    fn take_notices(&self) -> Vec<String> {
        Vec::new()
    }

    // ─── Agents ─────────────────────────────────────────────────────────────

    /// Build an `AgentInstance` from resolved options. The image is NOT
    /// pulled or built here — that's a separate concern handled by
    /// higher-level engines (e.g. `AgentEngine::ensure_available`).
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError>;

    /// Attach to an already-running agent this process did not start. The
    /// returned instance's execution never stops or removes the agent on
    /// grace-expiry — it belongs to another process.
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError>;

    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError>;

    /// List all running awman agents without requiring a session.
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError>;

    /// List running awman agents whose name starts with `prefix`.
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError>;

    /// List stopped (exited/dead) awman agents eligible for cleanup. Running
    /// and paused agents are never returned.
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError>;

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;

    /// One agent in depth. The default is built from the handle and
    /// `stats()`; backends that record more (builtin labels, docker inspect)
    /// override it.
    fn inspect(&self, handle: &AgentHandle) -> Result<AgentDetail, EngineError> {
        let running = self.list_running_all()?;
        let current = running.iter().find(|h| h.id == handle.id);
        let stopped = if current.is_none() {
            self.list_stopped()?
        } else {
            Vec::new()
        };
        let current = current
            .or_else(|| stopped.iter().find(|h| h.id == handle.id))
            .ok_or_else(|| EngineError::AgentNotFound {
                id: handle.id.clone(),
            })?;
        let reports = self.capabilities().reports_resource_stats;
        let is_running = running.iter().any(|h| h.id == handle.id);
        let stats = if reports && is_running {
            self.stats(current)
        } else {
            Err(EngineError::Other("resource metrics unavailable".into()))
        };
        let mut detail = default_detail(reports, current, stats);
        detail.running = is_running;
        Ok(detail)
    }

    /// Explicitly stop the resolved agent; propagate failures. Removal is separate.
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Remove a stopped agent by id or name. Errors when the runtime refuses
    /// (e.g. it transitioned back to running), so `awman clean` can count
    /// per-item failures.
    fn remove_agent(&self, id: &str) -> Result<(), EngineError>;

    // ─── Image store ────────────────────────────────────────────────────────

    /// Whether `tag` exists in the backend's local image store.
    fn image_exists(&self, tag: &str) -> Result<bool, EngineError>;

    /// Read the image's effective `$HOME` from its baked-in config. Used by
    /// `AgentEngine::build_options` to mount agent settings overlays at the
    /// path the running agent's user actually reads — which can diverge from
    /// the on-disk `Dockerfile.<agent>` after a Dockerfile change that hasn't
    /// been followed by an image rebuild. `Ok(None)` when the image is
    /// missing, the runtime is unreachable, or the config has no `HOME`.
    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError>;

    /// Content identity of a cached image, when the backend records one.
    /// Build backends record none.
    fn image_identity(&self, _tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        Ok(None)
    }

    fn image_matches_source(&self, _request: &ImageImportRequest) -> Result<bool, EngineError> {
        Ok(false)
    }

    /// Build `tag` from `dockerfile` in `context`, streaming output to
    /// `on_line`. Backends that cannot build answer `UnsupportedOnRuntime`.
    fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError>;

    /// Import an already-built image from an explicit source. Backends that
    /// build images answer `UnsupportedOnRuntime`.
    fn import_image(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError>;

    /// Import with a caller-owned cancellation signal. Build-only backends keep
    /// their existing unsupported result; importing backends override this.
    fn import_image_cancellable(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<ImportedImage, EngineError> {
        cancel.check()?;
        self.import_image(request, sink)
    }

    /// List dangling awman images eligible for cleanup.
    fn list_dangling_images(&self) -> Result<Vec<ContainerImageInfo>, EngineError>;

    /// Remove an image by id. Errors when the runtime refuses (e.g. the image
    /// is still referenced).
    fn remove_image(&self, id: &str) -> Result<(), EngineError>;

    // ─── Background lifecycle (setup/teardown) ──────────────────────────────

    /// Start an idle agent for setup/teardown `exec` calls; returns its id.
    /// The builtin backend sizes and confines it with
    /// `request.builtin_launch` and refuses when it is absent; other backends
    /// ignore it.
    fn start_background(
        &self,
        request: &BackgroundLaunchRequest<'_>,
    ) -> Result<String, EngineError>;

    fn exec_in_background(
        &self,
        id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError>;

    fn exec_in_background_streaming(
        &self,
        request: &crate::engine::container::background::BackgroundExecRequest<'_>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError>;

    /// Stop and remove a background agent. Best-effort.
    fn stop_and_remove(&self, id: &str) -> Result<(), EngineError>;
}
