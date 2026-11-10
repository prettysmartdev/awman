//! `workServers.<name>`: work servers a pull worker claims jobs from.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::config::daemon::Interaction;

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
mod tests {
    use super::*;

    #[test]
    fn configured_label_key_rules() {
        for valid in ["pool", "team.example/owner"] {
            assert!(validate_configured_label(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "Bad",
            "bad key",
            "awman/os",
            "team//owner",
            "a/b/c",
            "infact.kind",
        ] {
            assert!(validate_configured_label(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn work_server_defaults_are_applied() {
        let resolved = WorkServerConfig::default().resolved();
        assert!(resolved.enabled);
        assert_eq!(resolved.claim, ClaimMode::Auto);
        assert_eq!(resolved.max_concurrent_jobs, 1);
        assert_eq!(resolved.interaction, None);
        assert!(resolved.labels.is_empty());
    }

    #[test]
    fn normalizes_url_hosts_to_config_names() {
        assert_eq!(
            normalize_work_server_name("https://Infact.Example/awman").unwrap(),
            "infact-example"
        );
        assert_eq!(
            normalize_work_server_name("http://127.0.0.1:9876").unwrap(),
            "127-0-0-1"
        );
        assert!(normalize_work_server_name("/missing-host").is_err());
        assert!(crate::data::config::fields::is_valid_map_name(
            &normalize_work_server_name("https://a-very-long-hostname.example.org").unwrap()
        ));
    }

    #[test]
    fn long_and_underscore_led_hosts_still_normalize_to_valid_names() {
        // 62 characters, then a separator: the name must stop at the cap.
        let long = format!("https://{}.example.com", "a".repeat(62));
        let name = normalize_work_server_name(&long).unwrap();
        assert!(name.len() <= 63, "{name}");
        assert!(crate::data::config::fields::is_valid_map_name(&name));
        assert_eq!(
            normalize_work_server_name("https://_work.example").unwrap(),
            "work-example"
        );
    }

    #[test]
    fn an_all_digit_server_name_is_removed_as_a_key_not_an_array_index() {
        let mut json = serde_json::json!({
            "workServers": { "42": { "url": "https://x.example" }, "other": {} },
            "list": ["a", "b"]
        });
        crate::data::config::config_json::remove_config_field(&mut json, "workServers.42");
        assert!(json["workServers"].get("42").is_none());
        assert!(json["workServers"].get("other").is_some());
        // Array elements are still removed by index.
        crate::data::config::config_json::remove_config_field(&mut json, "list.0");
        assert_eq!(json["list"], serde_json::json!(["b"]));
    }
}
