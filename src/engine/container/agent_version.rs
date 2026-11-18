//! In-image agent CLI version proof (WI 0157).
//!
//! `awman ready` and the first launch of an image execute the agent binary
//! **inside the built image** (`<binary> --version`, no mounts, no network)
//! and classify its answer against the definition's [`CliVersionPin`]. A
//! host-installed binary is never consulted, so a host `opencode` 2.x can
//! never satisfy `opencode` (1.x) readiness or vice versa, and the probed
//! binary is the one the launch runs (`cli_version.binary ==
//! interactive_entrypoint[0]`).

use std::time::Duration;

use crate::engine::agent::definition::CliVersionPin;
use crate::engine::container::backend::ProbeRequest;
use crate::engine::container::runtime::ContainerRuntime;

pub const AGENT_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// `$1` is the binary; prints its first version line or nothing.
pub const AGENT_VERSION_PROBE_SCRIPT: &str =
    "version=$(\"$@\" --version 2>/dev/null) || exit $?; printf '%s\\n' \"$version\" | head -n 1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliVersionStatus {
    /// Required major matched and the version equals the tested pin.
    Tested { version: String },
    /// Acceptable (major matches, or no major rule) but not the tested pin.
    Untested { version: String, tested: String },
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

fn expected(pin: &CliVersionPin) -> String {
    match pin.required_major {
        Some(major) => format!("{} {major}.x", pin.binary),
        None => format!("{} {}", pin.binary, pin.tested_version),
    }
}

/// Pure: classify one version line against a pin.
pub fn classify_cli_version(pin: &CliVersionPin, first_line: &str) -> CliVersionStatus {
    let trimmed = first_line.trim();
    let found = if trimmed.is_empty() {
        "nothing".to_string()
    } else {
        trimmed.to_string()
    };
    let version = match pin.required_major {
        Some(required) => match semver_token(trimmed) {
            Some((token, major)) if major == required => token.to_string(),
            _ => {
                return CliVersionStatus::Mismatch {
                    found,
                    expected: expected(pin),
                }
            }
        },
        None => {
            if trimmed != pin.tested_version {
                return CliVersionStatus::Mismatch {
                    found,
                    expected: expected(pin),
                };
            }
            trimmed.to_string()
        }
    };
    if version == pin.tested_version {
        CliVersionStatus::Tested { version }
    } else {
        CliVersionStatus::Untested {
            version,
            tested: pin.tested_version.clone(),
        }
    }
}

/// Runs the probe inside `image` via the runtime's no-mount probe. Never
/// touches the host `PATH`.
pub fn probe_cli_version(
    runtime: &ContainerRuntime,
    image: &str,
    pin: &CliVersionPin,
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
    let mut tools = vec![pin.binary.as_str()];
    if pin.required_major.is_none() {
        tools.push("--disable-auto-update");
    }
    match runtime.run_probe(&ProbeRequest {
        image,
        tools: &tools,
        script: AGENT_VERSION_PROBE_SCRIPT,
        timeout: AGENT_VERSION_PROBE_TIMEOUT,
    }) {
        Ok(output) => classify_cli_version(pin, output.lines().next().unwrap_or_default()),
        Err(error) => CliVersionStatus::Unknown {
            reason: error.to_string(),
        },
    }
}

/// Definitive probe results keyed by `(image id, pin)`. `Unknown` results and
/// images without a content identity are never cached.
pub type AgentVersionCache =
    std::sync::Mutex<std::collections::HashMap<(String, CliVersionPinKey), CliVersionStatus>>;

/// The cache's pin component.
pub type CliVersionPinKey = (String, String, Option<u64>);

/// [`probe_cli_version`], once per image identity.
pub fn probe_cli_version_cached(
    runtime: &ContainerRuntime,
    cache: &AgentVersionCache,
    image: &str,
    pin: &CliVersionPin,
) -> CliVersionStatus {
    let process_env = runtime.env();
    if process_env.test_isolation() && !process_env.test_docker() {
        return CliVersionStatus::Unknown {
            reason: "test isolation".into(),
        };
    }
    let key = runtime.image_id(image).map(|id| {
        (
            id,
            (
                pin.binary.clone(),
                pin.tested_version.clone(),
                pin.required_major,
            ),
        )
    });
    if let (Some(key), Ok(cache)) = (&key, cache.lock()) {
        if let Some(hit) = cache.get(key) {
            return hit.clone();
        }
    }
    let status = probe_cli_version(runtime, image, pin);
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
        "{tag} does not contain {display_name}: expected {expected}, found {found}. Delete \
         .awman/Dockerfile.{agent} and run 'awman ready --build'."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(binary: &str, tested: &str, major: Option<u64>) -> CliVersionPin {
        CliVersionPin {
            binary: binary.into(),
            tested_version: tested.into(),
            required_major: major,
        }
    }

    fn v1() -> CliVersionPin {
        pin("opencode", "1.19.6", Some(1))
    }
    fn v2() -> CliVersionPin {
        pin("opencode", "2.3.2", Some(2))
    }
    fn cursor() -> CliVersionPin {
        pin("cursor-agent", "2026.11.17-f532c45", None)
    }

    #[test]
    fn v1_and_v2_binaries_are_told_apart() {
        assert_eq!(
            classify_cli_version(&v1(), "1.19.6\n"),
            CliVersionStatus::Tested {
                version: "1.19.6".into()
            }
        );
        assert_eq!(
            classify_cli_version(&v2(), "opencode v2.3.2"),
            CliVersionStatus::Tested {
                version: "2.3.2".into()
            }
        );
        // A V2 binary never satisfies V1 readiness, nor V1 → V2.
        assert!(matches!(
            classify_cli_version(&v1(), "opencode v2.3.2"),
            CliVersionStatus::Mismatch { .. }
        ));
        assert!(matches!(
            classify_cli_version(&v2(), "1.19.6"),
            CliVersionStatus::Mismatch { .. }
        ));
        // A two-digit major is not mistaken for its last digit.
        assert!(matches!(
            classify_cli_version(&v1(), "11.0.0"),
            CliVersionStatus::Mismatch { .. }
        ));
    }

    #[test]
    fn same_major_other_build_is_untested() {
        assert_eq!(
            classify_cli_version(&v1(), "1.19.7"),
            CliVersionStatus::Untested {
                version: "1.19.7".into(),
                tested: "1.19.6".into()
            }
        );
        assert_eq!(
            classify_cli_version(&cursor(), "2026.12.01-abcdef0"),
            CliVersionStatus::Mismatch {
                found: "2026.12.01-abcdef0".into(),
                expected: "cursor-agent 2026.11.17-f532c45".into()
            }
        );
        assert_eq!(
            classify_cli_version(&cursor(), "2026.11.17-f532c45"),
            CliVersionStatus::Tested {
                version: "2026.11.17-f532c45".into()
            }
        );
    }

    #[test]
    fn empty_or_garbled_output_is_a_mismatch() {
        for line in ["", "   ", "command not found", "v1.2"] {
            assert!(
                matches!(
                    classify_cli_version(&v1(), line),
                    CliVersionStatus::Mismatch { .. }
                ),
                "{line:?}"
            );
        }
        for line in ["", "sh: cursor-agent: not found"] {
            assert!(
                matches!(
                    classify_cli_version(&cursor(), line),
                    CliVersionStatus::Mismatch { .. }
                ),
                "{line:?}"
            );
        }
        assert_eq!(
            classify_cli_version(&v2(), ""),
            CliVersionStatus::Mismatch {
                found: "nothing".into(),
                expected: "opencode 2.x".into()
            }
        );
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
        assert!(m.contains("Delete .awman/Dockerfile.opencode-v2"));
        assert!(m.contains("awman ready --build"));
    }
}
