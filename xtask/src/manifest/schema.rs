use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub tools: BTreeMap<String, Tool>,
    pub capabilities: BTreeMap<String, Capability>,
    pub tiers: BTreeMap<String, Tier>,
    pub features: BTreeMap<String, Features>,
    pub platforms: BTreeMap<String, Platform>,
    pub builds: BTreeMap<String, Build>,
    pub suites: BTreeMap<String, Suite>,
    pub gates: BTreeMap<String, Gate>,
    pub release_critical: Vec<Scenario>,
    pub fixtures: BTreeMap<String, Fixture>,
    pub previous_release: BTreeMap<String, PreviousRelease>,
    pub shipped_switch: Vec<ShippedSwitch>,
    pub test_env: BTreeMap<String, String>,
    pub selection: Selection,
    pub discovery: Discovery,
    pub groups: BTreeMap<String, Group>,
    #[serde(default)]
    pub quarantine: Vec<Quarantine>,
    #[serde(default, rename = "override")]
    pub overrides: Vec<Override>,
    #[serde(default)]
    pub measurement: Vec<Measurement>,
    #[serde(default)]
    pub fakes: BTreeMap<String, ConformancePair>,
    pub release: Release,
    pub legacy_fallback: Vec<LegacyFallback>,
    pub workflows: BTreeMap<String, Workflow>,
    #[serde(skip)]
    pub version: String,
}
impl Manifest {
    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub version: String,
    pub source: String,
    pub sha256: BTreeMap<String, String>,
    pub pending: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub budget_seconds: Option<u64>,
    pub guarantees: Vec<String>,
    #[serde(default)]
    pub allowed_capabilities: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    pub cargo_args: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub runs_on: Vec<String>,
    pub triple: String,
    pub toolchain: String,
    pub image: String,
    pub trust: String,
    pub provides: Vec<String>,
    pub max_vms: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub platform: String,
    pub features: String,
    pub profile: String,
    pub packages: Vec<String>,
    pub target: Option<String>,
    #[serde(default = "test_kind")]
    pub kind: String,
    pub asset: Option<String>,
}
fn test_kind() -> String {
    "test".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub tier: String,
    pub kind: String,
    pub package: Option<String>,
    pub target: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    pub filter: Option<String>,
    pub backend: Option<String>,
    #[serde(default)]
    pub backends: Vec<String>,
    #[serde(default)]
    pub always: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    pub contracts: Vec<String>,
    pub group: Option<String>,
    pub budget_seconds: Option<u64>,
    #[serde(default)]
    pub serial: bool,
    #[serde(default)]
    pub applicability: Vec<Applicability>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    pub test: String,
    pub platform: String,
    pub features: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    #[serde(default)]
    pub includes: Vec<String>,
    pub requires_gate: Option<String>,
    pub budget_minutes: Option<u64>,
    pub required_check: Option<String>,
    #[serde(default)]
    pub build_smokes: Vec<String>,
    #[serde(default)]
    pub cells: Vec<CellGroup>,
    #[serde(default)]
    pub modes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellGroup {
    pub suites: Vec<String>,
    pub builds: Vec<String>,
    pub platform: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub id: String,
    pub suite: String,
    pub build: String,
    pub platform: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub suite: String,
    pub tests: Vec<String>,
    pub gates: Vec<String>,
    pub not_overridable: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub sha256: Option<String>,
    pub source: Option<String>,
    pub file: Option<String>,
    #[serde(default)]
    pub tiers: Vec<String>,
    pub build: Option<String>,
    pub verified_record: Option<String>,
    pub pending: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousRelease {
    pub version: String,
    pub asset: String,
    pub sha256: Option<String>,
    pub pending: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShippedSwitch {
    pub name: String,
    pub effect: String,
    pub reduces_only: bool,
    pub proof: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub full_run: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub allow: Vec<Exclusion>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclusion {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub max_threads: Option<u32>,
    pub max_threads_from: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quarantine {
    pub test: String,
    pub owner: String,
    pub issue: String,
    pub expires: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub gate: String,
    pub cell: String,
    pub reason: String,
    pub expires: String,
    pub approved_by: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub id: String,
    pub blocking: bool,
    pub max: Option<f64>,
    pub variance_note: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformancePair {
    pub fake_test: String,
    pub real_test: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub assets: Vec<String>,
    pub checksums: String,
    pub evidence_valid_days: i64,
    pub candidate_retention_days: u32,
    pub operator_path: String,
    pub environment: String,
    pub ci_guest_labels: Vec<String>,
    pub release_labels: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyFallback {
    pub path: String,
    pub remove_when: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub jobs: Vec<String>,
    pub commands: Vec<String>,
    pub timeout_minutes: u64,
}
