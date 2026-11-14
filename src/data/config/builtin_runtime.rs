//! `BuiltinRuntimeConfig` — the `builtin` block of global and repo config.
//!
//! Only read when `runtime` is `builtin`. Every field is optional: global and
//! repo blocks merge per field (repo wins, map entries merge per key). Layer 0
//! applies defaults and validation when resolving `EffectiveConfig`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::data::config::builtin_network::BuiltinNetworkConfig;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};
use crate::data::config::units::Size;

/// Whole vCPUs per agent VM when none are configured.
pub const DEFAULT_VCPUS: u8 = 2;

/// Guest memory per agent VM, in MiB, when none is configured.
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

/// Smallest guest memory, in MiB, the runtime accepts. The same floor applies
/// to config values and to per-launch memory limits.
pub const MIN_MEMORY_MIB: u32 = 128;

/// Writable root disk per agent VM, in MiB, when none is configured: the space
/// the guest has for everything outside its mounts (`/`, `$HOME`, installed
/// packages). Generous because the disk file is sparse: it only takes host
/// space as the guest writes. The runtime's own default is 4 GiB.
pub const DEFAULT_ROOT_DISK_MIB: u32 = 30 * 1024;

/// Smallest writable root disk, in MiB, the runtime accepts.
pub const MIN_ROOT_DISK_MIB: u32 = 256;

/// How much, in MiB, the guest may add to each host mount (the workspace
/// included) beyond what it already held, when none is configured. Bounds how
/// far an agent can fill the host disk through a mount; generous so build
/// output in the workspace fits. The runtime's own default is 4 GiB.
pub const DEFAULT_MOUNT_QUOTA_MIB: u32 = 30 * 1024;

/// Subdirectory of the awman data home that holds the builtin runtime's
/// private state when `stateDir` is not configured.
pub const DEFAULT_STATE_SUBDIR: &str = "builtin";

/// Settings for the builtin microVM runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuiltinRuntimeConfig {
    /// Private state root. Default: `<data home>/builtin` (i.e.
    /// `~/.awman/builtin`). Keep it short: control sockets live beneath it and
    /// a Unix socket path is limited to ~104 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    /// Whole vCPUs per agent VM. Default [`DEFAULT_VCPUS`]. Fractional
    /// requests are rejected, never rounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,
    /// Guest memory (`"4GiB"`). Default [`DEFAULT_MEMORY_MIB`] MiB, at least
    /// [`MIN_MEMORY_MIB`] MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Size>,
    /// Writable root disk (`"30GiB"`). Default [`DEFAULT_ROOT_DISK_MIB`] MiB, at
    /// least [`MIN_ROOT_DISK_MIB`] MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_disk: Option<Size>,
    /// Guest-write budget per host mount (`"30GiB"`). Default
    /// [`DEFAULT_MOUNT_QUOTA_MIB`] MiB, at least 1MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_quota: Option<Size>,
    /// Where `awman ready` acquires images that are not cached yet. No
    /// default: its absence is an error with guidance, never an implicit
    /// registry or daemon lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_source: Option<ImageSourceSpec>,
    /// Per-image overrides keyed by awman image tag
    /// (`awman-<stem>-<agent>:latest`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, ImageSourceSpec>,
    /// Registry host settings keyed by `host[:port]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub registries: BTreeMap<String, RegistryHostConfig>,
    /// Guest network policy. Global and repo blocks do not merge per field
    /// like the rest of this struct: see
    /// [`crate::data::config::builtin_network`] and
    /// `EffectiveConfig::builtin_network`, which the engine enforces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<BuiltinNetworkConfig>,
    /// Opt-in source-matched local builder. Merges per leaf (see
    /// [`BuiltinBuilderConfig::merged_over`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_builder"
    )]
    pub builder: Option<BuiltinBuilderConfig>,
}

/// Dotted name of the builder switch, as config errors and `awman config`
/// name it.
pub const BUILDER_ENABLED_FIELD: &str = "runtime.builtin.builder.enabled";

/// The `builtin.builder` block: whether `awman ready` may build a missing or
/// explicitly rebuilt image into the store its configured source exports
/// from, before importing it. Off unless set to `true`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuiltinBuilderConfig {
    /// `builtin.builder.enabled`. Only a JSON boolean is accepted; omission
    /// inherits, and the effective default is `false`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_builder_enabled"
    )]
    pub enabled: Option<bool>,
}

impl BuiltinBuilderConfig {
    /// `self` layered over `base`, leaf by leaf: an omitted `enabled`
    /// inherits `base`'s, an explicit value (including `false`) wins.
    pub fn merged_over(&self, base: &BuiltinBuilderConfig) -> BuiltinBuilderConfig {
        BuiltinBuilderConfig {
            enabled: self.enabled.or(base.enabled),
        }
    }

    /// Configured `enabled`, or `false`.
    pub fn enabled_or_default(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Parse a `builtin.builder.enabled` value typed by a user: exactly
    /// `true` or `false` after trimming surrounding whitespace. No other
    /// truthy spellings.
    pub fn parse_enabled(value: &str) -> Result<bool, String> {
        match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(format!(
                "{BUILDER_ENABLED_FIELD} must be true or false, not '{other}'"
            )),
        }
    }
}

/// A present `enabled` must be a JSON boolean: `null`, strings, numbers and
/// containers are rejected with an error naming the field.
fn deserialize_builder_enabled<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Bool(enabled) => Ok(Some(enabled)),
        other => Err(D::Error::custom(format!(
            "{BUILDER_ENABLED_FIELD} must be true or false, not {other}"
        ))),
    }
}

/// A present `builder` must be an object. Errors inside it carry its path,
/// so a raw config load names the offending key.
fn deserialize_builder<'de, D>(deserializer: D) -> Result<Option<BuiltinBuilderConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(D::Error::custom(format!(
            "runtime.builtin.builder must be an object holding {BUILDER_ENABLED_FIELD}, not {value}"
        )));
    }
    serde_json::from_value(value).map_err(|e| {
        let message = e.to_string();
        if message.contains(BUILDER_ENABLED_FIELD) {
            D::Error::custom(message)
        } else {
            D::Error::custom(format!("runtime.builtin.builder: {message}"))
        }
    })
}

/// Whole MiB of a configured size, saturating at `u32::MAX`.
fn size_mib(size: &Size) -> u32 {
    u32::try_from(size.mib()).unwrap_or(u32::MAX)
}

impl BuiltinRuntimeConfig {
    /// `self` layered over `base`: every scalar `self` sets wins, and map
    /// entries merge per key with `self`'s entry winning.
    ///
    /// `network` is left out (`None`): a repo network block may only narrow
    /// the global one, which a field-wise merge cannot express, so the only
    /// enforced network policy is `EffectiveConfig::builtin_network`, which
    /// resolves both layers. A merged view never carries an unenforced value.
    pub fn merged_over(&self, base: &BuiltinRuntimeConfig) -> BuiltinRuntimeConfig {
        let mut images = base.images.clone();
        images.extend(self.images.clone());
        let mut registries = base.registries.clone();
        registries.extend(self.registries.clone());
        BuiltinRuntimeConfig {
            state_dir: self.state_dir.clone().or_else(|| base.state_dir.clone()),
            vcpus: self.vcpus.or(base.vcpus),
            memory: self.memory.clone().or_else(|| base.memory.clone()),
            root_disk: self.root_disk.clone().or_else(|| base.root_disk.clone()),
            mount_quota: self
                .mount_quota
                .clone()
                .or_else(|| base.mount_quota.clone()),
            image_source: self
                .image_source
                .clone()
                .or_else(|| base.image_source.clone()),
            images,
            registries,
            network: None,
            builder: match (&self.builder, &base.builder) {
                (Some(repo), Some(global)) => Some(repo.merged_over(global)),
                (repo, global) => repo.clone().or_else(|| global.clone()),
            },
        }
    }

    /// Whether the source-matched local builder is enabled: `false` unless a
    /// layer set `builtin.builder.enabled` to `true` and no closer layer set
    /// it to `false`.
    pub fn builder_enabled(&self) -> bool {
        self.builder
            .as_ref()
            .is_some_and(BuiltinBuilderConfig::enabled_or_default)
    }

    /// Configured vCPUs, or [`DEFAULT_VCPUS`].
    pub fn vcpus_or_default(&self) -> u8 {
        self.vcpus.unwrap_or(DEFAULT_VCPUS)
    }

    /// Configured memory, or [`DEFAULT_MEMORY_MIB`].
    pub fn memory_mib_or_default(&self) -> u32 {
        self.memory.as_ref().map_or(DEFAULT_MEMORY_MIB, size_mib)
    }

    /// Configured root disk, or [`DEFAULT_ROOT_DISK_MIB`].
    pub fn root_disk_mib_or_default(&self) -> u32 {
        self.root_disk
            .as_ref()
            .map_or(DEFAULT_ROOT_DISK_MIB, size_mib)
    }

    /// Configured per-mount quota, or [`DEFAULT_MOUNT_QUOTA_MIB`].
    pub fn mount_quota_mib_or_default(&self) -> u32 {
        self.mount_quota
            .as_ref()
            .map_or(DEFAULT_MOUNT_QUOTA_MIB, size_mib)
    }

    /// Reject values that cannot be honoured: zero vCPUs, memory below
    /// [`MIN_MEMORY_MIB`], a root disk below [`MIN_ROOT_DISK_MIB`], a zero
    /// mount quota, or an image source that fails
    /// [`ImageSourceSpec::validate`].
    ///
    /// The network block is not checked here: `EffectiveConfig::builtin_network`
    /// validates the global and the repo block each on its own before
    /// combining them, so every launch already rejects a contradictory block
    /// in either layer (and a merged view carries no network to check).
    pub fn validate(&self) -> Result<(), String> {
        self.validate_resources()?;
        self.validate_image_sources()
    }

    /// Reject zero vCPUs, memory below [`MIN_MEMORY_MIB`], a root disk below
    /// [`MIN_ROOT_DISK_MIB`] and a zero mount quota: the per-launch sizing
    /// checks.
    pub fn validate_resources(&self) -> Result<(), String> {
        if self.vcpus == Some(0) {
            return Err("runtime.builtin.vcpus must be at least 1".into());
        }
        if self
            .memory
            .as_ref()
            .is_some_and(|m| m.mib() < u64::from(MIN_MEMORY_MIB))
        {
            return Err(format!(
                "runtime.builtin.memory must be at least {MIN_MEMORY_MIB}MiB"
            ));
        }
        if self
            .root_disk
            .as_ref()
            .is_some_and(|m| m.mib() < u64::from(MIN_ROOT_DISK_MIB))
        {
            return Err(format!(
                "runtime.builtin.rootDisk must be at least {MIN_ROOT_DISK_MIB}MiB"
            ));
        }
        if self.mount_quota.as_ref().is_some_and(|m| m.mib() < 1) {
            return Err("runtime.builtin.mountQuota must be at least 1MiB".into());
        }
        Ok(())
    }

    /// Reject an image source that fails [`ImageSourceSpec::validate`].
    pub fn validate_image_sources(&self) -> Result<(), String> {
        if let Some(source) = &self.image_source {
            source
                .validate()
                .map_err(|e| format!("runtime.builtin.imageSource: {e}"))?;
        }
        for (tag, source) in &self.images {
            source
                .validate()
                .map_err(|e| format!("runtime.builtin.images[{tag}]: {e}"))?;
        }
        Ok(())
    }
}
