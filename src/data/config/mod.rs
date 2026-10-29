//! Configuration concerns for awman: per-repo config, global config, env-var
//! reads, typed flag values, and the merged effective view.

pub mod agent_definition;
pub mod builtin_launch;
pub mod builtin_network;
pub mod builtin_runtime;
pub mod config_json;
pub mod effective;
pub mod env;
pub mod fields;
pub mod flags;
pub mod global;
pub mod image_source;
mod json_order;
pub mod leader_spec;
pub mod migrations;
pub mod overlays;
pub mod repo;
pub mod resolved_builtin;
pub mod runtime_selection;

pub use builtin_network::{
    BuiltinNetworkConfig, BuiltinNetworkSettings, NetworkAllowEntry, NetworkMode,
};
pub use builtin_runtime::BuiltinRuntimeConfig;
pub use effective::{
    DynamicWorkflowsSource, EffectiveConfig, GuidanceCounts, GuidanceEntry,
    ResolvedDynamicWorkflows, Sourced,
};
pub use env::{Env, EnvSnapshot};
pub use fields::{resolve_field_name, ConfigRowShape, FieldAlias, ResolvedFieldName};
pub use flags::FlagConfig;
pub use global::{GlobalConfig, LaunchModeFallback};
pub use image_source::{
    DockerTlsConfig, ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
};
pub use migrations::{
    ConfigMigration, ConfigMigrationChange, ConfigMigrationScope, GlobalConfigMigration,
    RepoConfigMigration, CONFIG_MIGRATIONS,
};
pub use overlays::{
    parse_overlay_list, parse_overlay_spec, CollectedOverlays, ContextOverlaySpec, ContextScope,
    DirectorySpec, OverlayPermission, SkillSpec, TypedOverlay,
};
pub use repo::{
    AgentAuthMode, ApiConfig, AuthRefreshConfig, DynamicWorkflowsConfig, DynamicWorkflowsKey,
    LaunchMode, MergeFixConfig, OnDirtyMergeTarget, OnMergeFailure, RemoteConfig, RepoConfig,
    SquadConfig, WorkItemsConfig, WorktreeConfig, REPO_CONFIG_FILENAME, REPO_CONFIG_SUBDIR,
};
pub use resolved_builtin::{BuiltinProcessConfig, ResolvedBuiltinConfig};
pub use runtime_selection::{RuntimeSelection, UnknownRuntimeValue};

/// Built-in default number of scrollback lines for the container terminal emulator.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Built-in default seconds of inactivity before the agent is considered stuck.
pub const DEFAULT_AGENT_STUCK_TIMEOUT_SECS: u64 = 30;
pub mod workflows;
