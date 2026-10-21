//! Layer 0 constants shared with the isolated builtin worker.

macro_rules! define_builtin_runtime_version {
    ($version:literal) => {
        /// Pinned Microsandbox SDK runtime metadata version.
        pub const MSB_VERSION: &str = $version;
        /// Protocol tag shared by worker discovery and sandbox ownership.
        pub const WORKER_PROTOCOL: &str = concat!($version, "/18/awman-1");
    };
}

define_builtin_runtime_version!("0.7.8");

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
pub const LIBKRUN_COMMIT: &str = "b35c7e118d7a1e48c8fad103b258e64ff5c49147";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_worker_constants_share_the_pinned_runtime_version() {
        assert_eq!(WORKER_PROTOCOL, format!("{MSB_VERSION}/18/awman-1"));
        assert_eq!(LIBKRUN_COMMIT, "b35c7e118d7a1e48c8fad103b258e64ff5c49147");
    }

    /// SDK crates whose exact pin must equal [`MSB_VERSION`].
    const MSB_CRATES: [&str; 4] = [
        "microsandbox",
        "microsandbox-cli",
        "microsandbox-image",
        "microsandbox-network",
    ];

    /// Every exact-pinned builtin-runtime crate, including those versioned
    /// independently of [`MSB_VERSION`].
    const PINNED_CRATES: [&str; 6] = [
        "microsandbox",
        "microsandbox-cli",
        "microsandbox-image",
        "microsandbox-network",
        "sqlx",
        "msb_krun",
    ];

    fn parse_manifest(name: &str, text: &str) -> toml::Table {
        text.parse()
            .unwrap_or_else(|e| panic!("{name} is not valid TOML: {e}"))
    }

    /// The `[workspace.dependencies]` table of the root manifest.
    fn workspace_dependencies(root: &toml::Table) -> &toml::Table {
        root.get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(toml::Value::as_table)
            .expect("root Cargo.toml has no [workspace.dependencies]")
    }

    /// The version requirement a workspace dependency entry declares.
    fn pinned_version<'a>(deps: &'a toml::Table, dependency: &str) -> &'a str {
        deps.get(dependency)
            .and_then(|d| d.get("version"))
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("[workspace.dependencies] is missing {dependency}"))
    }

    /// Every entry for `dependency` in a member's normal, build, dev and
    /// target-specific dependency tables.
    fn member_entries<'a>(manifest: &'a toml::Table, dependency: &str) -> Vec<&'a toml::Value> {
        let mut tables: Vec<&toml::Table> = Vec::new();
        for section in ["dependencies", "build-dependencies", "dev-dependencies"] {
            tables.extend(manifest.get(section).and_then(toml::Value::as_table));
            if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
                for target in targets.values() {
                    tables.extend(target.get(section).and_then(toml::Value::as_table));
                }
            }
        }
        tables
            .into_iter()
            .filter_map(|t| t.get(dependency))
            .collect()
    }

    #[test]
    fn constants_match_sdk_dependency_and_payload_pins() {
        let root = parse_manifest("Cargo.toml", include_str!("../../Cargo.toml"));
        let deps = workspace_dependencies(&root);
        for dependency in MSB_CRATES {
            assert_eq!(
                pinned_version(deps, dependency),
                format!("={MSB_VERSION}"),
                "[workspace.dependencies] pin for {dependency} does not match {MSB_VERSION}"
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
    fn workspace_members_inherit_sdk_pins_instead_of_restating_them() {
        let members = [
            ("Cargo.toml", include_str!("../../Cargo.toml")),
            (
                "crates/awman-cli/Cargo.toml",
                include_str!("../../crates/awman-cli/Cargo.toml"),
            ),
            (
                "crates/awman-host-shims/Cargo.toml",
                include_str!("../../crates/awman-host-shims/Cargo.toml"),
            ),
        ];
        let root = parse_manifest("Cargo.toml", members[0].1);
        let deps = workspace_dependencies(&root);
        for dependency in PINNED_CRATES {
            assert!(
                pinned_version(deps, dependency).starts_with('='),
                "[workspace.dependencies] {dependency} must be an exact `=` pin"
            );
        }

        for (name, text) in members {
            let manifest = parse_manifest(name, text);
            for dependency in PINNED_CRATES {
                for entry in member_entries(&manifest, dependency) {
                    assert_eq!(
                        entry.get("workspace").and_then(toml::Value::as_bool),
                        Some(true),
                        "{name}: {dependency} must use `workspace = true`"
                    );
                    assert!(
                        entry.get("version").is_none(),
                        "{name}: {dependency} restates a version instead of inheriting it"
                    );
                }
            }
        }
    }

    #[test]
    fn vendored_sdk_patches_match_the_pinned_versions() {
        let root = parse_manifest("Cargo.toml", include_str!("../../Cargo.toml"));
        let deps = workspace_dependencies(&root);
        let patches = root
            .get("patch")
            .and_then(|p| p.get("crates-io"))
            .and_then(toml::Value::as_table)
            .expect("root Cargo.toml has no [patch.crates-io]");
        let lock: toml::Value = toml::from_str(include_str!("../../Cargo.lock")).unwrap();
        for (dependency, patch) in patches {
            let path = patch["path"]
                .as_str()
                .expect("repository-relative patch path");
            let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(path)
                .join("Cargo.toml");
            let manifest: toml::Value =
                toml::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
            let version = manifest["package"]["version"].as_str().unwrap();
            let packages: Vec<_> = lock["package"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["name"].as_str() == Some(dependency))
                .collect();
            assert_eq!(
                packages.len(),
                1,
                "{dependency}: patch must resolve exactly once"
            );
            assert_eq!(
                packages[0]["version"].as_str(),
                Some(version),
                "{dependency}: unused patch version"
            );
            assert!(
                packages[0].get("source").is_none(),
                "{dependency}: patch fell off"
            );
            if deps.contains_key(dependency) {
                assert_eq!(
                    pinned_version(deps, dependency).trim_start_matches('='),
                    version
                );
            }
        }
    }

    #[test]
    fn link_section_version_bytes_match_the_shared_constant() {
        assert_eq!(version_bytes(MSB_VERSION), *MSB_VERSION.as_bytes());
    }
}
