//! Release-owned documents (design 3.9, 7.4, 7.13): the remote claim, the
//! byte manifest (`SHA256SUMS`), the prepublication evidence
//! (`release-evidence.json`), the immutable seal and the postpublication
//! receipt. All are deterministic for the same inputs so a retry after a
//! lost response regenerates identical bytes.

use super::journal::{Journal, SealedAsset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Owner {
    pub id: String,
    pub host: String,
    pub pid: u32,
}

/// `claim.json` on `awman-release-state/<key>`: inert metadata only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimDoc {
    pub schema: u32,
    pub repository_id: u64,
    pub version: String,
    pub mode: String,
    pub commit: String,
    pub tree: String,
    pub base_main: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    pub generation: u32,
    pub operation: String,
    pub nonce: String,
    pub push_ref: String,
    pub owner: Owner,
    #[serde(default)]
    pub previous_owners: Vec<Owner>,
    #[serde(default)]
    pub ci_run: Option<u64>,
    #[serde(default)]
    pub build_runs: BTreeMap<String, u64>,
    #[serde(default)]
    pub draft_id: Option<u64>,
    #[serde(default)]
    pub seal_sha256: Option<String>,
    /// When the attempt was admitted: the timing identity survives a lost
    /// journal (design 7.12).
    #[serde(default)]
    pub started: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

impl ClaimDoc {
    pub fn from_journal(j: &Journal, previous: Vec<Owner>) -> ClaimDoc {
        ClaimDoc {
            schema: 1,
            repository_id: j.repository_id,
            version: j.version.clone(),
            mode: j.mode().into(),
            commit: j.commit.clone(),
            tree: j.tree.clone(),
            base_main: j.base_main.clone(),
            plan_sha256: j.plan_sha256.clone(),
            policy_sha256: j.policy_sha256.clone(),
            generation: j.generation,
            operation: j.operation.clone(),
            nonce: j.nonce.clone(),
            push_ref: j.push_ref.clone(),
            owner: Owner {
                id: j.owner.clone(),
                host: host_name(),
                pid: std::process::id(),
            },
            previous_owners: previous,
            ci_run: j.ci.dispatch.run_id,
            build_runs: j
                .builds
                .iter()
                .filter_map(|(a, b)| b.dispatch.run_id.map(|r| (a.clone(), r)))
                .collect(),
            draft_id: j.release.draft_id,
            seal_sha256: j.release.seal_sha256.clone(),
            started: Some(j.started),
            decided_at: j.decided_at,
        }
    }
}

/// `SHA256SUMS`: `<sha256>  <name>` per binary asset, sorted by name.
pub fn sha256sums(assets: &BTreeMap<String, String>) -> String {
    assets
        .iter()
        .map(|(name, sha)| format!("{sha}  {name}\n"))
        .collect()
}

pub fn parse_sha256sums(text: &str) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (sha, name) = line.split_once("  ")?;
        if !super::util::is_sha256(sha) || out.insert(name.to_string(), sha.to_string()).is_some() {
            return None;
        }
    }
    Some(out)
}

/// `release-evidence.json` (schema 1). Only executed, verified obligations
/// appear; every required gate id is listed with an explicit outcome.
pub fn release_evidence(j: &Journal) -> serde_json::Value {
    let assets: Vec<_> = j
        .builds
        .iter()
        .map(|(name, b)| {
            serde_json::json!({
                "name": name,
                "sha256": b.sha256,
                "size": b.size,
                "build": {"run_id": b.dispatch.run_id, "run_attempt": b.run_attempt,
                          "artifact_id": b.artifact_id, "build_json_sha256": b.build_json_sha256},
                "attestation": b.attestation,
                "kit": b.kit,
                "validated": b.validated,
            })
        })
        .collect();
    let mut gates = BTreeMap::new();
    gates.insert("ci.release".to_string(), "passed");
    for name in j.builds.keys() {
        gates.insert(format!("build.{name}"), "passed");
        gates.insert(format!("smoke.{name}"), "passed");
    }
    for (id, n) in &j.native {
        gates.insert(
            id.clone(),
            if n.outcome == "passed" {
                "passed"
            } else {
                "failed"
            },
        );
    }
    serde_json::json!({
        "schema": 1,
        "version": j.version,
        "repository": j.repo,
        "repository_id": j.repository_id,
        "commit": j.commit,
        "tree": j.tree,
        "manifest_sha256": j.manifest_sha256,
        "plan_sha256": j.plan_sha256,
        "policy_sha256": j.policy_sha256,
        "ci": {"run_id": j.ci.dispatch.run_id, "run_attempt": j.ci.run_attempt, "reused": j.ci.reused,
               "verified_sha256": j.ci.verified_sha256, "completed": j.ci.completed},
        "started": j.started,
        "assets": assets,
        "native": j.native,
        "gates": gates,
    })
}

/// The immutable seal: hashes the frozen files; it never hashes itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Seal {
    pub schema: u32,
    pub version: String,
    pub repository_id: u64,
    pub commit: String,
    pub tree: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    pub release_id: u64,
    pub assets: Vec<SealedAsset>,
}

impl Seal {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = serde_json::to_vec_pretty(self).expect("seal serializes");
        b.push(b'\n');
        b
    }
    pub fn digest(&self) -> String {
        super::util::sha256(&self.bytes())
    }
}

/// The line the annotated tag carries; binds the tag to the seal.
pub fn tag_message(version: &str, seal_sha256: &str) -> String {
    format!("awman {version}\n\nawman-release-seal: sha256:{seal_sha256}\n")
}

pub fn tag_seal(message: &str) -> Option<String> {
    message
        .lines()
        .find_map(|l| l.strip_prefix("awman-release-seal: sha256:"))
        .map(|s| s.trim().to_string())
}

/// The marker in the draft body that identifies the owning attempt. It names
/// the attempt (operation key), not the controller, so an explicit takeover
/// keeps ownership of the same draft.
pub fn draft_marker(operation: &str) -> String {
    format!("<!-- awman-release operation={operation} -->")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub schema: u32,
    pub outcome: String,
    pub version: String,
    pub release_id: u64,
    pub tag: String,
    pub commit: String,
    pub seal_sha256: String,
    pub assets: Vec<ReceiptAsset>,
    pub verified_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReceiptAsset {
    pub name: String,
    pub url: String,
    pub sha256: String,
}

/// A receipt proves verification only when its bytes are the recorded ones
/// and it names this release's version, release ID, commit, seal and every
/// sealed file. Returns the first reason it does not.
pub fn check_receipt(path: &std::path::Path, j: &Journal, seal: &Seal) -> Result<Receipt, String> {
    let bytes = std::fs::read(path).map_err(|_| "no verification receipt".to_string())?;
    if j.release.receipt_sha256.as_deref() != Some(super::util::sha256(&bytes).as_str()) {
        return Err("the receipt differs from the recorded receipt digest".into());
    }
    let r: Receipt =
        serde_json::from_slice(&bytes).map_err(|e| format!("the receipt is unreadable: {e}"))?;
    let mut names: Vec<_> = r.assets.iter().map(|a| (&a.name, &a.sha256)).collect();
    let mut want: Vec<_> = seal.assets.iter().map(|a| (&a.name, &a.sha256)).collect();
    names.sort();
    want.sort();
    let bound = r.schema == 1
        && r.outcome == "verified"
        && r.version == j.version
        && r.tag == j.tag_name()
        && r.commit == j.commit
        && Some(r.release_id) == j.release.draft_id
        && r.release_id == seal.release_id
        && r.seal_sha256 == seal.digest()
        && names == want;
    if !bound {
        return Err(
            "the receipt does not name this release, its seal and every sealed file".into(),
        );
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_round_trip_and_the_tag_carries_the_seal() {
        let assets = BTreeMap::from([
            ("b".to_string(), "1".repeat(64)),
            ("a".to_string(), "2".repeat(64)),
        ]);
        let text = sha256sums(&assets);
        assert!(text.starts_with(&"2".repeat(64)));
        assert_eq!(parse_sha256sums(&text).unwrap(), assets);
        assert!(parse_sha256sums("zz  a\n").is_none());
        let m = tag_message("v1.2.3", &"f".repeat(64));
        assert_eq!(tag_seal(&m).unwrap(), "f".repeat(64));
        assert_eq!(tag_seal("awman v1.2.3"), None);
    }
}
