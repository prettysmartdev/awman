//! Layer 0 constants shared with the isolated builtin worker.

macro_rules! define_builtin_runtime_version {
    ($version:literal) => {
        /// Pinned Microsandbox SDK runtime metadata version.
        pub const MSB_VERSION: &str = $version;
        /// Protocol tag shared by worker discovery and sandbox ownership.
        pub const WORKER_PROTOCOL: &str = concat!($version, "/18/awman-1");
    };
}

define_builtin_runtime_version!("0.7.2");

/// Encode the SDK metadata version in the fixed-width executable link section.
pub const fn version_bytes(version: &str) -> [u8; 5] {
    let source = version.as_bytes();
    assert!(
        source.len() == 5,
        "builtin runtime version must be exactly 5 bytes"
    );
    let mut bytes = [0; 5];
    let mut index = 0;
    while index < bytes.len() {
        bytes[index] = source[index];
        index += 1;
    }
    bytes
}

/// libkrun revision pinned by the Microsandbox runtime payload.
pub const LIBKRUN_COMMIT: &str = "2bd0f84ad0956f3032e0490d3b8512b6851eca12";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_worker_constants_share_the_pinned_runtime_version() {
        assert_eq!(WORKER_PROTOCOL, format!("{MSB_VERSION}/18/awman-1"));
        assert_eq!(LIBKRUN_COMMIT, "2bd0f84ad0956f3032e0490d3b8512b6851eca12");
    }

    #[test]
    fn constants_match_sdk_dependency_and_payload_pins() {
        let cargo = include_str!("../../Cargo.toml");
        for dependency in [
            "microsandbox",
            "microsandbox-cli",
            "microsandbox-image",
            "microsandbox-network",
        ] {
            let line = cargo
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("{dependency} =")))
                .unwrap_or_else(|| panic!("Cargo.toml is missing {dependency}"));
            assert!(
                line.contains(&format!("version = \"={MSB_VERSION}\"")),
                "Cargo.toml pin for {dependency} does not match {MSB_VERSION}: {line}"
            );
        }

        let manifest = include_str!("../../third_party/msb-payloads/manifest.toml");
        assert!(manifest
            .lines()
            .any(|line| { line.trim() == format!("version = \"{MSB_VERSION}\"") }));
        assert!(manifest
            .lines()
            .any(|line| { line.trim() == format!("libkrun_revision = \"{LIBKRUN_COMMIT}\"") }));
    }

    #[test]
    fn link_section_version_bytes_match_the_shared_constant() {
        assert_eq!(version_bytes(MSB_VERSION), *MSB_VERSION.as_bytes());
    }
}
