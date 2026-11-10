//! Typed core daemon operations.
use super::{DaemonApi, Feature, FeatureState};
use crate::command::error::CommandError;
use crate::data::fs::prerequisite_grants::{GrantRecord, GrantSelector};
use crate::data::ids::GrantId;
use crate::data::wire::{List, ListQuery};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureRecord {
    pub enabled_by: Option<crate::data::daemon_state::EnabledBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub oldest_queued_age_ms: Option<u64>,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub since_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    pub name: String,
    pub state: FeatureState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<usize>,
    pub running: usize,
    pub queued: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// `worker` only (WI 0143): pushed jobs running and queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<super::worker::PushStatus>,
    /// `worker` only (WI 0143): one entry per `workServers` entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull: Option<super::worker_pull::PullStatus>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerRecord {
    pub kind: String,
    pub address: String,
    pub tls: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub version: String,
    pub protocol: BTreeMap<String, u32>,
    pub capabilities: Vec<String>,
    pub features: BTreeMap<String, FeatureRecord>,
    pub max_concurrent_jobs: usize,
    pub pid: u32,
    pub uptime_ms: u64,
    pub listeners: crate::command::daemon_runtime::ListenerStatus,
    pub agents: Vec<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadResponse {
    pub applied: Vec<String>,
    pub restart_required: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub config: serde_json::Value,
    pub sources: BTreeMap<String, String>,
}

pub async fn feature(api: &DaemonApi, f: Feature) -> Result<FeatureRecord, CommandError> {
    f.feature_name()
        .map(|name| with_worker_sides(api, api.runtime.feature_record(name)))
        .ok_or_else(|| CommandError::Other("unknown feature".into()))
}
/// Add `push` and `pull` to the `worker` record (WI 0143). The per-feature
/// fields keep 0140's meaning; these split them by arrival path.
pub fn with_worker_sides(api: &DaemonApi, mut record: FeatureRecord) -> FeatureRecord {
    if record.name != Feature::Worker.name() {
        return record;
    }
    let off = record.state == FeatureState::Off;
    record.push = Some(match (&api.worker, off) {
        (Some(worker), false) => super::worker::push_status(worker).unwrap_or_default(),
        _ => Default::default(),
    });
    record.pull = Some(super::worker_pull::pull_status(api));
    record
}
pub async fn features_list(api: &DaemonApi) -> Result<Vec<FeatureRecord>, CommandError> {
    let mut records = Vec::new();
    for f in Feature::ALL {
        records.push(feature(api, f).await?);
    }
    Ok(records)
}
pub async fn status(api: &DaemonApi) -> Result<StatusResponse, CommandError> {
    let mut status = api.runtime.status_snapshot();
    if let Some(worker) = status.features.remove(Feature::Worker.name()) {
        status
            .features
            .insert(worker.name.clone(), with_worker_sides(api, worker));
    }
    let mounts_outputs = api.engines.runtime.capabilities().mounts_outputs(true);
    status.capabilities =
        super::capabilities::advertised(mounts_outputs, &super::worker_pull::capabilities(api));
    let worker = api.runtime.config().daemon().worker;
    if worker.pull_enabled
        && worker.pull_interaction.is_headless()
        && !worker.pull_allow_override
        && !worker.work_servers.values().any(|server| {
            server.enabled && server.interaction.is_some_and(|mode| !mode.is_headless())
        })
        && status
            .features
            .get("engine")
            .is_none_or(|record| record.state != FeatureState::On)
    {
        status
            .capabilities
            .retain(|capability| capability != "interactive" && capability != "terminal");
    }
    Ok(status)
}
pub fn config_get(api: &DaemonApi) -> ConfigResponse {
    let config = api.runtime.config();
    let mut paths = Vec::new();
    leaves(config.value(), "", &mut paths);
    ConfigResponse {
        config: config.masked_value(),
        sources: paths
            .into_iter()
            .map(|p| {
                let source = format!("{:?}", config.source_of(&p)).to_lowercase();
                (p, source)
            })
            .collect(),
    }
}
pub fn grants_list(api: &DaemonApi) -> Result<Vec<GrantResponse>, CommandError> {
    let mut cached = api.grant_store().lock().expect("grants mutex");
    let fresh = crate::data::fs::prerequisite_grants::PrerequisiteGrantStore::load(
        api.engines.auth_engine.api_paths().data_paths().home(),
    );
    if fresh.load_warning.is_some() {
        return Err(CommandError::Other(
            "failed to read prerequisite grants".into(),
        ));
    }
    // A worker or the other daemon can remember a grant after this process
    // starts. Preserve IDs assigned to unchanged v1 records between reads.
    let unchanged =
        cached.records().len() == fresh.records().len()
            && cached.records().iter().zip(fresh.records()).all(|(a, b)| {
                a.key == b.key && a.grant == b.grant && a.recorded_at == b.recorded_at
            });
    if !unchanged {
        *cached = fresh;
    }
    cached
        .records()
        .iter()
        .cloned()
        .map(GrantResponse::try_from)
        .collect()
}
pub fn grants_delete(api: &DaemonApi, id: GrantId) -> Result<bool, CommandError> {
    Ok(api
        .grant_store()
        .lock()
        .expect("grants mutex")
        .revoke(&GrantSelector::Id(id))?
        > 0)
}
pub fn page<T>(items: Vec<T>, query: &ListQuery) -> Result<List<T>, crate::data::wire::WireError> {
    let page = query.page()?;
    let offset = page
        .offset_or_key
        .as_ref()
        .and_then(crate::data::wire::Cursor::offset_value)
        .unwrap_or(0);
    // Memory-backed resources use offset cursors. Reject a key cursor, never silently restart.
    if page
        .offset_or_key
        .as_ref()
        .is_some_and(|c| c.offset_value().is_none())
    {
        return Err(crate::data::wire::WireError::InvalidCursor);
    }
    let total = items.len();
    let end = offset.saturating_add(page.limit);
    Ok(List {
        items: items.into_iter().skip(offset).take(page.limit).collect(),
        next_cursor: (end < total).then(|| crate::data::wire::Cursor::offset(end).to_string()),
    })
}
pub fn at<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(v, |v, p| v.get(p))
}
fn leaves(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    if let Some(o) = v.as_object() {
        for (k, v) in o {
            leaves(
                v,
                &if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                },
                out,
            );
        }
    } else {
        out.push(prefix.into())
    }
}
pub fn changed_paths(
    before: &serde_json::Value,
    after: &serde_json::Value,
    pattern: &str,
) -> Vec<String> {
    // All work-server rows are live. A removed entry must replace the map as
    // a unit: writing null to its individual URL leaf leaves a ghost entry
    // which fails config validation and keeps the old claim policy running.
    if pattern.starts_with("workServers.")
        && before
            .get("workServers")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|entries| {
                entries.keys().any(|name| {
                    after
                        .get("workServers")
                        .and_then(|map| map.get(name))
                        .is_none()
                })
            })
    {
        return if pattern == "workServers.*.url" {
            vec!["workServers".into()]
        } else {
            Vec::new()
        };
    }
    // Label keys may themselves contain dots. Replace each changed label map
    // rather than interpreting a label's spelling as a dotted config path.
    if pattern == "workServers.*.labels" {
        let names = before
            .get("workServers")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|map| map.keys())
            .chain(
                after
                    .get("workServers")
                    .and_then(serde_json::Value::as_object)
                    .into_iter()
                    .flat_map(|map| map.keys()),
            );
        let names: std::collections::BTreeSet<_> = names.collect();
        return names
            .into_iter()
            .filter_map(|name| {
                let path = format!("workServers.{name}.labels");
                (at(before, &path) != at(after, &path)).then_some(path)
            })
            .collect();
    }
    if pattern == "workServers.*.labels.*" {
        return Vec::new();
    }
    let mut paths = Vec::new();
    leaves(before, "", &mut paths);
    leaves(after, "", &mut paths);
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter(|p| {
            let parts: Vec<_> = p.split('.').collect();
            let pat: Vec<_> = pattern.split('.').collect();
            parts.len() == pat.len()
                && parts.iter().zip(pat).all(|(s, p)| p == "*" || *s == p)
                && at(before, p) != at(after, p)
        })
        .collect()
}

/// Store-backed pages resume after a stable record key. Newer inserts before that
/// key do not repeat rows from the preceding page.
pub fn keyed_page<T>(
    items: Vec<T>,
    query: &ListQuery,
    key: impl Fn(&T) -> &str,
) -> Result<List<T>, crate::data::wire::WireError> {
    let page = query.page()?;
    let offset = match page.offset_or_key.as_ref() {
        None => 0,
        Some(cursor) => {
            let anchor = cursor
                .key_value()
                .ok_or(crate::data::wire::WireError::InvalidCursor)?;
            items
                .iter()
                .position(|item| key(item) == anchor)
                .ok_or(crate::data::wire::WireError::InvalidCursor)?
                + 1
        }
    };
    let more = items.len() > offset.saturating_add(page.limit);
    let items = items
        .into_iter()
        .skip(offset)
        .take(page.limit)
        .collect::<Vec<_>>();
    let next_cursor = if more {
        items
            .last()
            .map(|item| crate::data::wire::Cursor::key(key(item)).to_string())
    } else {
        None
    };
    Ok(List { items, next_cursor })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantResponse {
    pub id: GrantId,
    #[serde(flatten)]
    pub key: crate::data::fs::prerequisite_grants::GrantKey,
    pub grant: crate::data::fs::prerequisite_grants::RememberedGrant,
    #[serde(with = "crate::data::wire::timestamp")]
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}
impl TryFrom<GrantRecord> for GrantResponse {
    type Error = CommandError;
    fn try_from(value: GrantRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            key: value.key,
            grant: value.grant,
            recorded_at: chrono::DateTime::parse_from_rfc3339(&value.recorded_at)
                .map_err(|e| CommandError::Other(format!("invalid stored grant timestamp: {e}")))?
                .with_timezone(&chrono::Utc),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}
pub async fn health() -> HealthResponse {
    HealthResponse {
        status: "ok".into(),
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeaturePatch {
    pub enabled: bool,
    #[serde(default)]
    pub ephemeral: bool,
    pub reason: Option<String>,
}
#[derive(Debug, thiserror::Error)]
pub enum FeaturePatchError {
    #[error("persistent changes go through awman config set")]
    PersistentChange,
    #[error(transparent)]
    Change(#[from] crate::command::daemon_runtime::FeatureChangeError),
}
pub async fn feature_patch(
    api: &DaemonApi,
    f: crate::data::daemon_state::FeatureName,
    patch: FeaturePatch,
) -> Result<FeatureRecord, FeaturePatchError> {
    if !patch.ephemeral {
        return Err(FeaturePatchError::PersistentChange);
    }
    let runtime = &api.runtime;
    if patch.enabled {
        runtime.enable_ephemeral(f, patch.reason).await?;
    } else {
        runtime.disable_ephemeral(f).await?;
    }
    Ok(runtime.feature_record(f))
}
pub async fn jobs_cancel_feature(
    api: &DaemonApi,
    f: crate::data::daemon_state::FeatureName,
) -> Result<Vec<String>, CommandError> {
    if f == crate::data::daemon_state::FeatureName::Worker {
        if let Some(pull) = &api.pull {
            pull.cancel_jobs();
        }
    }
    let canceled = api.runtime.cancel_feature_jobs(f).await?;
    let hub = api.terminals.hub();
    let owners: std::collections::HashSet<_> = hub
        .list(None)
        .into_iter()
        .filter(|record| record.feature == f)
        .map(|record| record.owner_id)
        .collect();
    for owner in owners {
        hub.forget_owner(
            &owner,
            crate::engine::agent_runtime::execution::KILLED_EXIT_CODE,
        );
    }
    Ok(canceled)
}
pub async fn config_reload(api: &DaemonApi) -> Result<ReloadResponse, CommandError> {
    api.runtime.reload_config().await
}

#[cfg(test)]
mod worker_reload_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removing_a_work_server_replaces_the_map_without_ghost_leaves() {
        let before = json!({"workServers": {
            "removed": {"url": "https://old.example", "claim": "auto", "labels": {"team": "a"}},
            "kept": {"url": "https://kept.example"}
        }});
        let after =
            json!({"workServers": {"kept": {"url": "https://kept.example", "claim": "auto"}}});
        let paths: Vec<_> = crate::data::config::fields::CONFIG_KEYS
            .iter()
            .filter(|key| key.path.starts_with("workServers."))
            .flat_map(|key| changed_paths(&before, &after, key.path))
            .collect();
        assert_eq!(paths, ["workServers"]);
        let mut applied = before;
        for path in paths {
            crate::data::config::config_json::set_config_field(
                &mut applied,
                &path,
                at(&after, &path).unwrap().clone(),
            );
        }
        assert_eq!(applied, after);
        assert_eq!(
            changed_paths(&applied, &json!({}), "workServers.*.url"),
            ["workServers"]
        );
    }

    #[test]
    fn live_worker_labels_preserve_dots_and_removals_as_map_keys() {
        let before = json!({"workServers":{"team":{"url":"https://example.com","labels":{"team.example/owner":"old", "gone":"yes"}}}});
        let after = json!({"workServers":{"team":{"url":"https://example.com","labels":{"team.example/owner":"new"}}}});
        assert_eq!(
            changed_paths(&before, &after, "workServers.*.labels"),
            ["workServers.team.labels"]
        );
        assert!(changed_paths(&before, &after, "workServers.*.labels.*").is_empty());
        let mut applied = before;
        for path in changed_paths(&applied, &after, "workServers.*.labels") {
            crate::data::config::config_json::set_config_field(
                &mut applied,
                &path,
                at(&after, &path).unwrap().clone(),
            );
        }
        assert_eq!(applied, after);
    }
}
