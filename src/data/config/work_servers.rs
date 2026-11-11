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
