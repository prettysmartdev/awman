//! `workItems` section: local work item files and trusted external sources.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Default work items directory, relative to the repo root.
pub const DEFAULT_WORK_ITEMS_DIR: &str = "aspec/work-items";
/// Template file name inside the work items directory.
pub const DEFAULT_WORK_ITEM_TEMPLATE: &str = "0000-template.md";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum WorkItemSource {
    #[default]
    Local,
    Github,
    Http {
        name: String,
    },
    /// An unrecognized persisted value. Kept so config loading can warn and
    /// work-item operations can fail closed rather than silently use local.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkItemConfigError {
    #[error("unknown work item source `{value}` (expected `local`, `github`, or `http:<name>`)")]
    UnknownSource { value: String },
    #[error("invalid work item provider name `{name}` (must match [a-z0-9][a-z0-9_-]{{0,62}})")]
    InvalidProviderName { name: String },
    #[error("unknown work item provider `{name}`")]
    UnknownProvider { name: String },
    #[error("invalid baseUrl for work item provider `{name}`: {reason}")]
    InvalidBaseUrl { name: String, reason: String },
    #[error("invalid tokenEnv for work item provider `{name}`")]
    InvalidTokenEnv { name: String },
    #[error("work item provider `{name}` is untrusted outside the global config file")]
    UntrustedProviderOverride { name: String },
}

impl WorkItemSource {
    pub fn parse(value: &str) -> Result<Self, WorkItemConfigError> {
        match value {
            "local" => Ok(Self::Local),
            "github" => Ok(Self::Github),
            _ => match value.strip_prefix("http:") {
                Some(name) => {
                    validate_provider_name(name)?;
                    Ok(Self::Http { name: name.into() })
                }
                None => Err(WorkItemConfigError::UnknownSource {
                    value: value.into(),
                }),
            },
        }
    }

    pub fn as_config_value(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Github => "github".into(),
            Self::Http { name } => format!("http:{name}"),
            Self::Invalid(value) => value.clone(),
        }
    }
}

impl Serialize for WorkItemSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_config_value())
    }
}

impl<'de> Deserialize<'de> for WorkItemSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value).unwrap_or(Self::Invalid(value)))
    }
}

pub fn validate_provider_name(name: &str) -> Result<(), WorkItemConfigError> {
    if crate::data::config::fields::is_valid_map_name(name) {
        Ok(())
    } else {
        Err(WorkItemConfigError::InvalidProviderName { name: name.into() })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkItemProviderConfig {
    pub base_url: String,
    pub token_env: String,
}

impl WorkItemProviderConfig {
    pub fn validate(&self, name: &str) -> Result<(), WorkItemConfigError> {
        validate_provider_name(name)?;
        let invalid = |reason: &str| WorkItemConfigError::InvalidBaseUrl {
            name: name.into(),
            reason: reason.into(),
        };
        let url = reqwest::Url::parse(&self.base_url).map_err(|e| invalid(&e.to_string()))?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err(invalid("must use https, or http on loopback"));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid("userinfo, query, and fragment are not allowed"));
        }
        if url.scheme() == "http" && !is_loopback_host(&url) {
            return Err(invalid(
                "http is allowed only for literal loopback addresses",
            ));
        }
        if !valid_env_name(&self.token_env) {
            return Err(WorkItemConfigError::InvalidTokenEnv { name: name.into() });
        }
        Ok(())
    }
}

/// Whether `url` names a loopback host: `localhost` or a literal loopback
/// IPv4/IPv6 address. Shared by config validation and the HTTP transport so the
/// two never disagree about which `http` base URLs are allowed.
pub fn is_loopback_host(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The `workItems` block. Provider definitions are trusted global config;
/// repo config may only select a provider by name.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkItemsConfig {
    /// Work items directory (relative to the repo root, or absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Work item template (relative to the repo root, or absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkItemSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<BTreeMap<String, WorkItemProviderConfig>>,
}

impl WorkItemsConfig {
    pub fn validate_providers(&self) -> Result<(), WorkItemConfigError> {
        for (name, provider) in self.providers.iter().flatten() {
            provider.validate(name)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_base_urls_are_accepted_only_on_loopback() {
        let provider = |base_url: &str| WorkItemProviderConfig {
            base_url: base_url.into(),
            token_env: "TRACKER_TOKEN".into(),
        };
        for base in [
            "http://127.0.0.1:8080/items",
            "http://[::1]:8080/items",
            "http://localhost:8080/items",
            "http://LOCALHOST/items",
            "https://tracker.example/items",
        ] {
            assert_eq!(provider(base).validate("tracker"), Ok(()), "{base}");
        }
        for base in [
            "http://10.0.0.1/items",
            "http://[2001:db8::1]/items",
            "http://tracker.example/items",
            "http://localhost.evil.example/items",
        ] {
            assert!(provider(base).validate("tracker").is_err(), "{base}");
        }
    }

    #[test]
    fn source_serializes_as_a_single_config_string() {
        let config = WorkItemsConfig {
            source: Some(WorkItemSource::Http {
                name: "issues".into(),
            }),
            ..Default::default()
        };
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["source"], "http:issues");
        assert_eq!(
            serde_json::from_value::<WorkItemsConfig>(value).unwrap(),
            config
        );
    }

    #[test]
    fn parses_accepted_sources_and_preserves_unknown_values() {
        assert_eq!(WorkItemSource::default(), WorkItemSource::Local);
        assert_eq!(
            WorkItemSource::parse("local").unwrap(),
            WorkItemSource::Local
        );
        assert_eq!(
            WorkItemSource::parse("github").unwrap(),
            WorkItemSource::Github
        );
        assert_eq!(
            WorkItemSource::parse("http:issues").unwrap(),
            WorkItemSource::Http {
                name: "issues".into()
            }
        );
        let unknown: WorkItemsConfig = serde_json::from_str(r#"{"source":"mystery"}"#).unwrap();
        assert_eq!(
            unknown.source,
            Some(WorkItemSource::Invalid("mystery".into()))
        );
        assert!(WorkItemSource::parse("mystery").is_err());
        assert!(WorkItemSource::parse("http:missing-provider").is_ok());
        assert!(WorkItemSource::parse("http:").is_err());
    }

    #[test]
    fn provider_names_and_urls_are_validated() {
        assert!(validate_provider_name("a-1_ok").is_ok());
        assert!(validate_provider_name("Bad").is_err());
        let config = WorkItemProviderConfig {
            base_url: "https://issues.example/api".into(),
            token_env: "ISSUES_TOKEN".into(),
        };
        assert!(config.validate("issues").is_ok());
        assert!(WorkItemProviderConfig {
            base_url: "http://example.com".into(),
            ..config.clone()
        }
        .validate("issues")
        .is_err());
        assert!(WorkItemProviderConfig {
            token_env: "TOKEN-SECRET".into(),
            ..config
        }
        .validate("issues")
        .is_err());
    }

    #[test]
    fn source_set_validation_rejects_unknown_values() {
        let key = crate::data::config::fields::resolve("workItems.source").unwrap();
        assert_eq!(
            crate::data::config::fields::parse_input(key.key, "github").unwrap(),
            serde_json::json!("github")
        );
        assert!(crate::data::config::fields::parse_input(key.key, "elsewhere").is_err());
    }

    #[test]
    fn repo_provider_definitions_are_rejected_by_scope_validation() {
        let value = serde_json::json!({
            "workItems": { "providers": { "team": {
                "baseUrl": "https://issues.example",
                "tokenEnv": "ISSUES_TOKEN"
            }}}
        });
        let error =
            crate::data::config::ConfigFile::from_value(value, crate::data::config::FileKind::Repo)
                .unwrap_err()
                .to_string();
        assert!(error.contains("global config file"), "{error}");
    }

    #[test]
    fn unknown_provider_does_not_resolve_as_local_and_warns() {
        let config = crate::data::config::EffectiveConfig::from_files(
            crate::data::config::ConfigFile::default(),
            serde_json::from_value(serde_json::json!({
                "workItems": { "source": "http:missing" }
            }))
            .unwrap(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        assert!(matches!(
            config.work_item_source(),
            Err(WorkItemConfigError::UnknownProvider { .. })
        ));
        assert!(!config.load_warnings().is_empty());
    }
}
