//! `BuiltinLaunchSettings` — the per-launch part of the builtin runtime's
//! settings.
//!
//! The builtin runtime has two scopes. One runtime per process owns the state
//! directory, the embedded runtime and the ambient-override check. Everything
//! here is resolved from one session's [`EffectiveConfig`] for every launch
//! instead, so a repo's narrowing applies wherever an agent for that repo
//! runs, even when many sessions share one runtime.
//!
//! [`EffectiveConfig`]: crate::data::config::effective::EffectiveConfig

use std::path::PathBuf;

use crate::data::config::builtin_network::BuiltinNetworkSettings;
use crate::data::config::builtin_runtime::{
    DEFAULT_MEMORY_MIB, DEFAULT_MOUNT_QUOTA_MIB, DEFAULT_ROOT_DISK_MIB, DEFAULT_VCPUS,
};

/// The builtin settings one launch is sized and confined by, already layered
/// (repo over global) and validated. Built by
/// `EffectiveConfig::builtin_launch_settings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinLaunchSettings {
    /// Default whole vCPUs for the VM; a launch's own CPU limit overrides it.
    pub vcpus: u8,
    /// Default guest memory in MiB; a launch's own memory limit overrides it.
    pub memory_mib: u32,
    /// Writable root disk in MiB.
    pub root_disk_mib: u32,
    /// Guest-write budget per host mount in MiB.
    pub mount_quota_mib: u32,
    /// Guest network policy: the global block narrowed by the repo block.
    pub network: BuiltinNetworkSettings,
    /// The state directory this session resolves to, present only when its
    /// repo config sets `builtin.stateDir`. The state directory is
    /// process-wide, so this is compared with the process's and never used to
    /// open a second one.
    pub repo_state_dir: Option<PathBuf>,
}

impl Default for BuiltinLaunchSettings {
    /// The settings of a session with no `builtin` block anywhere.
    fn default() -> Self {
        Self {
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            root_disk_mib: DEFAULT_ROOT_DISK_MIB,
            mount_quota_mib: DEFAULT_MOUNT_QUOTA_MIB,
            network: BuiltinNetworkSettings::default(),
            repo_state_dir: None,
        }
    }
}
