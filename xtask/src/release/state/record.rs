//! `release-state.json` (schema 1) and the per-target build records.
//!
//! The draft's `release-state.json` is a journal, not proof: every decision
//! that matters re-hashes the downloaded bytes and compares them with these
//! records, and the records are bound to the candidate identity
//! (repository, version, commit, input digest, manifest version, number).

use super::inputs::CandidateInputs;
use super::{FailingGate, ReleaseState};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const VALIDATION_ASSET: &str = "validation.json";

/// Attested by the trusted gate job; the mutable journal is never authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validation {
    pub record: ReleaseRecord,
    pub gate_sha256: String,
}

pub const STATE_ASSET: &str = "release-state.json";
pub const GATE_ASSET: &str = "gate-evidence-candidate.json";
pub const MAIN_GATE_ASSET: &str = "gate-evidence-main.json";
pub const PRODUCTION: &str = "production";
pub const SUMS: &str = "SHA256SUMS";

/// Who produced a target's bytes. Filled from the Actions environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub run_id: u64,
    pub attempt: u32,
    pub job: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub name: String,
    pub target: String,
    pub build: String,
    /// Digest of the shipped bytes (signed, for macOS).
    pub sha256: String,
    /// Digest straight out of the optimized build, before signing.
    pub unsigned_sha256: String,
    pub bytes: u64,
    pub toolchain: String,
    /// This target's input digest (global inputs + this target).
    pub input_digest: String,
    pub attestation: Option<String>,
    pub entitlements_sha256: Option<String>,
    pub signing_command: Option<Vec<String>>,
    pub producer: Producer,
    pub asset_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRecord {
    /// `main` or `candidate`.
    pub id: String,
    /// `passed`, `failed` or `blocked`.
    pub verdict: String,
    pub gate_evidence_sha256: String,
    pub run_id: u64,
    pub commit: String,
    pub manifest_version: String,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub overrides_applied: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runs {
    pub candidate: u64,
    pub validation: Option<u64>,
    pub promote: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRecord {
    pub schema: u32,
    pub version: String,
    pub candidate: u32,
    /// `production` or `rehearsal/<run id>`. Rehearsal records never
    /// authorize a real promotion.
    pub namespace: String,
    pub repository_id: String,
    /// SHA-256(repo:version:commit), the operator's deterministic request id.
    pub request_id: String,
    pub commit: String,
    pub state: ReleaseState,
    pub manifest_version: String,
    pub input_digest: String,
    /// Inputs plus every artifact digest, set at assembly.
    pub candidate_digest: Option<String>,
    pub dry_run: bool,
    pub inputs: CandidateInputs,
    pub artifacts: Vec<ArtifactRecord>,
    pub gates: Vec<GateRecord>,
    pub failing: Vec<FailingGate>,
    /// Advisory: answers and local tests. `skipped by operator` satisfies nothing.
    pub operator_record: serde_json::Value,
    pub runs: Runs,
    pub release_id: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The fields that identify a candidate. Two records with the same identity
/// describe the same bytes-to-be; anything else is a different candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub repository_id: String,
    pub version: String,
    pub namespace: String,
    pub commit: String,
    pub input_digest: String,
    pub manifest_version: String,
}

impl ReleaseRecord {
    pub fn identity(&self) -> Identity {
        Identity {
            repository_id: self.repository_id.clone(),
            version: self.version.clone(),
            namespace: self.namespace.clone(),
            commit: self.commit.clone(),
            input_digest: self.input_digest.clone(),
            manifest_version: self.manifest_version.clone(),
        }
    }
    pub fn artifact(&self, name: &str) -> Option<&ArtifactRecord> {
        self.artifacts.iter().find(|a| a.name == name)
    }
    pub fn gate(&self, id: &str) -> Option<&GateRecord> {
        self.gates.iter().find(|g| g.id == id)
    }
    /// `SHA256SUMS`: `<64 lowercase hex>  <name>` per asset, manifest order.
    pub fn sha256sums(&self) -> String {
        self.artifacts
            .iter()
            .map(|a| format!("{}  {}\n", a.sha256, a.name))
            .collect()
    }
    /// The candidate digest binds the ordered per-target input and artifact digests.
    pub fn compute_candidate_digest(&self) -> Result<String> {
        let inputs = self
            .artifacts
            .iter()
            .map(|a| (a.name.clone(), a.input_digest.clone()))
            .collect();
        let artifacts = self
            .artifacts
            .iter()
            .map(|a| (a.name.clone(), a.sha256.clone()))
            .collect();
        crate::evidence::aggregate::candidate_digest(&inputs, &artifacts)
    }
}

/// Parse `SHA256SUMS` strictly: exactly two spaces, lowercase hex, no duplicates.
pub fn parse_sha256sums(text: &str) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let (hash, name) = line
            .split_once("  ")
            .ok_or_else(|| anyhow::anyhow!("malformed SHA256SUMS line `{line}`"))?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "malformed digest in SHA256SUMS line `{line}`"
        );
        ensure!(
            !name.is_empty() && !name.contains('/'),
            "bad file name in `{line}`"
        );
        ensure!(
            !out.iter().any(|(_, n)| n == name),
            "duplicate SHA256SUMS entry for {name}"
        );
        out.push((hash.into(), name.into()));
    }
    Ok(out)
}

/// The operator's deterministic request id.
pub fn request_id(repo: &str, version: &str, commit: &str) -> String {
    crate::evidence::digest::sha256(format!("{repo}:{version}:{commit}").as_bytes())
}

/// Draft release name for a candidate. Parsed back by [`candidate_of_name`].
pub fn draft_name(version: &str, candidate: u32) -> String {
    format!("{version} (candidate {candidate})")
}

pub fn candidate_of_name(name: &str) -> Option<u32> {
    name.rsplit_once("(candidate ")?
        .1
        .strip_suffix(')')?
        .parse()
        .ok()
}

/// Tag name a candidate's draft uses: the final tag for production, a
/// namespaced tag for rehearsals so they never collide with a real draft.
pub fn draft_tag(version: &str, namespace: &str) -> String {
    if namespace == PRODUCTION {
        version.into()
    } else {
        format!("{namespace}/{version}")
    }
}

/// Name of the retained per-candidate record artifact of a run.
pub fn record_artifact(version: &str, namespace: &str, candidate: u32) -> String {
    let ns = if namespace == PRODUCTION {
        ""
    } else {
        "rehearsal-"
    };
    format!("candidate-record-{ns}{version}-c{candidate}")
}

/// Name of the verification receipt artifact.
pub fn receipt_artifact(version: &str) -> String {
    format!("release-verified-{version}")
}

/// Authenticated receipt that the public release was verified. Written by
/// the post-publish job; never stored as a release asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema: u32,
    pub version: String,
    pub candidate: u32,
    pub release_id: u64,
    pub tag_commit: String,
    pub assets: Vec<(String, String)>,
    pub targets_verified: Vec<String>,
    pub run_id: u64,
    pub verified_at: DateTime<Utc>,
}

/// `vX.Y.Z` with numeric components.
pub fn valid_version(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256sums_parse_is_strict() {
        let h = "a".repeat(64);
        assert_eq!(
            parse_sha256sums(&format!("{h}  awman-linux-amd64\n")).unwrap(),
            vec![(h.clone(), "awman-linux-amd64".into())]
        );
        assert!(parse_sha256sums(&format!("{h} awman\n")).is_err());
        assert!(parse_sha256sums(&format!("{}  awman\n", "A".repeat(64))).is_err());
        assert!(parse_sha256sums(&format!("{h}  a\n{h}  a\n")).is_err());
        assert!(parse_sha256sums(&format!("{h}  ../a\n")).is_err());
    }

    #[test]
    fn names_round_trip_and_rehearsals_are_namespaced() {
        assert_eq!(candidate_of_name(&draft_name("v1.2.3", 7)), Some(7));
        assert_eq!(candidate_of_name("v1.2.3"), None);
        assert_eq!(draft_tag("v1.2.3", PRODUCTION), "v1.2.3");
        assert_eq!(draft_tag("v1.2.3", "rehearsal/99"), "rehearsal/99/v1.2.3");
        assert_ne!(
            record_artifact("v1.2.3", PRODUCTION, 1),
            record_artifact("v1.2.3", "rehearsal/99", 1)
        );
        assert!(valid_version("v0.14.0"));
        assert!(!valid_version("0.14.0") && !valid_version("v1.2") && !valid_version("v1.x.3"));
    }
}
