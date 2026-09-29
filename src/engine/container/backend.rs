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
use std::path::Path;

use crate::data::message::UserMessageSink;
use crate::data::oci_identity::ImageIdentity;
use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::background::ExecOutput;
use crate::engine::agent_runtime::execution::{AgentInstance, AgentStats};
use crate::engine::agent_runtime::{Capabilities, ImageImportRequest, ImportedImage};
use crate::engine::container::options::{OverlaySpec, ResolvedContainerOptions};
use crate::engine::container::runtime::ContainerImageInfo;
use crate::engine::error::EngineError;

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

    /// The host CLI this backend drives (`"docker"`, `"container"`), or `None`
    /// when it drives none. Every backend states its own (F-32), so adding a
    /// backend cannot forget to declare one.
    fn host_cli(&self) -> Option<&'static str>;

    /// Whether `attach` still reaches an agent after the launching process has
    /// exited. See `AgentRuntimeEngine::reattach_after_owner_exit`.
    fn reattach_after_owner_exit(&self) -> bool;

    /// `Ok` when the runtime is reachable; the error carries the precise
    /// reason it is not (CLI missing, daemon down, hypervisor unavailable, …).
    fn is_available(&self) -> Result<(), EngineError>;

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

    /// Stop and remove an agent this process owns.
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Remove a stopped agent by id or name. Errors when the runtime refuses
    /// (e.g. it transitioned back to running), so `awman clean` can count
    /// per-item failures.
    fn remove_agent(&self, id: &str) -> Result<(), EngineError>;

    /// Argv (after the host CLI name) for an interactive exec into a running
    /// agent, used by TUI re-attach. `None` when the backend has no host CLI.
    fn exec_args(
        &self,
        agent_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Option<Vec<String>>;

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

    /// Build `tag` from `dockerfile` in `context`, streaming output to
    /// `on_line`. Backends that cannot build answer `UnsupportedOnRuntime`.
    fn build_image(
        &self,
        tag: &str,
        dockerfile: &Path,
        context: &Path,
        no_cache: bool,
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
    fn start_background(
        &self,
        image: &str,
        workdir: &Path,
        env: &HashMap<String, String>,
        overlays: &[OverlaySpec],
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
        id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError>;

    /// Stop and remove a background agent. Best-effort.
    fn stop_and_remove(&self, id: &str) -> Result<(), EngineError>;
}
