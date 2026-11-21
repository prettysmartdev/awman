//! Shared capability detection for the mock server and reference worker.
use crate::{features, WorkItem};

pub(crate) fn supported(registered: &[String], item: &WorkItem) -> bool {
    let supports = |feature: &str| registered.iter().any(|value| value == feature);
    let exec = &item.exec;
    let mut overlays = exec.overlays.clone();
    if let Some(text) = &exec.workflow_toml {
        let Ok(workflow) = toml::from_str::<toml::Value>(text) else {
            return false;
        };
        collect_overlays(&workflow, &mut overlays);
    }
    (exec.secrets.is_empty() || supports(features::LEASE_SECRETS))
        && (!overlays.iter().any(|value| value.contains("env("))
            || supports(features::LEASE_SECRETS))
        && (!overlays.iter().any(|value| value.contains("skill("))
            || supports(features::LEASE_SKILLS))
        && (exec.prerequisites.is_none() || supports(features::LEASE_PREREQUISITES))
        && (exec.workflows_dynamic.is_none() || supports(features::LEASE_WORKFLOWS_DYNAMIC))
}

fn collect_overlays(value: &toml::Value, overlays: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            if let Some(values) = table.get("overlays").and_then(toml::Value::as_array) {
                overlays.extend(
                    values
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned),
                );
            }
            for value in table.values() {
                collect_overlays(value, overlays);
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                collect_overlays(value, overlays);
            }
        }
        _ => {}
    }
}
