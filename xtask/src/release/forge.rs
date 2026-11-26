//! Every GitHub operation the release uses, behind one trait so the same
//! production driver runs against `GhForge` (the operator's `gh` login) or
//! `DirForge` (a directory with fault injection, `forge_dir.rs`).
//!
//! Errors are already classified `Stop`s: 401/403 → `gh-auth`, transport →
//! `network`, everything else is decided by the caller from the typed result.

use super::error::Step;
use super::journal::Attestation;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub id: u64,
    pub full_name: String,
    pub default_branch: String,
    pub can_push: bool,
}

/// Outcome of a create-only or fast-forward-only ref write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefWrite {
    Done(String),
    /// Exists / not a fast-forward; carries the current target if known.
    Rejected(Option<String>),
}

/// `compare(base, head)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ancestry {
    Identical,
    /// head is ahead of base: base is an ancestor of head.
    Ahead,
    /// head is behind base: head is an ancestor of base.
    Behind,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    Merged(String),
    UpToDate,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub id: u64,
    pub workflow: String,
    pub title: String,
    pub head_sha: String,
    pub event: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub attempt: u32,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
}

impl RunInfo {
    pub fn completed(&self) -> bool {
        self.status == "completed"
    }
    pub fn succeeded(&self) -> bool {
        self.completed() && self.conclusion.as_deref() == Some("success")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub attempt: u32,
    /// Names of steps whose conclusion was `failure`.
    pub failed_steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub id: u64,
    pub name: String,
    pub digest: Option<String>,
    pub expired: bool,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: u64,
    pub name: String,
    pub size: u64,
    /// `uploaded` when complete; anything else is an incomplete stub.
    pub state: String,
    pub digest: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub id: u64,
    pub tag: String,
    pub name: String,
    pub draft: bool,
    pub target: String,
    pub body: String,
    pub assets: Vec<AssetInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TagInfo {
    /// The commit the tag peels to.
    pub commit: String,
    pub annotated: bool,
    pub message: String,
}

/// The repository-wide owner claim: a create-only branch
/// `awman-release-state/<version>` whose commit holds inert JSON files.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub oid: String,
    pub files: BTreeMap<String, Vec<u8>>,
}

pub const CLAIM_PREFIX: &str = "heads/awman-release-state/";

pub trait Forge {
    fn repo(&self) -> Step<RepoInfo>;
    /// `heads/main`, `tags/v1.2.3` → target commit (tags not peeled).
    fn get_ref(&self, name: &str) -> Step<Option<String>>;
    fn compare(&self, base: &str, head: &str) -> Step<Ancestry>;
    /// Push a local commit to a branch without force.
    fn push(&self, commit: &str, branch_ref: &str) -> Step<RefWrite>;
    /// Fast-forward-only update of an existing branch.
    fn fast_forward(&self, branch_ref: &str, commit: &str) -> Step<RefWrite>;
    fn delete_branch(&self, branch_ref: &str) -> Step;
    /// Server-side merge of `head` into `branch`; never rewrites `head`.
    fn merge(&self, branch: &str, head: &str, message: &str) -> Step<Merge>;

    fn read_claim(&self, version: &str) -> Step<Option<Claim>>;
    /// Create (`parent == None`, create-only) or fast-forward the claim.
    fn write_claim(
        &self,
        version: &str,
        parent: Option<&str>,
        files: &BTreeMap<String, Vec<u8>>,
        message: &str,
    ) -> Step<RefWrite>;
    fn delete_claim(&self, version: &str) -> Step;

    /// Blob SHA of a workflow file at a ref, `None` when absent.
    fn workflow_revision(&self, workflow: &str, git_ref: &str) -> Step<Option<String>>;
    /// The workflow is registered and active on the default branch.
    fn workflow_active(&self, workflow: &str) -> Step<bool>;
    fn dispatch(&self, workflow: &str, git_ref: &str, inputs: &BTreeMap<String, String>) -> Step;
    fn runs(&self, workflow: &str, head_sha: &str) -> Step<Vec<RunInfo>>;
    /// The most recent run of `workflow` on `branch` (any commit); used only
    /// for advisory display, never as release evidence.
    fn latest_run(&self, workflow: &str, branch: &str) -> Step<Option<RunInfo>>;
    fn run(&self, id: u64) -> Step<RunInfo>;
    fn jobs(&self, run_id: u64) -> Step<Vec<JobInfo>>;
    fn rerun_failed(&self, run_id: u64) -> Step;
    fn cancel(&self, run_id: u64) -> Step;
    fn artifacts(&self, run_id: u64) -> Step<Vec<ArtifactInfo>>;
    /// Download and unpack one artifact of a run into `dest`.
    fn download_artifact(&self, run_id: u64, artifact: &ArtifactInfo, dest: &Path) -> Step;
    /// Verify a build-provenance attestation: signer workflow, source digest
    /// and hosted runner. A failure is `attestation-invalid`.
    fn verify_attestation(
        &self,
        file: &Path,
        signer_workflow: &str,
        source_digest: &str,
    ) -> Step<Attestation>;

    /// All releases including drafts.
    fn releases(&self) -> Step<Vec<ReleaseInfo>>;
    fn create_draft(&self, tag: &str, name: &str, target: &str, body: &str) -> Step<ReleaseInfo>;
    fn upload_asset(&self, release_id: u64, name: &str, file: &Path) -> Step<AssetInfo>;
    fn delete_asset(&self, asset_id: u64) -> Step;
    fn download_asset(&self, asset_id: u64, dest: &Path) -> Step;
    fn publish(&self, release_id: u64) -> Step;
    fn delete_release(&self, release_id: u64) -> Step;

    fn tag(&self, name: &str) -> Step<Option<TagInfo>>;
    /// Annotated tag, create-only.
    fn create_tag(&self, name: &str, commit: &str, message: &str) -> Step<RefWrite>;
    /// Anonymous download (no credential).
    fn download_public(&self, url: &str, dest: &Path) -> Step;
    /// Rules that would stop this login from fast-forwarding/merging
    /// `branch` or creating `tag`. Unknown policy is an error, never "none".
    fn blocking_rules(&self, branch: &str, tag: &str) -> Step<Vec<String>>;

    /// Fault-injection checkpoint between driver steps; a no-op for GitHub.
    fn checkpoint(&self, _op: &str) -> Step {
        Ok(())
    }
}
