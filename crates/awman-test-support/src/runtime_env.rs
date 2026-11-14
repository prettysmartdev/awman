//! The only place test code reads the process environment.
//!
//! Every input is enumerated here (the `[test_env]` table of `ci/gates.toml`
//! plus the additional named inputs of design 16.7). The values are read
//! once per process into an immutable [`RuntimeEnv`]; nothing else in the
//! test process environment influences a test, and none of these values is
//! passed to a child process (children get [`crate::EnvMap`] only).
//!
//! None of these inputs implies a capability grant except
//! `AWMAN_TEST_HOST_RESOURCES`, which names the real host resources a run may
//! touch. Unset means none: a bare `cargo test` never touches a real engine
//! or boots a guest.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Names of every variable this module reads, for the hygiene lint (H7) and
/// for `gates check` to compare with `[test_env]`.
pub const INPUTS: &[&str] = &[
    "AWMAN_TEST_TIER",
    "AWMAN_TEST_FIXTURES",
    "AWMAN_TEST_ARTIFACT",
    "AWMAN_TEST_ARTIFACT_SHA256",
    "AWMAN_TEST_RUN_CONFIG",
    "AWMAN_TEST_TMPROOT",
    "AWMAN_TEST_EVIDENCE_DIR",
    "AWMAN_TEST_HOST_RESOURCES",
    // Snapshot update mode; set only by `cargo xtask test update-golden`.
    "INSTA_UPDATE",
    // Refuses snapshot updates on CI.
    "CI",
    // Set by cargo and nextest when running a test binary; used for test IDs.
    "CARGO_PKG_NAME",
    // Windows platform system path (resolved, never copied from PATH).
    "SystemRoot",
    // The real home directory: read ONLY so the isolation canary can prove
    // children never see or change it. Never passed to a child.
    "HOME",
    "USERPROFILE",
];

/// The immutable per-process snapshot of the enumerated inputs.
#[derive(Debug, Clone, Default)]
pub struct RuntimeEnv {
    /// `AWMAN_TEST_TIER`: the tier the runner profile selected (validation only).
    pub tier: Option<String>,
    /// `AWMAN_TEST_FIXTURES`: the fixture store directory.
    pub fixtures: Option<PathBuf>,
    /// `AWMAN_TEST_ARTIFACT`: the release executable under test.
    pub artifact: Option<PathBuf>,
    /// `AWMAN_TEST_ARTIFACT_SHA256`: its expected digest.
    pub artifact_sha256: Option<String>,
    /// `AWMAN_TEST_RUN_CONFIG`: the archived run configuration (CI).
    pub run_config: Option<PathBuf>,
    /// `AWMAN_TEST_TMPROOT`: parent directory of every sandbox.
    pub tmproot: Option<PathBuf>,
    /// `AWMAN_TEST_EVIDENCE_DIR`: where failure records go.
    pub evidence_dir: Option<PathBuf>,
    /// `AWMAN_TEST_HOST_RESOURCES`, parsed: `name` or `name=value`, comma separated.
    pub host_resources: BTreeMap<String, String>,
    /// `INSTA_UPDATE`.
    pub insta_update: Option<String>,
    /// Whether `CI` is set to a non-empty value other than `false`/`0`.
    pub ci: bool,
    /// `CARGO_PKG_NAME` of the running test binary.
    pub package: Option<String>,
    /// `SystemRoot` (Windows only).
    pub system_root: Option<PathBuf>,
    /// The real home directory (`HOME` / `USERPROFILE`), for the isolation
    /// canary only.
    pub real_home: Option<PathBuf>,
    /// The operating system's temporary directory (`std::env::temp_dir`).
    pub system_temp: PathBuf,
}

impl RuntimeEnv {
    /// Build from an explicit list of variables (no process access). Tests of
    /// this crate use it; the process snapshot is [`get`].
    pub fn from_vars(vars: &BTreeMap<String, OsString>, system_temp: PathBuf) -> RuntimeEnv {
        let text = |name: &str| {
            vars.get(name)
                .and_then(|v| v.to_str())
                .map(str::to_string)
                .filter(|v| !v.is_empty())
        };
        let path = |name: &str| vars.get(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        RuntimeEnv {
            tier: text("AWMAN_TEST_TIER"),
            fixtures: path("AWMAN_TEST_FIXTURES"),
            artifact: path("AWMAN_TEST_ARTIFACT"),
            artifact_sha256: text("AWMAN_TEST_ARTIFACT_SHA256"),
            run_config: path("AWMAN_TEST_RUN_CONFIG"),
            tmproot: path("AWMAN_TEST_TMPROOT"),
            evidence_dir: path("AWMAN_TEST_EVIDENCE_DIR"),
            host_resources: parse_host_resources(
                text("AWMAN_TEST_HOST_RESOURCES").as_deref().unwrap_or(""),
            ),
            insta_update: text("INSTA_UPDATE"),
            ci: text("CI").is_some_and(|v| v != "false" && v != "0"),
            package: text("CARGO_PKG_NAME"),
            system_root: path("SystemRoot"),
            real_home: if cfg!(windows) {
                path("USERPROFILE")
            } else {
                path("HOME")
            },
            system_temp,
        }
    }

    /// Whether `AWMAN_TEST_HOST_RESOURCES` names `resource`.
    pub fn host_resource(&self, resource: &str) -> Option<&str> {
        self.host_resources.get(resource).map(String::as_str)
    }
}

/// Parse `docker=unix:///var/run/docker.sock,apple-containers,hypervisor`.
pub fn parse_host_resources(text: &str) -> BTreeMap<String, String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| match item.split_once('=') {
            Some((name, value)) => (name.trim().to_string(), value.trim().to_string()),
            None => (item.to_string(), String::new()),
        })
        .collect()
}

/// The process snapshot, read once. The one sanctioned environment read.
#[allow(clippy::disallowed_methods)]
pub fn get() -> &'static RuntimeEnv {
    static SNAPSHOT: OnceLock<RuntimeEnv> = OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        let vars = INPUTS
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|v| (name.to_string(), v)))
            .collect();
        RuntimeEnv::from_vars(&vars, std::env::temp_dir())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect()
    }

    #[test]
    fn host_resources_parse_names_and_values() {
        let parsed = parse_host_resources(" docker=unix:///d.sock , hypervisor,,apple-containers ");
        assert_eq!(
            parsed.get("docker").map(String::as_str),
            Some("unix:///d.sock")
        );
        assert_eq!(parsed.get("hypervisor").map(String::as_str), Some(""));
        assert!(parsed.contains_key("apple-containers"));
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn unset_means_no_host_resources() {
        let env = RuntimeEnv::from_vars(&vars(&[]), PathBuf::from("/tmp"));
        assert!(env.host_resources.is_empty());
        assert!(env.host_resource("docker").is_none());
        assert!(!env.ci);
    }

    #[test]
    fn empty_values_are_unset_and_ci_false_is_off() {
        let env = RuntimeEnv::from_vars(
            &vars(&[
                ("AWMAN_TEST_TMPROOT", ""),
                ("CI", "false"),
                ("AWMAN_TEST_TIER", "component"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert!(env.tmproot.is_none());
        assert!(!env.ci);
        assert_eq!(env.tier.as_deref(), Some("component"));
    }

    #[test]
    fn unknown_variables_are_ignored() {
        let env = RuntimeEnv::from_vars(
            &vars(&[("GITHUB_TOKEN", "ghp_secret")]),
            PathBuf::from("/tmp"),
        );
        assert!(format!("{env:?}").contains("system_temp"));
        assert!(!format!("{env:?}").contains("ghp_secret"));
    }
}
