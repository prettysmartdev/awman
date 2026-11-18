//! In-image agent CLI version proof (WI 0157).
//!
//! `awman ready` and the first launch of an image execute the agent binary
//! **inside the built image** (`<binary> --version`, no mounts, no network)
//! and classify its answer against the definition's [`CliMajorRequirement`]. A
//! host-installed binary is never consulted, so a host `opencode` 2.x can
//! never satisfy `opencode` (1.x) readiness or vice versa, and the probed
//! binary is the one the launch runs (`cli_version.binary ==
//! interactive_entrypoint[0]`).

use std::time::Duration;

use crate::engine::agent::definition::CliMajorRequirement;
use crate::engine::container::backend::ProbeRequest;
use crate::engine::container::runtime::ContainerRuntime;

pub const AGENT_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// `$1` is the binary; prints its first version line or nothing.
pub const AGENT_VERSION_PROBE_SCRIPT: &str =
    "version=$(\"$@\" --version 2>/dev/null) || exit $?; printf '%s\\n' \"$version\" | head -n 1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliVersionStatus {
    /// The adapter's required major matched; minor and patch versions are unrestricted.
    Compatible { version: String },
    /// Wrong major, unparsable output, or the binary is missing.
    Mismatch { found: String, expected: String },
    /// The probe could not run (no image, runtime refused, test isolation).
    Unknown { reason: String },
}

/// First `\d+\.\d+\.\d+` token in `line` (`opencode v2.3.2` → `2.3.2`).
fn semver_token(line: &str) -> Option<(&str, u64)> {
    let bytes = line.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if bytes[start].is_ascii_digit() && (start == 0 || !bytes[start - 1].is_ascii_digit()) {
            let mut end = start;
            let mut dots = 0;
            let mut last_was_digit = false;
            while end < bytes.len() {
                let b = bytes[end];
                if b.is_ascii_digit() {
                    last_was_digit = true;
                } else if b == b'.' && last_was_digit && dots < 2 {
                    dots += 1;
                    last_was_digit = false;
                } else {
                    break;
                }
                end += 1;
            }
            if dots == 2 && last_was_digit {
                let token = &line[start..end];
                let major = token.split('.').next()?.parse().ok()?;
                return Some((token, major));
            }
        }
        start += 1;
    }
    None
}

/// Pure: classify one version line against a breaking-major requirement.
pub fn classify_cli_version(
    requirement: &CliMajorRequirement,
    first_line: &str,
) -> CliVersionStatus {
    let trimmed = first_line.trim();
    match semver_token(trimmed) {
        Some((version, major)) if major == requirement.required_major => {
            CliVersionStatus::Compatible {
                version: version.to_string(),
            }
        }
        _ => CliVersionStatus::Mismatch {
            found: if trimmed.is_empty() {
                "nothing".into()
            } else {
                trimmed.into()
            },
            expected: format!("{} {}.x", requirement.binary, requirement.required_major),
        },
    }
}

/// Runs the probe inside `image` via the runtime's no-mount probe. Never
/// touches the host `PATH`.
pub fn probe_cli_version(
    runtime: &ContainerRuntime,
    image: &str,
    requirement: &CliMajorRequirement,
) -> CliVersionStatus {
    let process_env = runtime.env();
    if process_env.test_isolation() && !process_env.test_docker() {
        return CliVersionStatus::Unknown {
            reason: "test isolation".into(),
        };
    }
    if !runtime.image_exists(image) {
        return CliVersionStatus::Unknown {
            reason: "image is not available locally".into(),
        };
    }
    let tools = [requirement.binary.as_str()];
    match runtime.run_probe(&ProbeRequest {
        image,
        tools: &tools,
        script: AGENT_VERSION_PROBE_SCRIPT,
        timeout: AGENT_VERSION_PROBE_TIMEOUT,
    }) {
        Ok(output) => classify_cli_version(requirement, output.lines().next().unwrap_or_default()),
        Err(error) => CliVersionStatus::Unknown {
            reason: error.to_string(),
        },
    }
}

/// Definitive probe results keyed by `(image id, major requirement)`. `Unknown` results and
/// images without a content identity are never cached.
pub type AgentVersionCache =
    std::sync::Mutex<std::collections::HashMap<(String, CliMajorRequirementKey), CliVersionStatus>>;

/// The cache's major requirement component.
pub type CliMajorRequirementKey = (String, u64);

/// [`probe_cli_version`], once per image identity.
pub fn probe_cli_version_cached(
    runtime: &ContainerRuntime,
    cache: &AgentVersionCache,
    image: &str,
    requirement: &CliMajorRequirement,
) -> CliVersionStatus {
    let process_env = runtime.env();
    if process_env.test_isolation() && !process_env.test_docker() {
        return CliVersionStatus::Unknown {
            reason: "test isolation".into(),
        };
    }
    let key = runtime
        .image_id(image)
        .map(|id| (id, (requirement.binary.clone(), requirement.required_major)));
    if let (Some(key), Ok(cache)) = (&key, cache.lock()) {
        if let Some(hit) = cache.get(key) {
            return hit.clone();
        }
    }
    let status = probe_cli_version(runtime, image, requirement);
    if let (Some(key), false, Ok(mut cache)) = (
        key,
        matches!(status, CliVersionStatus::Unknown { .. }),
        cache.lock(),
    ) {
        cache.insert(key, status.clone());
    }
    status
}

/// The ready/launch refusal for a [`CliVersionStatus::Mismatch`].
pub fn mismatch_message(
    display_name: &str,
    agent: &str,
    tag: &str,
    found: &str,
    expected: &str,
) -> String {
    format!(
        "{tag} does not contain {display_name}: expected {expected}, found {found}. Update \
         .awman/Dockerfile.{agent} to install {expected} and run 'awman ready --build'."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(major: u64) -> CliMajorRequirement {
        CliMajorRequirement {
            binary: "opencode".into(),
            required_major: major,
        }
    }

    #[test]
    fn major_selection_accepts_any_minor_patch_or_prerelease() {
        for (major, versions) in [
            (1, ["1.0.0", "opencode v1.99.123", "1.100.0-beta.1"]),
            (2, ["2.0.0", "opencode v2.99.123", "2.100.0-beta.1"]),
        ] {
            for version in versions {
                assert!(matches!(
                    classify_cli_version(&requirement(major), version),
                    CliVersionStatus::Compatible { .. }
                ));
                assert!(matches!(
                    classify_cli_version(&requirement(3 - major), version),
                    CliVersionStatus::Mismatch { .. }
                ));
            }
        }
    }

    #[test]
    fn missing_garbled_or_wrong_major_output_is_rejected() {
        for version in ["", "   ", "command not found", "v1.2", "11.0.0", "3.0.0"] {
            assert!(
                matches!(
                    classify_cli_version(&requirement(1), version),
                    CliVersionStatus::Mismatch { .. }
                ),
                "{version}"
            );
        }
    }

    #[test]
    fn mismatch_message_names_the_remediation() {
        let m = mismatch_message(
            "OpenCode 2",
            "opencode-v2",
            "awman-x-opencode-v2:latest",
            "1.19.6",
            "opencode 2.x",
        );
        assert!(m.contains("Update .awman/Dockerfile.opencode-v2"));
        assert!(m.contains("awman ready --build"));
    }
}
