//! The immutable run configuration: tier budgets and guarantees, the known
//! capability IDs and fixture digests, read from the gate manifest.
//!
//! Source, in order (design 16.7):
//! 1. `AWMAN_TEST_RUN_CONFIG` (CI: the archived run configuration). Set but
//!    unreadable → every sandbox panics `BLOCKED[run-config]`.
//! 2. `<workspace>/ci/gates.toml`, located from this crate's compile-time
//!    `CARGO_MANIFEST_DIR` (local runs).
//! 3. Built-in defaults equal to design section 3, used only when neither
//!    exists (before the manifest is committed). A test in this crate checks
//!    that the defaults equal `ci/gates.toml` whenever that file exists.

use crate::tier::Tier;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One `[tiers.<name>]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TierConfig {
    pub budget_seconds: Option<u64>,
    pub guarantees: Vec<String>,
    pub allowed_capabilities: Vec<String>,
}

/// Where the configuration came from (recorded in failure evidence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    File(PathBuf),
    BuiltinDefaults,
    Unreadable(String),
}

#[derive(Debug, Clone)]
pub(crate) struct RunConfig {
    pub source: Source,
    pub tiers: BTreeMap<String, TierConfig>,
    pub capabilities: Vec<String>,
    /// `[fixtures.<id>]`.
    pub fixtures: BTreeMap<String, FixtureSpec>,
    /// `[previous_release.<target triple>]`.
    pub previous_release: BTreeMap<String, FixtureSpec>,
}

/// One pinned external input (`[fixtures.<id>]` or `[previous_release.<triple>]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FixtureSpec {
    pub sha256: Option<String>,
    /// File name in the fixture store (`file`, or `asset` for previous releases).
    pub file: Option<String>,
    pub tiers: Vec<String>,
    /// Why the pin is not available yet; using the fixture then blocks.
    pub pending: Option<String>,
    pub version: Option<String>,
}

impl RunConfig {
    pub fn tier(&self, tier: Tier) -> TierConfig {
        self.tiers.get(tier.name()).cloned().unwrap_or_default()
    }

    /// Panics `BLOCKED[run-config]` when the configured file could not be read.
    pub fn ensure_usable(&self) {
        if let Source::Unreadable(reason) = &self.source {
            panic!("BLOCKED[run-config]: {reason}");
        }
    }

    pub fn parse(text: &str, source: Source) -> Result<RunConfig, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("invalid TOML: {e}"))?;
        let strings = |v: Option<&toml::Value>| -> Vec<String> {
            v.and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut tiers = BTreeMap::new();
        if let Some(table) = doc.get("tiers").and_then(|t| t.as_table()) {
            for (name, value) in table {
                let budget_seconds = value
                    .get("budget_seconds")
                    .and_then(|v| v.as_integer())
                    .map(|v| v as u64);
                tiers.insert(
                    name.clone(),
                    TierConfig {
                        budget_seconds,
                        guarantees: strings(value.get("guarantees")),
                        allowed_capabilities: strings(value.get("allowed_capabilities")),
                    },
                );
            }
        }
        if tiers.is_empty() {
            return Err("no [tiers] table".to_string());
        }
        let capabilities = doc
            .get("capabilities")
            .and_then(|t| t.as_table())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        let text_of =
            |v: &toml::Value, k: &str| v.get(k).and_then(|s| s.as_str()).map(str::to_string);
        let specs = |key: &str| -> BTreeMap<String, FixtureSpec> {
            doc.get(key)
                .and_then(|t| t.as_table())
                .map(|t| {
                    t.iter()
                        .map(|(id, v)| {
                            let spec = FixtureSpec {
                                sha256: text_of(v, "sha256"),
                                file: text_of(v, "file").or_else(|| text_of(v, "asset")),
                                tiers: strings(v.get("tiers")),
                                pending: text_of(v, "pending"),
                                version: text_of(v, "version"),
                            };
                            (id.clone(), spec)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(RunConfig {
            source,
            tiers,
            capabilities,
            fixtures: specs("fixtures"),
            previous_release: specs("previous_release"),
        })
    }

    pub fn builtin() -> RunConfig {
        RunConfig::parse(BUILTIN_DEFAULTS, Source::BuiltinDefaults)
            .expect("built-in defaults parse")
    }

    fn load(explicit: Option<&Path>, workspace_manifest: &Path) -> RunConfig {
        if let Some(path) = explicit {
            return match std::fs::read_to_string(path) {
                Ok(text) => RunConfig::parse(&text, Source::File(path.to_path_buf()))
                    .unwrap_or_else(|e| RunConfig {
                        source: Source::Unreadable(format!("{}: {e}", path.display())),
                        ..RunConfig::builtin()
                    }),
                Err(e) => RunConfig {
                    source: Source::Unreadable(format!("{}: {e}", path.display())),
                    ..RunConfig::builtin()
                },
            };
        }
        match std::fs::read_to_string(workspace_manifest) {
            Ok(text) => RunConfig::parse(&text, Source::File(workspace_manifest.to_path_buf()))
                .unwrap_or_else(|e| RunConfig {
                    source: Source::Unreadable(format!("{}: {e}", workspace_manifest.display())),
                    ..RunConfig::builtin()
                }),
            Err(_) => RunConfig::builtin(),
        }
    }
}

/// The repository root, from this crate's compile-time manifest directory.
pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .components()
        .collect()
}

pub(crate) fn run_config() -> &'static RunConfig {
    static CONFIG: OnceLock<RunConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let env = crate::runtime_env::get();
        RunConfig::load(
            env.run_config.as_deref(),
            &workspace_root().join("ci/gates.toml"),
        )
    })
}

/// Design section 3 `[capabilities]` and `[tiers]`, verbatim in meaning.
pub(crate) const BUILTIN_DEFAULTS: &str = r#"
[capabilities]
tempdir = {}
loopback = {}
subprocess = {}
awman-binary = {}
fake-cli = {}
local-git = {}
docker = {}
local-registry = {}
git-transports = {}
apple-containers = {}
docker-sbx = {}
hypervisor = {}
builtin-payload = {}
guest-fixtures = {}
previous-release = {}
release-artifact = {}
offline = {}
public-network = {}
registry-credentials = {}

[tiers.unit]
budget_seconds = 1
guarantees = ["tempdir"]
[tiers.component]
budget_seconds = 20
guarantees = ["tempdir", "loopback", "subprocess", "awman-binary", "fake-cli", "local-git"]
[tiers.boundary]
budget_seconds = 120
guarantees = ["tempdir", "loopback", "subprocess", "awman-binary", "fake-cli", "local-git"]
allowed_capabilities = ["docker", "local-registry", "git-transports", "apple-containers", "docker-sbx", "guest-fixtures"]
[tiers.guest]
budget_seconds = 300
guarantees = ["tempdir", "loopback", "subprocess", "awman-binary", "hypervisor", "builtin-payload", "guest-fixtures", "previous-release"]
allowed_capabilities = ["docker", "apple-containers"]
[tiers.artifact]
budget_seconds = 300
guarantees = ["tempdir", "loopback", "subprocess", "release-artifact"]
allowed_capabilities = ["hypervisor", "guest-fixtures", "previous-release", "offline"]
[tiers.soak]
guarantees = ["tempdir", "loopback", "subprocess", "awman-binary"]
allowed_capabilities = ["docker", "hypervisor", "guest-fixtures", "public-network", "registry-credentials"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_defaults_cover_every_tier() {
        let config = RunConfig::builtin();
        for tier in Tier::ALL {
            assert!(config.tiers.contains_key(tier.name()), "{tier}");
        }
        assert_eq!(config.capabilities.len(), 19);
    }

    #[test]
    fn missing_workspace_manifest_falls_back_to_defaults() {
        let config = RunConfig::load(None, Path::new("/nonexistent/ci/gates.toml"));
        assert_eq!(config.source, Source::BuiltinDefaults);
    }

    #[test]
    fn unreadable_explicit_config_blocks() {
        let config = RunConfig::load(Some(Path::new("/nonexistent/run.toml")), Path::new("/x"));
        assert!(matches!(config.source, Source::Unreadable(_)));
        let panic = std::panic::catch_unwind(|| config.ensure_usable()).unwrap_err();
        let message = crate::panic_text(&panic);
        assert!(message.starts_with("BLOCKED[run-config]: "), "{message}");
    }

    #[test]
    fn parse_reads_tiers_capabilities_and_fixtures() {
        let text = r#"
[capabilities]
tempdir = { description = "x" }
[tiers.unit]
budget_seconds = 2
guarantees = ["tempdir"]
[fixtures.guest-oci-base]
sha256 = "abc"
file = "base.tar"
tiers = ["guest"]
[previous_release.x86_64-unknown-linux-gnu]
version = "v1"
asset = "awman-linux-amd64"
pending = "P7"
"#;
        let config = RunConfig::parse(text, Source::BuiltinDefaults).unwrap();
        assert_eq!(config.tier(Tier::Unit).budget_seconds, Some(2));
        assert_eq!(config.capabilities, vec!["tempdir".to_string()]);
        assert_eq!(
            config
                .fixtures
                .get("guest-oci-base")
                .and_then(|f| f.sha256.as_deref()),
            Some("abc")
        );
        assert_eq!(
            config.fixtures["guest-oci-base"].file.as_deref(),
            Some("base.tar")
        );
        let previous = &config.previous_release["x86_64-unknown-linux-gnu"];
        assert_eq!(previous.file.as_deref(), Some("awman-linux-amd64"));
        assert_eq!(previous.pending.as_deref(), Some("P7"));
        assert!(RunConfig::parse("x = 1", Source::BuiltinDefaults).is_err());
    }

    /// The defaults must not drift from the committed manifest.
    #[test]
    fn builtin_defaults_match_the_committed_manifest() {
        let path = workspace_root().join("ci/gates.toml");
        let text = std::fs::read_to_string(&path).expect("ci/gates.toml is committed");
        let committed = RunConfig::parse(&text, Source::File(path)).unwrap();
        let builtin = RunConfig::builtin();
        assert_eq!(committed.tiers, builtin.tiers, "tier tables drifted");
        let mut a = committed.capabilities.clone();
        let mut b = builtin.capabilities.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b, "capability IDs drifted");
    }
}
