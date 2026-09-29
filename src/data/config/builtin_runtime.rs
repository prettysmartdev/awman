//! `BuiltinRuntimeConfig` — the `builtin` block of global and repo config.
//!
//! Only read when `runtime` is `builtin`. Every field is optional: global and
//! repo blocks merge per field (repo wins, map entries merge per key), and the
//! engine applies the defaults below to whatever is still unset.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::data::config::builtin_network::BuiltinNetworkConfig;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};

/// Whole vCPUs per agent VM when none are configured.
pub const DEFAULT_VCPUS: u8 = 2;

/// Guest memory per agent VM, in MiB, when none is configured.
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

/// Smallest guest memory, in MiB, the runtime accepts. The same floor applies
/// to config values and to per-launch memory limits.
pub const MIN_MEMORY_MIB: u32 = 128;

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
    /// Guest memory in MiB. Default [`DEFAULT_MEMORY_MIB`], at least
    /// [`MIN_MEMORY_MIB`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
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
}

impl BuiltinRuntimeConfig {
    /// `self` layered over `base`: every scalar `self` sets wins, and map
    /// entries merge per key with `self`'s entry winning.
    pub fn merged_over(&self, base: &BuiltinRuntimeConfig) -> BuiltinRuntimeConfig {
        let mut images = base.images.clone();
        images.extend(self.images.clone());
        let mut registries = base.registries.clone();
        registries.extend(self.registries.clone());
        BuiltinRuntimeConfig {
            state_dir: self.state_dir.clone().or_else(|| base.state_dir.clone()),
            vcpus: self.vcpus.or(base.vcpus),
            memory_mib: self.memory_mib.or(base.memory_mib),
            image_source: self
                .image_source
                .clone()
                .or_else(|| base.image_source.clone()),
            images,
            registries,
            // Informational only; enforcement resolves both layers.
            network: self.network.clone().or_else(|| base.network.clone()),
        }
    }

    /// Configured vCPUs, or [`DEFAULT_VCPUS`].
    pub fn vcpus_or_default(&self) -> u8 {
        self.vcpus.unwrap_or(DEFAULT_VCPUS)
    }

    /// Configured memory, or [`DEFAULT_MEMORY_MIB`].
    pub fn memory_mib_or_default(&self) -> u32 {
        self.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB)
    }

    /// The source for `awman_tag`: its per-image override, else the default
    /// `image_source`. `None` means nothing is configured.
    pub fn image_source_for(&self, awman_tag: &str) -> Option<&ImageSourceSpec> {
        self.images.get(awman_tag).or(self.image_source.as_ref())
    }

    /// Reject values that cannot be honoured: zero vCPUs, memory below
    /// [`MIN_MEMORY_MIB`], an image source that fails
    /// [`ImageSourceSpec::validate`], or a contradictory network block.
    pub fn validate(&self) -> Result<(), String> {
        if self.vcpus == Some(0) {
            return Err("builtin.vcpus must be at least 1".into());
        }
        if self.memory_mib.is_some_and(|m| m < MIN_MEMORY_MIB) {
            return Err(format!(
                "builtin.memoryMib must be at least {MIN_MEMORY_MIB}"
            ));
        }
        if let Some(network) = &self.network {
            network
                .validate()
                .map_err(|e| format!("builtin.network: {e}"))?;
        }
        if let Some(source) = &self.image_source {
            source
                .validate()
                .map_err(|e| format!("builtin.imageSource: {e}"))?;
        }
        for (tag, source) in &self.images {
            source
                .validate()
                .map_err(|e| format!("builtin.images[{tag}]: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_block_serializes_to_empty_object() {
        let json = serde_json::to_string(&BuiltinRuntimeConfig::default()).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn camel_case_fields_parse_and_unknown_fields_are_rejected() {
        let cfg: BuiltinRuntimeConfig = serde_json::from_str(
            r#"{"stateDir":"/s","vcpus":4,"memoryMib":2048,
                "imageSource":{"type":"registry","registry":"localhost:5000"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/s")));
        assert_eq!(cfg.vcpus_or_default(), 4);
        assert_eq!(cfg.memory_mib_or_default(), 2048);
        assert!(serde_json::from_str::<BuiltinRuntimeConfig>(r#"{"cpus":1.5}"#).is_err());
        // Fractional vCPUs are not representable.
        assert!(serde_json::from_str::<BuiltinRuntimeConfig>(r#"{"vcpus":1.5}"#).is_err());
    }

    #[test]
    fn defaults_apply_when_unset() {
        let cfg = BuiltinRuntimeConfig::default();
        assert_eq!(cfg.vcpus_or_default(), DEFAULT_VCPUS);
        assert_eq!(cfg.memory_mib_or_default(), DEFAULT_MEMORY_MIB);
        assert!(cfg.image_source_for("awman-x:latest").is_none());
    }

    #[test]
    fn repo_merges_over_global_per_field_and_per_key() {
        let global = BuiltinRuntimeConfig {
            vcpus: Some(2),
            memory_mib: Some(8192),
            image_source: Some(ImageSourceSpec::Registry {
                registry: Some("global".into()),
                reference: None,
            }),
            images: BTreeMap::from([
                (
                    "a".to_string(),
                    ImageSourceSpec::Archive {
                        path: "/global-a".into(),
                    },
                ),
                (
                    "b".to_string(),
                    ImageSourceSpec::Archive {
                        path: "/global-b".into(),
                    },
                ),
            ]),
            ..Default::default()
        };
        let repo = BuiltinRuntimeConfig {
            vcpus: Some(6),
            images: BTreeMap::from([(
                "a".to_string(),
                ImageSourceSpec::Archive {
                    path: "/repo-a".into(),
                },
            )]),
            ..Default::default()
        };
        let merged = repo.merged_over(&global);
        assert_eq!(merged.vcpus, Some(6));
        assert_eq!(merged.memory_mib, Some(8192));
        assert_eq!(merged.image_source, global.image_source);
        assert_eq!(
            merged.image_source_for("a"),
            Some(&ImageSourceSpec::Archive {
                path: "/repo-a".into()
            })
        );
        assert_eq!(
            merged.image_source_for("b"),
            Some(&ImageSourceSpec::Archive {
                path: "/global-b".into()
            })
        );
        assert_eq!(merged.image_source_for("c"), global.image_source.as_ref());
    }

    #[test]
    fn validate_rejects_zero_resources_and_bad_sources() {
        assert!(BuiltinRuntimeConfig {
            vcpus: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(BuiltinRuntimeConfig {
            memory_mib: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(BuiltinRuntimeConfig {
            images: BTreeMap::from([(
                "t".to_string(),
                ImageSourceSpec::Archive { path: "".into() }
            )]),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(BuiltinRuntimeConfig::default().validate().is_ok());
    }
}
