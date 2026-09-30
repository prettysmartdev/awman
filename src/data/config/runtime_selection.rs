//! `RuntimeSelection` — the closed set of agent runtimes `GlobalConfig::runtime`
//! may name.
//!
//! The persisted value stays a free string (`GlobalConfig::runtime`) so an
//! older or newer config file still loads; this type is the one place that
//! string is interpreted. An unset or blank value is Docker, exactly as it
//! was before the builtin runtime existed. An unknown value is an error, never
//! a silent fall back to Docker: launching agents under a different isolation
//! model than the user configured is unsafe.

use serde::{Deserialize, Serialize};

/// Which agent runtime the user selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeSelection {
    /// The local Docker daemon, driven through the `docker` CLI. The default.
    Docker,
    /// Apple Containers, driven through the `container` CLI (macOS only).
    AppleContainers,
    /// Docker Sandboxes, driven through the `sbx` CLI (experimental).
    DockerSbxExperimental,
    /// The microVM runtime embedded in the awman executable (experimental;
    /// released for macOS on Apple Silicon only).
    BuiltinExperimental,
}

/// A `runtime` value that names no known runtime. Carries the raw value so
/// the error can quote it back to the user.
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

    /// Interpret a persisted `runtime` value.
    ///
    /// `None`, empty or whitespace → `Docker` (the unchanged default).
    /// Surrounding whitespace is ignored. Anything else that is not one of
    /// [`RuntimeSelection::ALL`]'s names → `Err` carrying the trimmed value.
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

    /// The persisted spelling of this runtime.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::AppleContainers => "apple-containers",
            Self::DockerSbxExperimental => "docker-sbx-experimental",
            Self::BuiltinExperimental => "builtin-experimental",
        }
    }

    /// A pointer for a value that is close to a real runtime name, appended
    /// to the unknown-runtime error. `builtin` was never released under that
    /// name; the runtime is experimental and spelled `builtin-experimental`.
    pub fn hint_for(value: &str) -> Option<&'static str> {
        (value == "builtin").then_some(
            "the builtin runtime is experimental: set runtime to \"builtin-experimental\" \
             (macOS on Apple Silicon only)",
        )
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
    fn unset_or_blank_is_docker() {
        assert_eq!(RuntimeSelection::parse(None), Ok(RuntimeSelection::Docker));
        assert_eq!(
            RuntimeSelection::parse(Some("")),
            Ok(RuntimeSelection::Docker)
        );
        assert_eq!(
            RuntimeSelection::parse(Some("   ")),
            Ok(RuntimeSelection::Docker)
        );
    }

    #[test]
    fn every_name_round_trips() {
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
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            RuntimeSelection::parse(Some(" builtin-experimental ")),
            Ok(RuntimeSelection::BuiltinExperimental)
        );
    }

    #[test]
    fn unknown_value_is_an_error_not_docker() {
        assert_eq!(
            RuntimeSelection::parse(Some("dokcer")),
            Err(UnknownRuntimeValue("dokcer".into()))
        );
        // Case matters: the persisted spelling is exact.
        assert!(RuntimeSelection::parse(Some("Docker")).is_err());
    }

    #[test]
    fn valid_values_lists_every_runtime_in_order() {
        assert_eq!(
            RuntimeSelection::valid_values(),
            "docker, apple-containers, docker-sbx-experimental, builtin-experimental"
        );
    }

    #[test]
    fn the_unreleased_builtin_spelling_is_refused_with_a_hint() {
        assert_eq!(
            RuntimeSelection::parse(Some("builtin")),
            Err(UnknownRuntimeValue("builtin".into()))
        );
        assert!(RuntimeSelection::hint_for("builtin")
            .unwrap()
            .contains("builtin-experimental"));
        assert_eq!(RuntimeSelection::hint_for("dokcer"), None);
    }
}
