//! Schema 1. CI authenticates producer metadata independently; records never authenticate themselves.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Passed,
    Failed,
    Blocked,
    Skipped,
    Inconclusive,
    NotSelected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shard {
    pub index: u32,
    pub of: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inputs {
    pub schema: u32,
    pub source_tree: String,
    pub cargo_manifests: BTreeMap<String, String>,
    pub cargo_lock: String,
    pub toolchain: String,
    pub tool_digests: BTreeMap<String, String>,
    pub rust_toolchain_file: Option<String>,
    pub cargo_config: String,
    pub build_scripts: BTreeMap<String, String>,
    pub payload_manifest: Option<String>,
    pub features: Vec<String>,
    pub target: String,
    pub profile: String,
    pub runner_image: String,
    pub fixtures: BTreeMap<String, String>,
    pub previous_release: Option<String>,
    pub signing: Option<Signing>,
    pub release_notes: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signing {
    pub entitlements_sha256: String,
    pub command: Vec<String>,
    pub identity: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub provider: String,
    pub repository_id: String,
    pub workflow_path: String,
    pub workflow_revision: String,
    pub run_id: u64,
    pub attempt: u32,
    pub job: String,
    pub job_id: u64,
    pub artifact_id: u64,
    pub artifact_service_digest: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultRecord {
    pub test: String,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub classification: Option<String>,
    pub reason: Option<String>,
    pub capabilities_lost: Vec<String>,
    pub assertions: Vec<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub expected: u64,
    pub ran: u64,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub blocked: u64,
    pub quarantined_failed: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioResult {
    pub id: String,
    pub outcome: Outcome,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowTest {
    pub test: String,
    pub ms: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Durations {
    pub queue_ms: u64,
    pub setup_ms: u64,
    pub fixtures_ms: u64,
    pub run_ms: u64,
    pub total_ms: u64,
    pub slowest: Vec<SlowTest>,
    pub compile_ms: Option<u64>,
    pub link_ms: Option<u64>,
    pub cache_restore_ms: Option<u64>,
    pub previous_release_ms: Option<u64>,
    pub vm_boot_ms: Option<u64>,
    pub signing_ms: Option<u64>,
    pub upload_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    pub key: String,
    pub hit: bool,
    pub restored_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub schema: u32,
    pub kind: String,
    pub gate: String,
    pub cell: String,
    pub suite: String,
    pub tier: String,
    pub backend: Option<String>,
    pub build: String,
    pub platform: String,
    pub shard: Shard,
    pub commit: String,
    pub tree: String,
    pub manifest_version: String,
    pub input_digest: String,
    pub inputs: Inputs,
    pub expected_set_sha256: String,
    pub archive_sha256: String,
    pub plan_sha256: String,
    pub artifacts: Vec<Artifact>,
    pub counts: Counts,
    pub results: Vec<ResultRecord>,
    pub release_critical: Vec<ScenarioResult>,
    pub outcome: Outcome,
    pub classification: Option<String>,
    pub durations: Durations,
    pub cache: Option<Cache>,
    pub reproduce: String,
    pub run: Run,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub command_exit: i32,
    pub phases: BTreeMap<String, Outcome>,
    pub capabilities_probed: BTreeSet<String>,
}
/// Produced separately from a compiled list, never inferred from the results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedSet {
    pub schema: u32,
    pub build: String,
    pub cell: String,
    pub manifest_version: String,
    pub archive_sha256: String,
    pub tests: BTreeSet<String>,
    /// Independently listed nextest partitions; disjoint union equals tests.
    pub partitions: BTreeMap<u32, BTreeSet<String>>,
    pub contracts: BTreeMap<String, BTreeSet<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateCell {
    pub cell: String,
    pub outcome: Outcome,
    pub evidence_sha256: Vec<String>,
    pub counts: Counts,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateEvidence {
    pub schema: u32,
    pub kind: String,
    pub gate: String,
    pub commit: String,
    pub manifest_version: String,
    pub plan_sha256: String,
    pub verdict: Outcome,
    pub cells: Vec<GateCell>,
    pub missing: Vec<String>,
    pub errors: Vec<String>,
    pub release_critical: Vec<ScenarioResult>,
    pub quarantine: Vec<String>,
    pub overrides_applied: Vec<String>,
    pub queue_ms: u64,
    pub execution_ms: u64,
    pub p95_budget_minutes: Option<u64>,
    pub generated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}
/// Transport handoff. Construct only from trusted service API responses and verified
/// artifact/attestation bytes, outside the evidence download directory. Core compares
/// every field and never accepts an uploader's `authenticated: true` assertion.
#[derive(Debug, Clone)]
pub struct VerifiedProducer {
    pub identity: Run,
    pub evidence_sha256: String,
    pub conclusion: Outcome,
    pub subject_sha256: String,
}
#[derive(Debug, Clone)]
pub struct TrustedContext {
    pub request: crate::gates::plan::Request,
    pub plan_sha256: String,
    pub producers: Vec<VerifiedProducer>,
    pub expected: BTreeMap<String, ExpectedSet>,
    pub input_digests: BTreeMap<String, String>,
    pub archive_digests: BTreeMap<String, String>,
    /// Required workflow roles from the service API, including the plan job.
    pub jobs: BTreeMap<String, Outcome>,
    pub now: DateTime<Utc>,
}
