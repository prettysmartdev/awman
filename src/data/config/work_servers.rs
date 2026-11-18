//! `workServers.<name>`: work servers a pull worker claims jobs from.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::config::daemon::Interaction;
use crate::data::config::units::Size;
use crate::data::worker::lease_images::ImageBudget;

/// One `workServers.<name>` entry.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction: Option<Interaction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    /// `imageBudget`: how many lease-built images the worker keeps (WI 0163).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_budget: Option<ImageBudgetConfig>,
    /// Worker environment variables a lease may request with `env(NAME)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_env: Option<Vec<String>>,
    /// Non-default-library skills a lease may request with `skill(name)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_skills: Option<Vec<String>>,
}

/// `workServers.<name>.imageBudget`: `{maxImages?, maxBytes?}`.
///
/// An absent object, or one with neither field, gives the default
/// `{maxImages: 20}`. Otherwise each field limits its own axis and an absent
/// field leaves that axis unlimited. `maxBytes` is a size string
/// (`"20GiB"`), like every other size key.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageBudgetConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_images: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<Size>,
}

impl ImageBudgetConfig {
    pub fn resolved(&self) -> ImageBudget {
        if self.max_images.is_none() && self.max_bytes.is_none() {
            return ImageBudget::default();
        }
        ImageBudget {
            max_images: self.max_images,
            max_bytes: self.max_bytes.as_ref().map(Size::bytes),
        }
    }
}

/// `workServers.<name>.claim`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimMode {
    #[default]
    Auto,
    Manual,
}

/// A work-server entry with its defaults applied. Entries always have a URL
/// after `ConfigFile` validation; keep it optional here so callers that build
/// an `EffectiveConfig` directly still get a useful, non-panicking view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkServer {
    pub url: Option<String>,
    pub enabled: bool,
    pub claim: ClaimMode,
    pub max_concurrent_jobs: usize,
    pub interaction: Option<Interaction>,
    pub labels: BTreeMap<String, String>,
    pub image_budget: ImageBudget,
    pub allow_env: Vec<String>,
    pub allow_skills: Vec<String>,
}

impl WorkServerConfig {
    pub fn resolved(&self) -> ResolvedWorkServer {
        ResolvedWorkServer {
            url: self.url.clone(),
            enabled: self.enabled.unwrap_or(true),
            claim: self.claim.unwrap_or_default(),
            max_concurrent_jobs: self.max_concurrent_jobs.unwrap_or(1),
            interaction: self.interaction,
            labels: self.labels.clone().unwrap_or_default(),
            image_budget: self
                .image_budget
                .as_ref()
                .map(ImageBudgetConfig::resolved)
                .unwrap_or_default(),
            allow_env: self.allow_env.clone().unwrap_or_default(),
            allow_skills: self.allow_skills.clone().unwrap_or_default(),
        }
    }
}

/// Derive a valid config map name from a work-server URL's host.
///
/// The 0139 name rule allows lowercase ASCII letters, digits, `_` and `-`.
/// Host punctuation (including dots and IPv6 separators) becomes `-`.
pub fn normalize_work_server_name(url: &str) -> Result<String, String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("invalid server URL: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "server URL must include a host".to_string())?;
    let mut name = String::with_capacity(host.len());
    let mut separator = false;
    for byte in host.bytes() {
        if byte.is_ascii_alphanumeric() {
            if separator && !name.is_empty() {
                name.push('-');
            }
            name.push(byte.to_ascii_lowercase() as char);
            separator = false;
        } else if (byte == b'_' || byte == b'-') && !name.is_empty() {
            // A name starts with a letter or digit; leading ones are dropped.
            name.push(byte as char);
            separator = false;
        } else {
            separator = true;
        }
        if name.len() >= 63 {
            break;
        }
    }
    // A separator plus a character can step one past the cap.
    name.truncate(63);
    while name.ends_with('-') || name.ends_with('_') {
        name.pop();
    }
    if name.is_empty() || !crate::data::config::fields::is_valid_map_name(&name) {
        return Err(format!(
            "URL host {host:?} cannot be normalized to a valid work-server name"
        ));
    }
    Ok(name)
}

/// Owner-configured keys may be plain (`pool`) or namespaced
/// (`example.com/pool`); `awman/` belongs to the runtime.
pub fn validate_configured_label(key: &str) -> Result<(), String> {
    if key.starts_with("awman/") {
        return Err("uses the reserved awman/ namespace".into());
    }
    awman_wsp::labels::validate_key(key).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod image_budget_tests {
    use super::*;

    #[test]
    fn image_budget_defaults_to_twenty_images_and_reads_camel_case() {
        let old: WorkServerConfig = serde_json::from_str(r#"{"url":"https://h"}"#).unwrap();
        assert_eq!(old.resolved().image_budget, ImageBudget::default());
        let empty: WorkServerConfig =
            serde_json::from_str(r#"{"url":"https://h","imageBudget":{}}"#).unwrap();
        assert_eq!(empty.resolved().image_budget, ImageBudget::default());
        let set: WorkServerConfig = serde_json::from_str(
            r#"{"url":"https://h","imageBudget":{"maxImages":5,"maxBytes":"2GiB"}}"#,
        )
        .unwrap();
        assert_eq!(
            set.resolved().image_budget,
            ImageBudget {
                max_images: Some(5),
                max_bytes: Some(2 << 30)
            }
        );
        let bytes_only: WorkServerConfig =
            serde_json::from_str(r#"{"url":"https://h","imageBudget":{"maxBytes":"1MiB"}}"#)
                .unwrap();
        assert_eq!(bytes_only.resolved().image_budget.max_images, None);
        assert!(serde_json::from_str::<WorkServerConfig>(
            r#"{"url":"https://h","imageBudget":{"maxImage":5}}"#
        )
        .is_err());
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["imageBudget"]["maxBytes"], "2GiB");
    }
}

/// An `allowEnv` entry must be a valid environment-variable name
/// (`^[A-Z_][A-Z0-9_]*$`), the same rule as a command secret name.
pub fn validate_allow_env_entry(name: &str) -> Result<(), String> {
    crate::data::secret::SecretName::parse(name)
        .map(|_| ())
        .map_err(|_| "must match ^[A-Z_][A-Z0-9_]*$ (at most 128 bytes)".to_string())
}

/// An `allowSkills` entry must be a name `skill(name)` accepts: a skill or
/// `library/skill`, never `*`, a list, or an empty, `.` or `..` segment.
pub fn validate_allow_skill_entry(name: &str) -> Result<(), String> {
    use crate::data::config::overlays::{parse_overlay_list, SkillSpec, TypedOverlay};
    const HINT: &str = "must be a skill name such as `planner` or `library/planner`";
    if name.is_empty()
        || name == "*"
        || name.trim() != name
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '(' | ')' | ',' | '\\'))
    {
        return Err(HINT.into());
    }
    match parse_overlay_list(&format!("skill({name})")) {
        Ok(parsed)
            if matches!(parsed.as_slice(),
                [TypedOverlay::Skill(SkillSpec::Named(n))] if n == name) =>
        {
            Ok(())
        }
        Ok(_) => Err(HINT.into()),
        Err(reason) => Err(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_lists_default_empty() {
        let resolved = WorkServerConfig::default().resolved();
        assert!(resolved.allow_env.is_empty());
        assert!(resolved.allow_skills.is_empty());
    }

    #[test]
    fn allow_lists_resolve_configured_values() {
        let config: WorkServerConfig = serde_json::from_str(
            r#"{"url":"https://w.example","allowEnv":["GATEWAY_TOKEN","_X1"],"allowSkills":["planner","lib/planner"]}"#,
        )
        .unwrap();
        let resolved = config.resolved();
        assert_eq!(resolved.allow_env, ["GATEWAY_TOKEN", "_X1"]);
        assert_eq!(resolved.allow_skills, ["planner", "lib/planner"]);
    }

    #[test]
    fn allow_env_names_are_validated() {
        for ok in ["A", "_", "GATEWAY_TOKEN", "A1_B2"] {
            assert!(validate_allow_env_entry(ok).is_ok(), "{ok}");
        }
        for bad in ["", "lower", "1A", "A-B", "A B", "A=B", "é", "*"] {
            assert!(validate_allow_env_entry(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn allow_skill_names_are_validated() {
        for ok in ["planner", "team-planner", "awman/create-custom-agent"] {
            assert!(validate_allow_skill_entry(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "*", " a", "a ", "a,b", "a/b/c", "a//b", "..", "a/..", "/a", "a)",
        ] {
            assert!(validate_allow_skill_entry(bad).is_err(), "{bad}");
        }
    }
}
