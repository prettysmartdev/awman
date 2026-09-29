//! Configuration concerns for awman: per-repo config, global config, env-var
//! reads, typed flag values, and the merged effective view.

pub mod builtin_network;
pub mod builtin_runtime;
pub mod config_json;
pub mod effective;
pub mod env;
pub mod fields;
pub mod flags;
pub mod global;
pub mod image_source;
pub mod leader_spec;
pub mod overlays;
pub mod repo;
pub mod runtime_selection;

pub use builtin_network::{
    BuiltinNetworkConfig, BuiltinNetworkSettings, NetworkAllowEntry, NetworkMode,
};
pub use builtin_runtime::BuiltinRuntimeConfig;
pub use effective::EffectiveConfig;
pub use env::{Env, EnvSnapshot};
pub use flags::FlagConfig;
pub use global::{GlobalConfig, LaunchModeFallback};
pub use image_source::{
    DockerTlsConfig, ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
};
pub use overlays::{
    parse_overlay_list, parse_overlay_spec, CollectedOverlays, ContextOverlaySpec, ContextScope,
    DirectorySpec, OverlayPermission, SkillSpec, TypedOverlay,
};
pub use repo::{
    AgentAuthMode, ApiConfig, AuthRefreshConfig, DynamicWorkflowsConfig, LaunchMode, RemoteConfig,
    RepoConfig, SquadConfig, WorkItemsConfig, REPO_CONFIG_FILENAME, REPO_CONFIG_SUBDIR,
};
pub use runtime_selection::{RuntimeSelection, UnknownRuntimeValue};

/// Built-in default number of scrollback lines for the container terminal emulator.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Built-in default seconds of inactivity before the agent is considered stuck.
pub const DEFAULT_AGENT_STUCK_TIMEOUT_SECS: u64 = 30;
