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

use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::config::env::Env;
use crate::data::config::flags::FlagConfig;
use crate::data::config::global::GlobalConfig;
use crate::data::config::image_source::ImageSourceSpec;
use crate::data::config::repo::RepoConfig;
use crate::data::config::runtime_selection::RuntimeSelection;
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

pub use background::{AgentExec, ExecOutput};
pub use capabilities::{Capabilities, DindSupport, ImageAcquisition};
pub use execution::{
    AgentExecution, AgentExitInfo, AgentHandle, AgentHandlePreview, AgentInstance, AgentStats,
    CancelHandle, StuckEvent,
};
pub use frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
pub use output_tail::{OutputTail, DEFAULT_OUTPUT_TAIL_LINES};

/// Common option carrier between Layer 2 and the runtime tier. Layer 2
/// constructs whichever variant matches the runtime paradigm it's targeting
/// (branching on `AgentRuntimeEngine::capabilities()`).
#[derive(Debug, Clone)]
pub enum ResolvedAgentOptions {
    Container(ResolvedContainerOptions),
    Sandbox(ResolvedSandboxOptions),
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
        Ok(ResolvedAgentOptions::Container(resolved))
    }

    /// Resolve a sandbox option list into the `Sandbox` variant. The sandbox
    /// option bag never conflicts (last-writer-wins on `ingest`), so this is
    /// infallible — the `Result` signature mirrors `container()` for symmetry.
    pub fn sandbox(
        options: impl IntoIterator<Item = crate::engine::sandbox::options::SandboxOption>,
    ) -> Self {
        ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::resolve(options))
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

    /// Stop a running agent. Semantics vary per runtime:
    ///   - container: stop + rm
    ///   - sandbox:   stop (preserve persistent volume)
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Argv (after the host CLI name) for an exec/re-attach against an
    /// existing agent through [`Self::host_cli`]. `None` when the runtime has no
    /// host CLI (builtin); callers must handle `None` and use [`Self::attach`].
    fn exec_args(
        &self,
        agent_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Option<Vec<String>>;

    /// Attach to an already-running agent this process did not start.
    ///
    /// Returns a `Box<dyn AgentInstance>` so the result flows into the same
    /// `run_with_frontend(Box<dyn AgentFrontend>) -> AgentExecution` path a
    /// freshly-built instance uses — `CliFrontend` and `TuiContainerProxy`
    /// both drive an attach session unchanged. The instance opens an
    /// `exec`/`sbx exec` session (argv from `exec_args`); its execution never
    /// stops the target on grace-expiry, because this process does not own it.
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError>;

    /// Enumerate running agents whose container/sandbox name starts with
    /// `prefix`. Every tier can honour this: Docker `ps --filter name=`,
    /// Apple's client-side prefix predicate, sandbox `sbx ls` name filter.
    /// The name prefix is the one identity channel all three tiers share.
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError>;

    /// The host CLI this runtime drives (`"docker"`, `"container"`, `"sbx"`),
    /// or `None` for a runtime that drives no host CLI (builtin). Every
    /// runtime states its own, so adding one cannot forget to.
    fn host_cli(&self) -> Option<&'static str>;

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
        tag: &str,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
        no_cache: bool,
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

    /// Content identity (digests, platform, source) of a cached image, when
    /// the runtime records one. `Ok(None)`: no identity is recorded, which
    /// is always the case for build runtimes.
    fn image_identity(&self, _tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        Ok(None)
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
}

/// Every value `GlobalConfig::runtime` accepts, in `RuntimeSelection::ALL`
/// order. Quoted in the fatal invalid-runtime error so the user can see what
/// to fix the config to.
pub const VALID_RUNTIMES: &[&str] = &[
    RuntimeSelection::Docker.as_str(),
    RuntimeSelection::AppleContainers.as_str(),
    RuntimeSelection::DockerSbxExperimental.as_str(),
    RuntimeSelection::Builtin.as_str(),
];

/// Factory: pick the right runtime based on `GlobalConfig::runtime`.
///
/// Reads the builtin runtime's settings from `global_config` and the process
/// environment only; a caller holding repo config uses [`detect_effective`].
pub fn detect(global_config: &GlobalConfig) -> Result<DetectedRuntime, EngineError> {
    detect_effective(&EffectiveConfig::new(
        FlagConfig::default(),
        Env::from_process(),
        RepoConfig::default(),
        global_config.clone(),
    ))
}

/// Factory: pick the right runtime based on the effective config.
///
/// - unset / blank / `"docker"` → `ContainerRuntime` with the Docker backend
/// - `"apple-containers"` → `ContainerRuntime` with the Apple backend
///   (macOS only)
/// - `"docker-sbx-experimental"` → `SandboxRuntime` with the (stubbed)
///   Docker Sandbox backend (macOS arm64 / Windows only; see WI 0090)
/// - `"builtin"` → `ContainerRuntime` with the builtin microVM backend, or
///   the precise reason it cannot run here (`BuiltinRuntimeUnavailable`,
///   `AmbientRuntimeOverride`). Never a fall back to another runtime.
/// - anything else → `EngineError::UnknownRuntime`. A misspelled runtime is
///   a fatal configuration error, never a silent fall back to Docker: the
///   user asked for an isolation model awman can't identify, and launching
///   agents under a different one than they configured is unsafe.
pub fn detect_effective(config: &EffectiveConfig) -> Result<DetectedRuntime, EngineError> {
    let selection = config
        .runtime_selection()
        .map_err(|unknown| EngineError::UnknownRuntime {
            value: unknown.0,
            valid: RuntimeSelection::valid_values(),
        })?;
    match selection {
        RuntimeSelection::Docker => Ok(DetectedRuntime::Container(Arc::new(
            ContainerRuntime::docker(),
        ))),
        RuntimeSelection::AppleContainers => {
            if cfg!(target_os = "macos") {
                Ok(DetectedRuntime::Container(Arc::new(
                    ContainerRuntime::apple(),
                )))
            } else {
                Err(EngineError::BackendUnsupportedOnPlatform {
                    backend: "apple-containers".into(),
                    platform: std::env::consts::OS.into(),
                })
            }
        }
        RuntimeSelection::DockerSbxExperimental => {
            Ok(DetectedRuntime::Sandbox(Arc::new(SandboxRuntime::dsbx()?)))
        }
        RuntimeSelection::Builtin => {
            let settings = BuiltinRuntimeSettings::resolve(config)?;
            Ok(DetectedRuntime::Container(Arc::new(
                ContainerRuntime::builtin(settings)?,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{ContainerOption, ImageRef};

    fn docker_cfg() -> GlobalConfig {
        GlobalConfig {
            runtime: Some("docker".into()),
            ..Default::default()
        }
    }

    // ─── Detection ────────────────────────────────────────────────────────────

    #[test]
    fn detect_none_runtime_picks_docker() {
        let cfg = GlobalConfig::default();
        let rt = detect(&cfg).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
        assert!(rt.container_runtime().is_some());
        assert!(rt.sandbox_runtime().is_none());
    }

    #[test]
    fn detect_explicit_docker_string_picks_docker() {
        let rt = detect(&docker_cfg()).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
        assert!(rt.container_runtime().is_some());
    }

    #[test]
    fn detect_empty_runtime_string_falls_back_to_docker() {
        let cfg = GlobalConfig {
            runtime: Some("  ".into()),
            ..Default::default()
        };
        let rt = detect(&cfg).unwrap();
        assert_eq!(rt.engine().runtime_name(), "docker");
    }

    #[test]
    fn detect_apple_on_non_mac_errors() {
        let cfg = GlobalConfig {
            runtime: Some("apple-containers".into()),
            ..Default::default()
        };
        let res = detect(&cfg);
        if cfg!(target_os = "macos") {
            assert!(res.is_ok());
            assert_eq!(res.unwrap().engine().runtime_name(), "apple-containers");
        } else {
            match res {
                Err(EngineError::BackendUnsupportedOnPlatform { .. }) => {}
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
                Ok(_) => panic!("expected error on non-macOS"),
            }
        }
    }

    #[test]
    fn detect_dsbx_routes_to_sandbox_or_errors_on_unsupported_platform() {
        let cfg = GlobalConfig {
            runtime: Some("docker-sbx-experimental".into()),
            ..Default::default()
        };
        let res = detect(&cfg);
        // dsbx is only supported on macOS arm64 and Windows.
        if cfg!(target_os = "linux") || cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            match res {
                Err(EngineError::BackendUnsupportedOnPlatform { .. }) => {}
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
                Ok(_) => panic!("expected platform error for dsbx on this OS/arch"),
            }
        } else {
            let rt = res.expect("dsbx should succeed on this platform");
            assert_eq!(rt.engine().runtime_name(), "docker-sbx-experimental");
            assert!(rt.sandbox_runtime().is_some());
            assert!(rt.container_runtime().is_none());
        }
    }

    #[test]
    fn detect_unknown_runtime_is_a_fatal_error() {
        let cfg = GlobalConfig {
            runtime: Some("blarg".into()),
            ..Default::default()
        };
        // Unknown runtime must error — never silently fall back to Docker.
        match detect(&cfg) {
            Err(EngineError::UnknownRuntime { value, valid }) => {
                assert_eq!(value, "blarg");
                for name in VALID_RUNTIMES {
                    assert!(
                        valid.contains(name),
                        "error must list valid runtime '{name}'; got: {valid}"
                    );
                }
            }
            Err(e) => panic!("expected UnknownRuntime error, got {e:?}"),
            Ok(_) => panic!("expected UnknownRuntime error, got Ok"),
        }
    }

    #[test]
    fn unknown_runtime_error_message_names_value_and_valid_set() {
        let cfg = GlobalConfig {
            runtime: Some("dokcer".into()),
            ..Default::default()
        };
        let msg = match detect(&cfg) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected UnknownRuntime error, got Ok"),
        };
        assert!(
            msg.contains("'dokcer'"),
            "message must quote the bad value: {msg}"
        );
        assert!(
            msg.contains("docker"),
            "message must list valid values: {msg}"
        );
        assert!(msg.contains("apple-containers"), "{msg}");
        assert!(msg.contains("docker-sbx-experimental"), "{msg}");
    }

    #[test]
    fn valid_runtimes_follow_runtime_selection() {
        let from_selection: Vec<&str> = RuntimeSelection::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(VALID_RUNTIMES, from_selection.as_slice());
        assert!(VALID_RUNTIMES.contains(&"builtin"));
    }

    /// Selecting `builtin` yields the builtin runtime or the precise reason it
    /// cannot run — never Docker, never a panic.
    #[test]
    fn detect_builtin_never_falls_back_to_another_runtime() {
        let cfg = GlobalConfig {
            runtime: Some("builtin".into()),
            ..Default::default()
        };
        match detect(&cfg) {
            Ok(rt) => assert_eq!(rt.engine().runtime_name(), "builtin"),
            Err(EngineError::BuiltinRuntimeUnavailable { reason }) => {
                assert!(!reason.is_empty())
            }
            Err(EngineError::AmbientRuntimeOverride { variable }) => {
                assert!(variable.starts_with("MSB_"))
            }
            Err(e) => panic!("expected the builtin runtime or its unavailability, got: {e:?}"),
        }
    }

    #[test]
    fn build_runtimes_do_not_import_images() {
        use crate::data::config::image_source::ImageSourceSpec;
        use crate::data::message::{UserMessage, UserMessageSink};
        struct Sink;
        impl UserMessageSink for Sink {
            fn write_message(&mut self, _msg: UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        let rt = detect(&docker_cfg()).unwrap().engine();
        let request = ImageImportRequest {
            tag: "awman-x-claude:latest".into(),
            source: ImageSourceSpec::Archive {
                path: "/nonexistent.tar".into(),
            },
            platform: OciPlatform::host_linux(),
            refresh: false,
        };
        match rt.import_image(&request, &mut Sink) {
            Err(EngineError::UnsupportedOnRuntime { runtime, operation }) => {
                assert_eq!(runtime, "docker");
                assert_eq!(operation, "image import");
            }
            other => panic!("expected UnsupportedOnRuntime, got {other:?}"),
        }
        assert!(rt
            .image_identity("awman-x-claude:latest")
            .unwrap()
            .is_none());
        assert_eq!(rt.host_cli(), Some("docker"));
        assert!(rt.reattach_after_owner_exit());
        assert_eq!(rt.capabilities().image_acquisition, ImageAcquisition::Build);
    }

    // ─── Runtime switching (host-side, no live sbx needed) ───────────────────
    //
    // Mutate GlobalConfig::runtime between values and re-detect. Each detection
    // must return a runtime whose runtime_name() matches the config — no state
    // leaks between calls.

    #[test]
    fn default_runtime_resolves_to_docker() {
        let cfg = GlobalConfig::default();
        assert!(
            cfg.runtime.is_none(),
            "default GlobalConfig must have no runtime set"
        );
        let rt = detect(&cfg).unwrap();
        assert_eq!(
            rt.engine().runtime_name(),
            "docker",
            "default config must resolve to Docker"
        );
        assert!(rt.container_runtime().is_some());
        assert!(rt.sandbox_runtime().is_none());
    }

    #[test]
    fn runtime_switching_docker_to_sandbox_and_back() {
        let docker_cfg = GlobalConfig {
            runtime: Some("docker".into()),
            ..Default::default()
        };
        let sbx_cfg = GlobalConfig {
            runtime: Some("docker-sbx-experimental".into()),
            ..Default::default()
        };
        let docker_again = GlobalConfig {
            runtime: Some("docker".into()),
            ..Default::default()
        };

        // Step 1: docker → ContainerRuntime
        let rt1 = detect(&docker_cfg).unwrap();
        assert_eq!(rt1.engine().runtime_name(), "docker");
        assert!(rt1.container_runtime().is_some());
        assert!(rt1.sandbox_runtime().is_none());

        // Step 2: sbx → SandboxRuntime (or BackendUnsupportedOnPlatform on Linux/x86)
        let rt2 = detect(&sbx_cfg);
        if cfg!(target_os = "linux") || cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            assert!(matches!(
                rt2,
                Err(EngineError::BackendUnsupportedOnPlatform { .. })
            ));
        } else {
            let rt2 = rt2.unwrap();
            assert_eq!(rt2.engine().runtime_name(), "docker-sbx-experimental");
            assert!(rt2.sandbox_runtime().is_some());
            assert!(rt2.container_runtime().is_none());
        }

        // Step 3: back to docker — no state leak from sbx detection
        let rt3 = detect(&docker_again).unwrap();
        assert_eq!(rt3.engine().runtime_name(), "docker");
        assert!(rt3.container_runtime().is_some());
        assert!(rt3.sandbox_runtime().is_none());
    }

    #[test]
    fn unknown_runtime_string_never_selects_a_runtime() {
        // "blarg" must not accidentally select sbx or any other runtime —
        // and must not fall back to Docker either.
        let cfg = GlobalConfig {
            runtime: Some("blarg".into()),
            ..Default::default()
        };
        assert!(matches!(
            detect(&cfg),
            Err(EngineError::UnknownRuntime { .. })
        ));
    }

    // ─── Option-variant mismatch via ContainerRuntime ─────────────────────────

    #[test]
    fn container_engine_rejects_sandbox_options() {
        use crate::engine::sandbox::options::ResolvedSandboxOptions;
        let rt = detect(&docker_cfg()).unwrap();
        let engine = rt.engine();
        let opts = ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions::default());
        match engine.build(opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker");
                assert_eq!(got, "sandbox");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ─── Docker integration (env-gated) ───────────────────────────────────────

    /// End-to-end test: runs a real Docker container through
    /// `Box<dyn AgentRuntimeEngine>` and asserts the exit code matches the
    /// pre-refactor path.
    ///
    /// Gate: set `AWMAN_DOCKER_INTEGRATION=1` and ensure `docker` is on PATH
    /// and the daemon is reachable. Skipped silently otherwise.
    #[tokio::test]
    async fn docker_integration_runs_container_through_trait() {
        // Skip if the gate env var is absent.
        if std::env::var("AWMAN_DOCKER_INTEGRATION").as_deref() != Ok("1") {
            return;
        }

        use crate::data::message::{UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use crate::engine::container::ContainerRuntime;

        // Minimal no-op frontend for test purposes.
        struct NullFrontend;
        impl UserMessageSink for NullFrontend {
            fn write_message(&mut self, _msg: UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        #[async_trait::async_trait]
        impl AgentFrontend for NullFrontend {
            fn report_status(&mut self, _: AgentStatus) {}
            fn report_progress(&mut self, _: AgentProgress) {}
            fn take_io(&mut self) -> AgentIo {
                let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
                let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
                let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
                AgentIo {
                    stdout,
                    stderr,
                    stdin_tx,
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }
            }
        }

        // Build through the trait surface (Box<dyn AgentRuntimeEngine>).
        let engine: Arc<dyn AgentRuntimeEngine> = Arc::new(ContainerRuntime::docker());
        let opts = ResolvedAgentOptions::container([
            ContainerOption::Image(ImageRef::new("busybox:latest")),
            ContainerOption::Entrypoint(crate::engine::container::options::Entrypoint::new([
                "true",
            ])),
        ])
        .expect("resolve container options");

        let instance = engine.build(opts).expect("build agent instance");
        let mut execution = instance
            .run_with_frontend(Box::new(NullFrontend))
            .expect("run_with_frontend");

        let exit_info = execution.wait().await.expect("wait for container");
        assert_eq!(exit_info.exit_code, 0, "expected clean exit from `true`");
    }
}
