//! Bounded tool and image-user probe for phase containers.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::data::fs::phase_tools_cache::{PhaseTools, PhaseToolsCache};
use crate::engine::container::backend::ProbeRequest;
use crate::engine::container::runtime::ContainerRuntime;

pub const PHASE_TOOLS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PHASE_TOOLS_SCRIPT: &str = "for t in \"$@\"; do if command -v \"$t\" >/dev/null 2>&1; then echo \"$t=1\"; else echo \"$t=0\"; fi; done; echo \"uid=$(id -u)\"; echo \"user=$(id -un 2>/dev/null)\"";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseToolsStatus {
    Probed(PhaseTools),
    Unknown { reason: String },
}

pub(crate) fn probe(
    runtime: &ContainerRuntime,
    image: &str,
    tools: &[&str],
    cache: &mut PhaseToolsCache,
) -> PhaseToolsStatus {
    let process_env = crate::data::config::env::Env::from_process();
    if process_env.test_isolation() && !process_env.test_docker() {
        return PhaseToolsStatus::Unknown {
            reason: "test isolation".into(),
        };
    }
    let key = runtime.image_id(image).unwrap_or_else(|| image.to_string());
    if let Some(value) = cache
        .get(&key)
        .filter(|v| tools.iter().all(|t| v.tools.contains_key(*t)))
    {
        return PhaseToolsStatus::Probed(value.clone());
    }
    if !runtime.image_exists(image) {
        return PhaseToolsStatus::Unknown {
            reason: "image is not available locally".into(),
        };
    }
    let output = match runtime.run_probe(&ProbeRequest {
        image,
        tools,
        script: PHASE_TOOLS_SCRIPT,
        timeout: PHASE_TOOLS_PROBE_TIMEOUT,
    }) {
        Ok(output) => output,
        Err(error) => {
            return PhaseToolsStatus::Unknown {
                reason: error.to_string(),
            };
        }
    };
    let mut found = BTreeMap::new();
    let mut user = None;
    let mut uid = None;
    for line in output.lines() {
        if let Some((name, value)) = line.split_once('=') {
            if name == "uid" {
                uid = value.parse().ok();
            } else if name == "user" {
                user = Some(value.to_string());
            } else if tools.contains(&name) {
                found.insert(name.to_string(), value == "1");
            }
        }
    }
    let result = PhaseTools {
        tools: found,
        user,
        uid,
        probed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string(),
    };
    let _ = cache.put(key, result.clone());
    PhaseToolsStatus::Probed(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_script_reports_tool_presence_uid_and_user() {
        assert!(PHASE_TOOLS_SCRIPT.contains("command -v"));
        assert!(PHASE_TOOLS_SCRIPT.contains("uid=$(id -u)"));
        // The script reads tool names from its arguments; nothing is interpolated.
        assert!(PHASE_TOOLS_SCRIPT.contains("\"$@\""));
        assert!(PHASE_TOOLS_PROBE_TIMEOUT <= Duration::from_secs(10));
    }

    #[test]
    fn probe_returns_unknown_under_isolation_without_docker_opt_in() {
        let env = crate::data::config::env::Env::from_process();
        if !env.test_isolation() || env.test_docker() {
            return; // only meaningful under tools/isolated-test.sh without AWMAN_TEST_DOCKER
        }
        let runtime = ContainerRuntime::docker();
        let mut cache = PhaseToolsCache::in_memory();
        let status = probe(&runtime, "any:image", &["git", "gh"], &mut cache);
        assert_eq!(
            status,
            PhaseToolsStatus::Unknown {
                reason: "test isolation".into()
            }
        );
        // Not Missing, and nothing was cached.
        assert!(cache.get("any:image").is_none());
        let via_runtime = runtime.probe_phase_tools("any:image", &["git"]);
        assert!(matches!(via_runtime, PhaseToolsStatus::Unknown { .. }));
    }
}
