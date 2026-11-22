//! `remote` section: named API targets `awman remote` and `awman daemon api`
//! talk to.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// The `remote` block.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteConfig {
    /// Target name used when `--remote` names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<BTreeMap<String, RemoteTarget>>,
}

/// One `remote.targets.<name>` entry.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// A target resolved for use: name, URL and (optional) bearer key.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedRemoteTarget {
    pub name: String,
    pub url: String,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for ResolvedRemoteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedRemoteTarget")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "••••"))
            .finish()
    }
}

impl RemoteConfig {
    /// Resolve `name`, else `remote.default`, to a configured target.
    pub fn resolve_target(&self, name: Option<&str>) -> Result<ResolvedRemoteTarget, DataError> {
        let name = name.or(self.default.as_deref()).ok_or_else(|| {
            DataError::Other(
                "no remote target configured: pass --remote <target> or set remote.default \
                     (and remote.targets.<name>.url)"
                    .into(),
            )
        })?;
        let target = self
            .targets
            .as_ref()
            .and_then(|t| t.get(name))
            .ok_or_else(|| {
                DataError::Other(format!(
                    "remote target `{name}` is not configured; set remote.targets.{name}.url"
                ))
            })?;
        let url = target
            .url
            .clone()
            .ok_or_else(|| DataError::Other(format!("remote.targets.{name}.url is not set")))?;
        Ok(ResolvedRemoteTarget {
            name: name.to_string(),
            url,
            api_key: target.api_key.clone(),
        })
    }
}
