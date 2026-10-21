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
    /// Guest memory in MiB. Default [`DEFAULT_MEMORY_MIB`], at least
    /// [`MIN_MEMORY_MIB`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    /// Writable root disk in MiB. Default [`DEFAULT_ROOT_DISK_MIB`], at least
    /// [`MIN_ROOT_DISK_MIB`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_disk_mib: Option<u32>,
    /// Guest-write budget per host mount in MiB. Default
    /// [`DEFAULT_MOUNT_QUOTA_MIB`], at least 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_quota_mib: Option<u32>,
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
pub const BUILDER_ENABLED_FIELD: &str = "builtin.builder.enabled";

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
            "builtin.builder must be an object holding {BUILDER_ENABLED_FIELD}, not {value}"
        )));
    }
    serde_json::from_value(value).map_err(|e| {
        let message = e.to_string();
        if message.contains(BUILDER_ENABLED_FIELD) {
            D::Error::custom(message)
        } else {
            D::Error::custom(format!("builtin.builder: {message}"))
        }
    })
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
            memory_mib: self.memory_mib.or(base.memory_mib),
            root_disk_mib: self.root_disk_mib.or(base.root_disk_mib),
            mount_quota_mib: self.mount_quota_mib.or(base.mount_quota_mib),
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
        self.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB)
    }

    /// Configured root disk, or [`DEFAULT_ROOT_DISK_MIB`].
    pub fn root_disk_mib_or_default(&self) -> u32 {
        self.root_disk_mib.unwrap_or(DEFAULT_ROOT_DISK_MIB)
    }

    /// Configured per-mount quota, or [`DEFAULT_MOUNT_QUOTA_MIB`].
    pub fn mount_quota_mib_or_default(&self) -> u32 {
        self.mount_quota_mib.unwrap_or(DEFAULT_MOUNT_QUOTA_MIB)
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
            return Err("builtin.vcpus must be at least 1".into());
        }
        if self.memory_mib.is_some_and(|m| m < MIN_MEMORY_MIB) {
            return Err(format!(
                "builtin.memoryMib must be at least {MIN_MEMORY_MIB}"
            ));
        }
        if self.root_disk_mib.is_some_and(|m| m < MIN_ROOT_DISK_MIB) {
            return Err(format!(
                "builtin.rootDiskMib must be at least {MIN_ROOT_DISK_MIB}"
            ));
        }
        if self.mount_quota_mib == Some(0) {
            return Err("builtin.mountQuotaMib must be at least 1".into());
        }
        Ok(())
    }

    /// Reject an image source that fails [`ImageSourceSpec::validate`].
    pub fn validate_image_sources(&self) -> Result<(), String> {
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
        assert!(cfg
            .images
            .get("awman-x:latest")
            .or(cfg.image_source.as_ref())
            .is_none());
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
            merged.images.get("a").or(merged.image_source.as_ref()),
            Some(&ImageSourceSpec::Archive {
                path: "/repo-a".into()
            })
        );
        assert_eq!(
            merged.images.get("b").or(merged.image_source.as_ref()),
            Some(&ImageSourceSpec::Archive {
                path: "/global-b".into()
            })
        );
        assert_eq!(
            merged.images.get("c").or(merged.image_source.as_ref()),
            global.image_source.as_ref()
        );
    }

    #[test]
    fn merged_view_never_carries_a_network_block() {
        let global: BuiltinRuntimeConfig =
            serde_json::from_str(r#"{"network":{"mode":"allowlist","allow":["a.example"]}}"#)
                .unwrap();
        let repo: BuiltinRuntimeConfig =
            serde_json::from_str(r#"{"network":{"mode":"none"}}"#).unwrap();
        assert_eq!(repo.merged_over(&global).network, None);
        assert_eq!(
            BuiltinRuntimeConfig::default().merged_over(&global).network,
            None
        );
        // The persisted blocks themselves keep their network (config show,
        // config files and `EffectiveConfig::builtin_network` read them).
        assert!(global.network.is_some() && repo.network.is_some());
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
            root_disk_mib: Some(MIN_ROOT_DISK_MIB - 1),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(BuiltinRuntimeConfig {
            mount_quota_mib: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(BuiltinRuntimeConfig {
            root_disk_mib: Some(MIN_ROOT_DISK_MIB),
            mount_quota_mib: Some(1),
            ..Default::default()
        }
        .validate()
        .is_ok());
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

    #[test]
    fn builder_defaults_false_and_accepts_both_boolean_values() {
        assert!(!BuiltinRuntimeConfig::default().builder_enabled());
        for enabled in [true, false] {
            let raw = format!(r#"{{"builder":{{"enabled":{enabled}}}}}"#);
            let parsed: BuiltinRuntimeConfig = serde_json::from_str(&raw).unwrap();
            assert_eq!(parsed.builder_enabled(), enabled);
            assert_eq!(parsed.builder.unwrap().enabled, Some(enabled));
        }
        assert!(
            !serde_json::from_str::<BuiltinRuntimeConfig>(r#"{"builder":{}}"#)
                .unwrap()
                .builder_enabled()
        );
    }

    #[test]
    fn builder_accepts_only_present_json_booleans_and_names_the_field() {
        for value in ["null", "\"true\"", "1", "[]", "{}"] {
            let raw = format!(r#"{{"builder":{{"enabled":{value}}}}}"#);
            let error = serde_json::from_str::<BuiltinRuntimeConfig>(&raw)
                .unwrap_err()
                .to_string();
            assert!(error.contains(BUILDER_ENABLED_FIELD), "{error}");
        }
    }

    #[test]
    fn builder_serializes_only_as_a_nested_json_object() {
        let cfg: BuiltinRuntimeConfig = serde_json::from_str(
            r#"{"builder":{"enabled":true},"imageSource":{"type":"archive","path":"/tmp/a.tar"}}"#,
        )
        .unwrap();
        let value = serde_json::to_value(cfg).unwrap();
        assert_eq!(value["builder"]["enabled"], true);
        assert!(value.get("builtin.builder.enabled").is_none());
        assert!(value["imageSource"].is_object());
    }

    #[test]
    fn omitted_builder_and_omitted_enabled_are_distinct_but_default_off() {
        for raw in ["{}", r#"{"builder":{}}"#] {
            let cfg: BuiltinRuntimeConfig = serde_json::from_str(raw).unwrap();
            assert!(!cfg.builder_enabled());
        }
        let cfg: BuiltinRuntimeConfig =
            serde_json::from_str(r#"{"builder":{"enabled":true}}"#).unwrap();
        assert!(cfg.builder_enabled());
    }

    #[test]
    fn builder_merges_per_leaf_and_explicit_repo_false_disables_global_true() {
        let global = BuiltinRuntimeConfig {
            builder: Some(BuiltinBuilderConfig {
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let omitted = BuiltinRuntimeConfig {
            builder: Some(BuiltinBuilderConfig::default()),
            ..Default::default()
        };
        assert!(omitted.merged_over(&global).builder_enabled());
        let disabled = BuiltinRuntimeConfig {
            builder: Some(BuiltinBuilderConfig {
                enabled: Some(false),
            }),
            ..Default::default()
        };
        assert!(!disabled.merged_over(&global).builder_enabled());
        assert!(global
            .merged_over(&BuiltinRuntimeConfig::default())
            .builder_enabled());
    }

    #[test]
    fn builder_enabled_cli_value_parser_is_strict_and_trimmed() {
        assert_eq!(BuiltinBuilderConfig::parse_enabled(" true "), Ok(true));
        assert_eq!(BuiltinBuilderConfig::parse_enabled("false"), Ok(false));
        for bad in ["yes", "1", "TRUE", "", "null"] {
            assert!(BuiltinBuilderConfig::parse_enabled(bad)
                .unwrap_err()
                .contains(BUILDER_ENABLED_FIELD));
        }
    }
}
