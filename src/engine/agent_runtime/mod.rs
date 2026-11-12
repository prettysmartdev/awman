//! `engine::agent_runtime` — the Layer 1 `AgentRuntimeEngine` trait family.
//!
//! Abstracts over the two paradigms of agent-isolation runtime awman
//! supports: **container-class** (`ContainerRuntime` — Docker, Apple
//! Containers) and **sandbox-class** (`SandboxRuntime` — microVM-per-session
//! runtimes such as Docker Sandboxes).
//!
//! Layer 2 sees only `Arc<dyn AgentRuntimeEngine>` and this module's types.
//! Paradigm-specific operations (image builds, background containers) stay
//! as inherent methods on the concrete runtimes and are reached through the
//! typed handles `agent_runtime::detect()` hands back.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::config::env::Env;
use crate::data::config::file::ConfigFile;
use crate::data::config::flags::FlagConfig;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};
use crate::data::config::runtime::RuntimeSelection;
use crate::data::oci_identity::{ImageIdentity, OciPlatform};
use crate::data::session::Session;
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::container::runtime::BuiltinRuntimeSettings;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::sandbox::options::ResolvedSandboxOptions;
use crate::engine::sandbox::SandboxRuntime;

pub mod background;
pub mod capabilities;

pub mod execution;
pub mod frontend;
pub mod output_tail;
pub mod program_status;

pub use background::{AgentExec, ExecOutput};
pub use capabilities::{Capabilities, ImageAcquisition};
pub use execution::{
    AgentDetail, AgentExecution, AgentExitInfo, AgentHandle, AgentHandlePreview, AgentInstance,
    AgentOwner, AgentStats, CancelHandle, StuckEvent,
};
pub use frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
pub use output_tail::{OutputTail, DEFAULT_OUTPUT_TAIL_LINES};

/// Common option carrier between Layer 2 and the runtime tier. Layer 2
/// constructs whichever variant matches the runtime paradigm it's targeting
/// (branching on `AgentRuntimeEngine::capabilities()`).
#[derive(Debug, Clone)]
pub enum ResolvedAgentOptions {
    Container(Box<ResolvedContainerOptions>),
    Sandbox(Box<ResolvedSandboxOptions>),
}

impl ResolvedAgentOptions {
    /// Name of the carried paradigm — used in `OptionVariantMismatch` errors.
    pub fn paradigm(&self) -> &'static str {
        match self {
            ResolvedAgentOptions::Container(_) => "container",
            ResolvedAgentOptions::Sandbox(_) => "sandbox",
        }
    }

    /// Resolve a container option list into the `Container` variant.
    /// Conflicting options surface as `EngineError::ConflictingOptions`,
    /// exactly as the pre-refactor `ContainerRuntime::build` reported them.
    pub fn container(
        options: impl IntoIterator<Item = crate::engine::container::options::ContainerOption>,
    ) -> Result<Self, EngineError> {
        let resolved = ResolvedContainerOptions::resolve(options)?;
        Ok(ResolvedAgentOptions::Container(Box::new(resolved)))
    }

    /// Resolve a sandbox option list into the `Sandbox` variant. The sandbox
    /// option bag never conflicts (last-writer-wins on `ingest`), so this is
    /// infallible.
    pub fn sandbox(
        options: impl IntoIterator<Item = crate::engine::sandbox::options::SandboxOption>,
    ) -> Self {
        ResolvedAgentOptions::Sandbox(Box::new(ResolvedSandboxOptions::resolve(options)))
    }
}

/// What `AgentRuntimeEngine::ready_agent` should do for one agent.
///
/// Both tiers honour `no_cache`; the container tier also honours `build`
/// (force a rebuild even when the image is present). A sandbox runtime has
/// no image to force, so `build` is inert there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadyAgentOptions {
    /// Rebuild/re-emit from scratch, ignoring any cache.
    pub no_cache: bool,
    /// Container tier: rebuild the agent image even if it already exists.
    pub build: bool,
}

/// What every agent runtime must support. The cross-paradigm trait surface
/// Layer 2 programs against; paradigm-specific decisions branch on
/// `capabilities()` or `runtime_name()`, never on concrete types.
pub trait AgentRuntimeEngine: Send + Sync {
    /// The daemon's shared terminal registry, carried through runtime wrappers.
    fn terminal_hub(&self) -> Option<Arc<crate::engine::terminal_hub::TerminalHub>> {
        None
    }
    /// Optional ownership prefix for auxiliary containers of this launch.
    fn container_name_prefix(&self) -> Option<&str> {
        None
    }

    /// Stable machine name for this runtime (e.g. "docker", "apple-containers",
    /// "docker-sbx-experimental"). Used for log lines and config round-trips.
    fn runtime_name(&self) -> &'static str;

    /// User-facing display name (e.g. "Docker", "Apple Containers",
    /// "Docker Sandboxes (experimental)").
    fn display_name(&self) -> &'static str;

    /// Static description of what this runtime can do. Layer 2 reads this
    /// to decide how to map cross-paradigm options before calling build().
    fn capabilities(&self) -> &Capabilities;

    /// Probe whether the underlying tooling is reachable. Times out on its own.
    fn is_available(&self) -> bool;

    /// Why the runtime is unreachable (`Ok` when reachable). Runtimes with a
    /// detailed probe override this; the default reports only the boolean.
    fn availability(&self) -> Result<(), EngineError> {
        if self.is_available() {
            Ok(())
        } else {
            Err(EngineError::Other(format!(
                "{} is not available",
                self.display_name()
            )))
        }
    }

    /// Construct a configured `AgentInstance` from typed options — the first
    /// half of the two-step build/run pattern (no spawn happens here). The
    /// runtime rejects options whose paradigm doesn't fit with
    /// `EngineError::OptionVariantMismatch`.
    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError>;

    /// Enumerate handles for running agents created by this runtime.
    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError>;

    /// Same as list_running but session-less, for stats polling loops.
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError>;

    /// Per-handle resource stats. Returns zeros when the runtime can't
    /// provide per-resource metrics (sandbox-class runtimes today).
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;

    /// Stats for a running agent known only by name.
    ///
    /// Callers that hold a name but no `AgentHandle` — a view tracking a
    /// container the engine named for it — used to fabricate a handle with an
    /// empty image tag and `Utc::now()` for the start time, purely to satisfy
    /// the signature (WI 0114 F-16). The default does exactly that, once, in
    /// the layer that owns the handle type; a runtime whose stats call takes
    /// a name natively can override it.
    fn stats_by_name(&self, name: &str) -> Result<AgentStats, EngineError> {
        self.stats(&AgentHandle {
            id: name.to_string(),
            name: name.to_string(),
            image_tag: String::new(),
            started_at: chrono::Utc::now(),
        })
    }

    /// Explicitly stop a running agent, reporting runtime failures.
    /// Call `remove_agent` afterwards to request cleanup of its resources.
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Attach to an already-running agent this process did not start.
    ///
    /// Returns a `Box<dyn AgentInstance>` so the result flows into the same
    /// `run_with_frontend(Box<dyn AgentFrontend>) -> AgentExecution` path a
    /// freshly-built instance uses — `CliFrontend` and `TuiContainerProxy`
    /// both drive an attach session unchanged. The instance opens an
    /// attach session through the runtime's own channel; its execution never
    /// stops the target on grace-expiry, because this process does not own it.
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError>;

    /// Enumerate running agents whose container/sandbox name starts with
    /// `prefix`. Every tier can honour this: Docker `ps --filter name=`,
    /// Apple's client-side prefix predicate, sandbox `sbx ls` name filter.
    /// The name prefix is the one identity channel all three tiers share.
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError>;

    /// Stopped agents still present in the runtime (`awman agents ls -a`).
    /// Default: none. `ContainerRuntime` forwards to its backend (Apple's
    /// answers empty by design); sandbox-class runtimes keep the default.
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }

    /// One agent in depth. The default is built from the handle and
    /// `stats()` (skipped when `reports_resource_stats` is false; a stats
    /// error becomes `None`), so every runtime works without an override.
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
        let mut detail = execution::default_detail(reports, current, stats);
        detail.running = is_running;
        Ok(detail)
    }

    /// Whether [`Self::attach`] still reaches an agent after the process that
    /// launched it has exited. Docker: yes (`docker attach`). Apple: no (the
    /// launcher hosts the attach socket). Default: no.
    fn reattach_after_owner_exit(&self) -> bool {
        false
    }

    /// Remove a stopped agent by id or name (`awman clean`). Returns an error
    /// when the runtime refuses, so callers can count per-item failures.
    fn remove_agent(&self, _id: &str) -> Result<(), EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.runtime_name(),
            operation: "agent removal",
        })
    }

    // ─── Agent environment preparation (F-40b) ───────────────────────────
    //
    // `ready`, `init` and the agent engine program against these instead of
    // holding a typed `Arc<ContainerRuntime>`. The image-store methods are
    // container-paradigm operations: a kit-declarative runtime answers
    // `EngineError::UnsupportedOnRuntime`, which is a typed "this runtime
    // cannot" rather than a panic or a silent `false`.

    /// Make `agent`'s environment ready to launch: an image build on the
    /// container tier, a kit emit + validate on the sandbox tier. The
    /// paradigm branch lives here, not in the caller.
    fn ready_agent(
        &self,
        agent: &str,
        opts: ReadyAgentOptions,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError>;

    /// Whether `tag` exists in this runtime's local image store.
    fn image_exists(&self, tag: &str) -> Result<bool, EngineError>;

    /// The `HOME` baked into `tag`'s image config, when the runtime can read
    /// it. `Ok(None)` means "image present, no `HOME` declared".
    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError>;

    /// Build `tag` from `dockerfile` in `context`, streaming build output to
    /// `on_line`.
    fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError>;

    /// Import an already-built image from an explicit source into this
    /// runtime's store. Only runtimes whose
    /// `capabilities().image_acquisition` is `Import` implement it.
    fn import_image(
        &self,
        _request: &ImageImportRequest,
        _sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.runtime_name(),
            operation: "image import",
        })
    }

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

    fn import_image_in(
        &self,
        request: &ImageImportRequest,
        _env: &crate::data::config::env::EnvSnapshot,
        sink: &mut dyn crate::data::message::UserMessageSink,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<ImportedImage, EngineError> {
        self.import_image_cancellable(request, sink, cancel)
    }

    /// Content identity (digests, platform, source) of a cached image, when
    /// the runtime records one. `Ok(None)`: no identity is recorded, which
    /// is always the case for build runtimes.
    fn image_identity(&self, _tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        Ok(None)
    }

    /// Verify installed provenance using local metadata only. Unknown
    /// provenance is refused; this operation must never acquire an image.
    fn image_matches_source(&self, _request: &ImageImportRequest) -> Result<bool, EngineError> {
        Ok(false)
    }
}

/// What [`AgentRuntimeEngine::import_image`] should import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageImportRequest {
    /// The awman tag the image is stored under (`awman-<stem>-<agent>:latest`).
    pub tag: String,
    /// The explicit source. Never inferred from `tag`.
    pub source: ImageSourceSpec,
    /// The platform the image must match.
    pub platform: OciPlatform,
    /// Re-acquire even when an identity for `tag` is already cached.
    pub refresh: bool,
    /// The exact content produced by a local build, checked before cache
    /// publication or installation. Requires `refresh`.
    pub expected_built_identity: Option<crate::data::oci_identity::BuiltImageIdentity>,
    /// Immutable selector for export from the same source store. Docker uses
    /// the build's config digest (image ID), while the cache key and recorded
    /// identity keep the configured logical reference.
    pub immutable_export_selector: Option<String>,
    /// Registry host settings (`builtin.registries`) from the importing
    /// session's config, used for the pull itself. The same session value
    /// resolved the source, so resolution and pull never disagree.
    pub registries: BTreeMap<String, RegistryHostConfig>,
}

/// The result of a successful [`AgentRuntimeEngine::import_image`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedImage {
    pub tag: String,
    pub identity: ImageIdentity,
    /// The image config's `HOME`, when it declares one.
    pub home_dir: Option<String>,
    /// The image config's `User`, when it declares one.
    pub user: Option<String>,
}

/// The concrete runtime `detect()` chose, exposing both the cross-paradigm
/// trait handle and the typed paradigm-specific handle. `Engines` populates
/// its `container_runtime` / `sandbox_runtime` fields from this — both
/// handles point at the same underlying object.
pub enum DetectedRuntime {
    Container(Arc<ContainerRuntime>),
    Sandbox(Arc<SandboxRuntime>),
}

impl DetectedRuntime {
    /// Cross-paradigm trait-object handle to the detected runtime.
    pub fn engine(&self) -> Arc<dyn AgentRuntimeEngine> {
        match self {
            DetectedRuntime::Container(rt) => rt.clone(),
            DetectedRuntime::Sandbox(rt) => rt.clone(),
        }
    }

    /// Typed handle, set when the detected runtime is container-class.
    pub fn container_runtime(&self) -> Option<Arc<ContainerRuntime>> {
        match self {
            DetectedRuntime::Container(rt) => Some(rt.clone()),
            DetectedRuntime::Sandbox(_) => None,
        }
    }

    /// Typed handle, set when the detected runtime is sandbox-class.
    pub fn sandbox_runtime(&self) -> Option<Arc<SandboxRuntime>> {
        match self {
            DetectedRuntime::Container(_) => None,
            DetectedRuntime::Sandbox(rt) => Some(rt.clone()),
        }
    }

    /// One-line notices from opening the runtime, each returned once. The
    /// caller presents them (stderr in command mode, the TUI status area).
    pub fn take_notices(&self) -> Vec<String> {
        match self {
            DetectedRuntime::Container(rt) => rt.take_notices(),
            DetectedRuntime::Sandbox(_) => Vec::new(),
        }
    }
}

/// Every value `runtime.kind` accepts, in `RuntimeSelection::ALL`
/// order. Quoted in the fatal invalid-runtime error so the user can see what
/// to fix the config to.
pub const VALID_RUNTIMES: &[&str] = &[
    RuntimeSelection::Docker.as_str(),
    RuntimeSelection::AppleContainers.as_str(),
    RuntimeSelection::DockerSbxExperimental.as_str(),
    RuntimeSelection::BuiltinExperimental.as_str(),
];

/// Factory: pick the right runtime from the global config file's `runtime.kind`.
///
/// Reads the builtin runtime's settings from `global` and the process
/// environment only; a caller holding repo config uses [`detect_effective`].
pub fn detect(global: &ConfigFile) -> Result<DetectedRuntime, EngineError> {
    let config = EffectiveConfig::from_files(
        global.clone(),
        ConfigFile::default(),
        FlagConfig::default(),
        Env::from_process(),
    )?;
    detect_effective(&config)
}

/// Factory: pick the right runtime based on the effective config.
///
/// - unset / blank / `"docker"` → `ContainerRuntime` with the Docker backend
/// - `"apple-containers"` → `ContainerRuntime` with the Apple backend
///   (macOS only)
/// - `"docker-sbx-experimental"` → `SandboxRuntime` with the (stubbed)
///   Docker Sandbox backend (macOS arm64 / Windows only; see WI 0090)
/// - `"builtin-experimental"` → `ContainerRuntime` with the builtin microVM
///   backend (released for macOS on Apple Silicon only), or
///   the precise reason it cannot run here (`BuiltinRuntimeUnavailable`,
///   `AmbientRuntimeOverride`). Never a fall back to another runtime.
///   Only the process-wide builtin settings (`BuiltinRuntimeSettings`) come
///   from `config`; each launch brings its own session's
///   `BuiltinLaunchSettings`.
/// - anything else → `EngineError::UnknownRuntime`. A misspelled runtime is
///   a fatal configuration error, never a silent fall back to Docker: the
///   user asked for an isolation model awman can't identify, and launching
///   agents under a different one than they configured is unsafe.
pub fn detect_effective(config: &EffectiveConfig) -> Result<DetectedRuntime, EngineError> {
    let selection = config
        .runtime_selection()
        .map_err(|unknown| EngineError::UnknownRuntime {
            valid: match RuntimeSelection::hint_for(&unknown.0) {
                Some(hint) => format!("{} ({hint})", RuntimeSelection::valid_values()),
                None => RuntimeSelection::valid_values(),
            },
            value: unknown.0,
        })?;
    match selection {
        RuntimeSelection::Docker => Ok(DetectedRuntime::Container(
            ContainerRuntime::docker_handle(config.env().clone()),
        )),
        RuntimeSelection::AppleContainers => {
            if cfg!(target_os = "macos") {
                Ok(DetectedRuntime::Container(ContainerRuntime::apple_handle(
                    config.env().clone(),
                )))
            } else {
                Err(EngineError::BackendUnsupportedOnPlatform {
                    backend: "apple-containers".into(),
                    platform: std::env::consts::OS.into(),
                })
            }
        }
        RuntimeSelection::DockerSbxExperimental => Ok(DetectedRuntime::Sandbox(Arc::new(
            SandboxRuntime::dsbx_in(config.env().clone())?,
        ))),
        RuntimeSelection::BuiltinExperimental => {
            // Process-wide settings only: a repo whose per-launch `builtin`
            // settings are invalid (e.g. a widening network block) fails its
            // own launches, not runtime detection for the whole process.
            let resolved = config.builtin_process_settings()?;
            let settings = BuiltinRuntimeSettings::resolve(&resolved);
            Ok(DetectedRuntime::Container(Arc::new(
                ContainerRuntime::builtin(settings)?,
            )))
        }
    }
}
