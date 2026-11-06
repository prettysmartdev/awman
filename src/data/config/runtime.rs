//! `runtime` section: which container runtime agents run in, and the images
//! they run.

use serde::{Deserialize, Serialize};

use crate::data::config::builtin_runtime::BuiltinRuntimeConfig;

/// The `runtime` block.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RuntimeSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<BuiltinRuntimeConfig>,
}

/// Which agent runtime the user selected (`runtime.kind`).
///
/// `sbx` and `builtin` are experimental; that maturity is stated in the docs
/// and in `awman config show`, never in the value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSelection {
    /// The local Docker daemon, driven through the `docker` CLI. The default.
    #[default]
    Docker,
    /// Apple Containers, driven through the `container` CLI (macOS only).
    #[serde(rename = "apple")]
    AppleContainers,
    /// Docker Sandboxes, driven through the `sbx` CLI (experimental).
    #[serde(rename = "sbx")]
    DockerSbxExperimental,
    /// The microVM runtime embedded in the awman executable (experimental;
    /// released for macOS on Apple Silicon only).
    #[serde(rename = "builtin")]
    BuiltinExperimental,
}

/// The new name of [`RuntimeSelection`] (`runtime.kind`).
pub type RuntimeKind = RuntimeSelection;

/// A `runtime.kind` value that names no known runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRuntimeValue(pub String);

impl RuntimeSelection {
    /// Every selectable runtime, in the order the valid values are listed.
    pub const ALL: &'static [RuntimeSelection] = &[
        Self::Docker,
        Self::AppleContainers,
        Self::DockerSbxExperimental,
        Self::BuiltinExperimental,
    ];

    /// Interpret a `runtime.kind` spelling. `None` or blank → `Docker`; an
    /// unknown value is an error, never a silent fall back.
    pub fn parse(raw: Option<&str>) -> Result<Self, UnknownRuntimeValue> {
        let Some(value) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self::Docker);
        };
        Self::ALL
            .iter()
            .copied()
            .find(|selection| selection.as_str() == value)
            .ok_or_else(|| UnknownRuntimeValue(value.to_string()))
    }

    /// The config spelling of this runtime.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::AppleContainers => "apple",
            Self::DockerSbxExperimental => "sbx",
            Self::BuiltinExperimental => "builtin",
        }
    }

    /// Whether the runtime is experimental (`sbx`, `builtin`).
    pub const fn is_experimental(self) -> bool {
        matches!(
            self,
            Self::DockerSbxExperimental | Self::BuiltinExperimental
        )
    }

    /// A pointer for a value that is close to a real runtime name. The
    /// pre-0139 spellings name their replacement.
    pub fn hint_for(value: &str) -> Option<&'static str> {
        match value {
            "apple-containers" => Some("runtime.kind is now \"apple\""),
            "docker-sbx-experimental" => Some("runtime.kind is now \"sbx\""),
            "builtin-experimental" => Some("runtime.kind is now \"builtin\""),
            _ => None,
        }
    }

    /// Every valid spelling, comma-separated, for error messages.
    pub fn valid_values() -> String {
        Self::ALL
            .iter()
            .map(|selection| selection.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl std::fmt::Display for RuntimeSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_round_trips_through_serde_and_parse() {
        for selection in RuntimeSelection::ALL {
            assert_eq!(
                RuntimeSelection::parse(Some(selection.as_str())),
                Ok(*selection)
            );
            let json = serde_json::to_string(selection).unwrap();
            assert_eq!(json, format!("\"{}\"", selection.as_str()));
            let back: RuntimeSelection = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *selection);
        }
    }

    #[test]
    fn valid_values_are_the_short_spellings() {
        assert_eq!(
            RuntimeSelection::valid_values(),
            "docker, apple, sbx, builtin"
        );
    }

    #[test]
    fn old_spellings_are_unknown_with_a_hint() {
        for old in [
            "apple-containers",
            "docker-sbx-experimental",
            "builtin-experimental",
        ] {
            assert!(RuntimeSelection::parse(Some(old)).is_err());
            assert!(serde_json::from_str::<RuntimeSelection>(&format!("\"{old}\"")).is_err());
            assert!(RuntimeSelection::hint_for(old).is_some());
        }
        assert_eq!(RuntimeSelection::parse(None), Ok(RuntimeSelection::Docker));
    }
}
