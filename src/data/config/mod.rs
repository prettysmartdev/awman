//! Configuration: one config tree ([`ConfigFile`]) read from the global,
//! repo and squad task files, the key table that defines it ([`fields`]),
//! env/`--set` overrides, typed flag values, and the merged
//! [`EffectiveConfig`].

pub mod agent_definition;
pub mod agents;
pub mod builtin_launch;
pub mod builtin_network;
pub mod builtin_runtime;
pub mod config_json;
pub mod daemon;
pub mod effective;
pub mod env;
pub mod fields;
pub mod file;
pub mod flags;
pub mod image_source;
pub mod integrations;
pub mod leader_spec;
pub mod overlays;
pub mod overrides;
pub mod remote;
pub mod resolved_builtin;
pub mod runtime;
pub mod squad;
pub mod state;
pub mod tui;
pub mod units;
pub mod work_items;
pub mod work_servers;
pub mod workflows;
pub mod worktree;

pub use agents::{
    AcpFallback, AgentAuthConfig, AgentAuthMode, AgentsConfig, AuthRefreshConfig, LaunchMode,
    YoloConfig,
};
pub use builtin_network::{
    BuiltinNetworkConfig, BuiltinNetworkSettings, NetworkAllowEntry, NetworkMode,
};
pub use builtin_runtime::BuiltinRuntimeConfig;
pub use daemon::{DaemonConfig, DaemonSettings, Interaction};
pub use effective::{
    AuthRefreshSettings, ConfigLayers, DynamicWorkflowsSource, EffectiveConfig, GuidanceCounts,
    GuidanceEntry, Layer, ResolvedDynamicWorkflows, Sourced,
};
pub use env::{Env, EnvSnapshot};
pub use fields::{ConfigKey, FileKind, KeyError, KeyScope, ValueKind, CONFIG_KEYS};
pub use file::{global_config_path, repo_config_path, task_config_path, ConfigFile};
pub use flags::FlagConfig;
pub use image_source::{
    DockerTlsConfig, ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
};
pub use overlays::{
    parse_overlay_list, parse_overlay_spec, CollectedOverlays, ContextOverlaySpec, ContextScope,
    DirectorySpec, OverlayPermission, SkillSpec, TypedOverlay,
};
pub use overrides::ConfigOverrides;
pub use remote::{RemoteConfig, RemoteTarget, ResolvedRemoteTarget};
pub use resolved_builtin::{BuiltinProcessConfig, ResolvedBuiltinConfig};
pub use runtime::{RuntimeConfig, RuntimeKind, RuntimeSelection, UnknownRuntimeValue};
pub use squad::SquadConfig;
pub use state::RepoState;
pub use tui::TuiConfig;
pub use work_items::WorkItemsConfig;
pub use work_servers::WorkServerConfig;
pub use workflows::{DynamicConfig, WorkflowsConfig};
pub use worktree::{MergeFixConfig, OnDirtyMergeTarget, OnMergeFailure, WorktreeConfig};

pub use crate::data::fs::data_paths::AWMAN_HOME;
