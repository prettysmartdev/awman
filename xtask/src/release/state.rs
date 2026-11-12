//! Release state shared by the operator flow and the pipeline commands.
//!
//! Owner: workflow step `candidate-promote-pipeline`, which implements
//! [`query`]. The public types and signatures in this file are a fixed
//! contract (WI 0144 section 5.1): `release-operator-contract` codes against
//! them, so change them only through the integration step.
//!
//! State is always derived from GitHub (the draft or published release, its
//! `release-state.json` asset, tags and workflow runs), never from files in
//! the local checkout, so a release interrupted on one machine can be resumed
//! from another.

// The contract is declared before either side uses it.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

pub mod args;
pub mod decide;
pub mod hub;
pub mod inputs;
pub mod ops;
pub mod record;

/// Where a version is in `prepared → candidate-built → validated → promoted →
/// verified`. `Abandoned` can follow any state before `Promoted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseState {
    /// No candidate exists for this version on GitHub.
    None,
    /// A candidate run has started and has not yet uploaded every artifact.
    Prepared,
    CandidateBuilt,
    Validated,
    Promoted,
    Verified,
    Abandoned,
}

/// What `release status` prints and what `release run` resumes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseStatus {
    /// `vX.Y.Z`.
    pub version: String,
    pub state: ReleaseState,
    /// Candidate number for this version, starting at 1. Internal: the
    /// operator never types it.
    pub candidate: Option<u32>,
    /// Commit the candidate was built from.
    pub commit: Option<String>,
    /// Gate-manifest version the candidate's evidence was produced under.
    pub manifest_version: Option<String>,
    /// Id of the newest candidate, promotion or verification workflow run.
    pub run_id: Option<u64>,
    /// Required gate ids that are failed or blocked, with the reason.
    pub failing_gates: Vec<FailingGate>,
    /// URLs of the evidence bundle and the workflow run summaries.
    pub evidence_links: Vec<String>,
    /// Whether the final `vX.Y.Z` tag exists on origin.
    pub tag_on_origin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailingGate {
    pub gate_id: String,
    /// `failed` (product) or `blocked` (infrastructure).
    pub outcome: String,
    pub reason: String,
}

/// Read the release state of `version` (`vX.Y.Z`) in `repo` (`owner/name`)
/// from GitHub through the `gh` CLI on `PATH`.
pub fn query(repo: &str, version: &str) -> anyhow::Result<ReleaseStatus> {
    query_in(&hub::GhHub::new(repo), version)
}

/// [`query`] against any store. Errors are returned, never mapped to `None`.
pub fn query_in(hub: &dyn hub::Hub, version: &str) -> anyhow::Result<ReleaseStatus> {
    anyhow::ensure!(
        record::valid_version(version),
        "version must be vX.Y.Z, got {version}"
    );
    Ok(decide::derive(&facts(hub, version)?))
}

/// A private scratch directory for downloads (removed on drop).
pub struct Scratch(pub std::path::PathBuf);
impl Scratch {
    pub fn new(what: &str) -> anyhow::Result<Scratch> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "awman-release-{what}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Download and parse a release's `release-state.json`, if it has one.
pub fn read_record(
    hub: &dyn hub::Hub,
    release: &hub::ReleaseFact,
) -> anyhow::Result<Option<record::ReleaseRecord>> {
    let Some(asset) = release.asset(record::STATE_ASSET) else {
        return Ok(None);
    };
    let tmp = Scratch::new("state")?;
    let path = tmp.0.join(record::STATE_ASSET);
    hub.download_asset(asset.id, &path)?;
    Ok(Some(args::read_json(&path)?))
}

/// Every release of any namespace whose tag ends in `version`, with records.
pub fn existing(hub: &dyn hub::Hub, tag: &str) -> anyhow::Result<Vec<decide::Existing>> {
    let mut out = Vec::new();
    for release in hub.releases()?.into_iter().filter(|r| r.tag == tag) {
        let record = read_record(hub, &release)?;
        out.push(decide::Existing { release, record });
    }
    Ok(out)
}

/// Highest candidate number of a retained record artifact.
pub fn recorded(hub: &dyn hub::Hub, version: &str, namespace: &str) -> anyhow::Result<Vec<u32>> {
    let prefix = record::record_artifact(version, namespace, 0);
    let prefix = prefix.trim_end_matches('0');
    Ok(hub
        .artifacts(prefix)?
        .iter()
        .filter_map(|a| a.name.strip_prefix(prefix)?.parse().ok())
        .collect())
}

/// Gather everything [`decide::derive`] needs from the store.
pub fn facts(hub: &dyn hub::Hub, version: &str) -> anyhow::Result<decide::Facts> {
    let existing = existing(hub, version)?;
    let tag = hub.tag(version)?;
    let runs = hub.runs("candidate.yml")?;
    let recorded = recorded(hub, version, record::PRODUCTION)?;
    let mut receipt = None;
    let promote_runs = hub.runs("promote.yml")?;
    let trusted = |id: u64| {
        runs.iter()
            .chain(&promote_runs)
            .any(|r| r.id == id && r.head_branch == "main")
    };
    let name = record::receipt_artifact(version);
    if let Some(a) = hub
        .artifacts(&name)?
        .into_iter()
        .filter(|a| a.name == name && a.head_branch == "main" && trusted(a.run_id))
        .max_by_key(|a| a.id)
    {
        let tmp = Scratch::new("receipt")?;
        hub.download_artifact(&a, &tmp.0)?;
        receipt = args::read_json(&tmp.0.join("receipt.json")).ok();
    }
    Ok(decide::Facts {
        version: version.into(),
        existing,
        tag,
        runs,
        recorded,
        receipt,
    })
}
