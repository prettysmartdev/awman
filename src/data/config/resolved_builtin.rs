//! Fully resolved builtin-runtime configuration for the engine boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::data::config::builtin_launch::BuiltinLaunchSettings;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};

/// Builtin configuration after Layer 0 has merged layers, applied defaults,
/// validated restrictions and normalized source-kind aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBuiltinConfig {
    /// Process-wide private state root.
    pub state_dir: PathBuf,
    /// Ambient `MSB_*` overrides captured by the effective environment.
    pub ambient_overrides: Vec<&'static str>,
    /// Test isolation is active without an explicit opt-in to the real runtime.
    pub test_isolation: bool,
    /// Defaults and network policy for this session's launches.
    pub launch: BuiltinLaunchSettings,
    /// Default source and per-image overrides for acquisition.
    pub image_source: Option<ImageSourceSpec>,
    pub images: BTreeMap<String, ImageSourceSpec>,
    /// Registry settings carried through source resolution and acquisition.
    pub registries: BTreeMap<String, RegistryHostConfig>,
    /// Whether source-matched local image building is enabled for this session.
    pub builder_enabled: bool,
}

/// The process-wide part of the builtin configuration: what opening the one
/// builtin runtime needs, independent of any session's launches or images.
///
/// Resolving it never validates per-session settings (network narrowing,
/// vCPUs, memory, image sources), so a repo whose `builtin` block is invalid
/// fails that repo's launches only, never runtime detection for the whole
/// process (a TUI or API server started in that repo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinProcessConfig {
    /// Process-wide private state root.
    pub state_dir: PathBuf,
    /// Ambient `MSB_*` overrides captured by the effective environment.
    pub ambient_overrides: Vec<&'static str>,
    /// Test isolation is active without an explicit opt-in to the real runtime.
    pub test_isolation: bool,
}
