//! Private attempt state (design 3.5, 7.4–7.6): `state.json`, the local
//! process lock and the timing log.
//!
//! Location: `$(git rev-parse --git-common-dir)/awman-release/<version>/`, or
//! `$AWMAN_RELEASE_STATE/<version>/`. Private to the clone (0700/0600), never
//! committed, survives `cargo clean` and `git clean`. The journal is a cache,
//! never proof: identity is recovered from the remote claim and the sealed
//! draft, and every completed phase is re-probed before it is skipped.

use super::error::{Step, Stop};
use super::util::{private_dir, read_json, write_atomic, write_json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SCHEMA: u32 = 1;

/// Phases in order (design 3.1). `pin` … `confirm` are reconcilable; `land`,
/// `tag` and `publish` are the only irreversible steps.
pub const PHASES: &[&str] = &[
    "preflight",
    "pin",
    "push",
    "dispatch",
    "collect",
    "native",
    "seal",
    "confirm",
    "land",
    "tag",
    "publish",
    "verify",
    "cleanup",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ErrorRecord {
    pub code: String,
    pub message: String,
    pub remedy: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PhaseRecord {
    pub status: Status,
    #[serde(default)]
    pub started: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished: Option<DateTime<Utc>>,
    #[serde(default)]
    pub error: Option<ErrorRecord>,
}

/// Durable dispatch intent (design 7.6): written as `pending` before the
/// network call, `acked` when the service accepted it, and pinned to one run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Dispatch {
    pub key: String,
    #[serde(default)]
    pub pending_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub acked: bool,
    #[serde(default)]
    pub run_id: Option<u64>,
    /// Other runs found with the same key; never mixed into the seal.
    #[serde(default)]
    pub duplicates: Vec<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CiRecord {
    pub dispatch: Dispatch,
    #[serde(default)]
    pub reused: bool,
    #[serde(default)]
    pub run_attempt: u32,
    #[serde(default)]
    pub auto_retried: bool,
    #[serde(default)]
    pub verified_sha256: Option<String>,
    #[serde(default)]
    pub completed: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Attestation {
    pub subject_sha256: String,
    pub signer_workflow: String,
    pub source_digest: String,
    pub hosted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KitRecord {
    pub artifact_id: u64,
    /// Attestation subject: the complete tar archive.
    pub sha256: String,
    /// Extracted kit.json, binding runtime files and native result metadata.
    pub metadata_sha256: String,
    pub attestation: Attestation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildRecord {
    pub dispatch: Dispatch,
    #[serde(default)]
    pub run_attempt: u32,
    #[serde(default)]
    pub auto_retried: bool,
    /// Immutable service artifact ID of `release-asset-<asset>`.
    #[serde(default)]
    pub artifact_id: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub attestation: Option<Attestation>,
    #[serde(default)]
    pub build_json_sha256: Option<String>,
    #[serde(default)]
    pub kit: Option<KitRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_kits: BTreeMap<String, KitRecord>,
    #[serde(default)]
    pub validated: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NativeRecord {
    pub outcome: String,
    pub result_sha256: String,
    pub asset_sha256: String,
    pub kit_sha256: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub finished: Option<DateTime<Utc>>,
    #[serde(default)]
    pub host: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sbx: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SealedAsset {
    pub name: String,
    pub asset_id: u64,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReleaseRecord {
    #[serde(default)]
    pub draft_id: Option<u64>,
    #[serde(default)]
    pub seal_sha256: Option<String>,
    #[serde(default)]
    pub confirmed_seal: Option<String>,
    #[serde(default)]
    pub landed_main: Option<String>,
    /// A main tip whose publication policy was read and found unchanged.
    #[serde(default)]
    pub policy_checked_main: Option<String>,
    #[serde(default)]
    pub tag: bool,
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub receipt_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Journal {
    pub schema: u32,
    pub version: String,
    pub repo: String,
    pub repository_id: u64,
    pub rehearsal: bool,
    /// `REHEARSAL_REPO`: a complete publication rehearsal in this
    /// explicitly allowlisted disposable repository (design 7.13).
    #[serde(default)]
    pub disposable_repo: Option<String>,
    /// Random per attempt; names the rehearsal branch and draft.
    pub nonce: String,
    pub commit: String,
    pub tree: String,
    pub base_main: String,
    /// `refs/heads/release/vX.Y.Z`, `refs/heads/rehearsal/<nonce>` or empty
    /// when R is already on main.
    pub push_ref: String,
    pub generation: u32,
    /// `sha256(repo-id ‖ mode ‖ version ‖ R ‖ plan ‖ generation)`.
    pub operation: String,
    pub owner: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    pub manifest_sha256: String,
    pub started: DateTime<Utc>,
    /// When the release decision/preparation began, if the operator gave
    /// it (`DECIDED_AT`); the hour target counts from here (design 7.12).
    #[serde(default)]
    pub decided_at: Option<DateTime<Utc>>,
    /// One record per controller invocation, kept across interruptions.
    #[serde(default)]
    pub invocations: Vec<Invocation>,
    #[serde(default)]
    pub claim_oid: Option<String>,
    #[serde(default)]
    pub phases: BTreeMap<String, PhaseRecord>,
    #[serde(default)]
    pub ci: CiRecord,
    #[serde(default)]
    pub builds: BTreeMap<String, BuildRecord>,
    #[serde(default)]
    pub native: BTreeMap<String, NativeRecord>,
    #[serde(default)]
    pub release: ReleaseRecord,
}

impl Journal {
    pub fn done(&self, phase: &str) -> bool {
        self.phases
            .get(phase)
            .is_some_and(|p| p.status == Status::Done)
    }

    pub fn mode(&self) -> &'static str {
        if self.rehearsal {
            "rehearsal"
        } else if self.disposable_repo.is_some() {
            "disposable"
        } else {
            "production"
        }
    }

    /// The tag (and release) name. A disposable-repository rehearsal only
    /// ever writes the `rehearsal-*` namespace, never a product version.
    pub fn tag_name(&self) -> String {
        if self.disposable_repo.is_some() {
            format!("rehearsal-{}-{}", self.version, self.nonce)
        } else {
            self.version.clone()
        }
    }

    /// Display abbreviation of the operation key (run names, branch names).
    pub fn short(&self) -> &str {
        &self.operation[..12]
    }

    pub fn key(&self, what: &str) -> String {
        format!("{what}-{}", self.short())
    }
}

/// `sha256(repo-id ‖ mode ‖ version ‖ R ‖ plan ‖ generation)` (design 3.3).
pub fn operation_key(
    repository_id: u64,
    rehearsal: bool,
    version: &str,
    commit: &str,
    plan_sha256: &str,
    generation: u32,
) -> String {
    let mode = if rehearsal { "rehearsal" } else { "production" };
    super::util::sha256(
        format!("{repository_id}\0{mode}\0{version}\0{commit}\0{plan_sha256}\0{generation}")
            .as_bytes(),
    )
}

/// The private state directories of one clone.
#[derive(Debug, Clone)]
pub struct StateRoot {
    pub root: PathBuf,
}

impl StateRoot {
    pub fn new(root: PathBuf) -> Step<StateRoot> {
        private_dir(&root).map_err(|e| Stop::new("state-corrupt", format!("{e:#}")))?;
        Ok(StateRoot { root })
    }

    /// Stable per-clone owner ID. Losing it loses ownership: a recovered
    /// journal can rebuild identity, never invent ownership (design 7.4).
    pub fn owner_id(&self) -> Step<String> {
        let path = self.root.join("owner-id");
        if let Ok(text) = std::fs::read_to_string(&path) {
            let id = text.trim().to_string();
            if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Ok(id);
            }
            return Err(Stop::new(
                "state-corrupt",
                format!("{} is malformed", path.display()),
            ));
        }
        let id = super::util::random_id()?;
        write_atomic(&path, format!("{id}\n").as_bytes())?;
        Ok(id)
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.root.join(version)
    }
}

pub struct Store {
    pub dir: PathBuf,
    #[cfg(unix)]
    _lock: Option<nix::fcntl::Flock<std::fs::File>>,
    #[cfg(windows)]
    _lock: Option<std::fs::File>,
}

impl Store {
    /// Open (creating) the version directory and take the exclusive process
    /// lock. A held lock is `attempt-locked` naming the holder.
    pub fn open(dir: &Path) -> Step<Store> {
        private_dir(dir).map_err(|e| Stop::new("state-corrupt", format!("{e:#}")))?;
        for sub in [
            "assets", "kits", "native", "evidence", "logs", "ci", "fixtures",
        ] {
            private_dir(&dir.join(sub))
                .map_err(|e| Stop::new("state-corrupt", format!("{e:#}")))?;
        }
        let path = dir.join("lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|e| Stop::new("state-corrupt", format!("{}: {e}", path.display())))?;
        #[cfg(unix)]
        let lock = match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        {
            Ok(lock) => lock,
            Err(_) => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                return Err(Stop::new(
                    "attempt-locked",
                    format!(
                        "another release process holds {} ({})",
                        path.display(),
                        holder.trim()
                    ),
                ));
            }
        };
        #[cfg(windows)]
        let lock = {
            let holder_path = dir.join("lock-holder");
            if file.try_lock().is_err() {
                // Windows locks can prevent reading the locked file itself.
                let holder = std::fs::read_to_string(&holder_path).unwrap_or_default();
                return Err(Stop::new(
                    "attempt-locked",
                    format!(
                        "another release process holds {} ({})",
                        path.display(),
                        holder.trim()
                    ),
                ));
            }
            file
        };
        // Holder identity for the message above; the flock itself is the
        // proof and is released by the kernel when the process dies.
        #[cfg(unix)]
        let holder_path = &path;
        #[cfg(windows)]
        let holder_path = &dir.join("lock-holder");
        std::fs::write(
            holder_path,
            format!(
                "pid {} since {}\n",
                std::process::id(),
                Utc::now().to_rfc3339()
            ),
        )
        .ok();
        Ok(Store {
            dir: dir.to_path_buf(),
            _lock: Some(lock),
        })
    }

    /// Read-only view without the lock (status).
    pub fn view(dir: &Path) -> Store {
        Store {
            dir: dir.to_path_buf(),
            _lock: None,
        }
    }

    pub fn journal_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn load(&self) -> Step<Option<Journal>> {
        let path = self.journal_path();
        if !path.exists() {
            return Ok(None);
        }
        let j: Journal =
            read_json(&path).map_err(|e| Stop::new("state-corrupt", format!("{e:#}")))?;
        if j.schema != SCHEMA {
            return Err(Stop::new(
                "state-corrupt",
                format!("{} has unsupported schema {}", path.display(), j.schema),
            ));
        }
        Ok(Some(j))
    }

    pub fn save(&self, j: &Journal) -> Step {
        write_json(&self.journal_path(), j)
            .map_err(|e| Stop::new("state-corrupt", format!("{e:#}")))
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// Append one timing event (design 7.12). Wall-clock timestamps survive
    /// interruptions, so a resumed attempt keeps its total elapsed time.
    pub fn timing(&self, now: DateTime<Utc>, phase: &str, event: &str, detail: &str) {
        use std::io::Write;
        let line = serde_json::json!({"at": now.to_rfc3339(), "phase": phase, "event": event, "detail": detail});
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("timings.jsonl"))
        {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn log(&self, phase: &str, text: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("logs").join(format!("{phase}.log")))
        {
            let _ = writeln!(f, "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_keys_separate_mode_source_plan_and_generation() {
        let base = operation_key(1, false, "v1.0.0", "a", "p", 1);
        for other in [
            operation_key(2, false, "v1.0.0", "a", "p", 1),
            operation_key(1, true, "v1.0.0", "a", "p", 1),
            operation_key(1, false, "v1.0.1", "a", "p", 1),
            operation_key(1, false, "v1.0.0", "b", "p", 1),
            operation_key(1, false, "v1.0.0", "a", "q", 1),
            operation_key(1, false, "v1.0.0", "a", "p", 2),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn a_second_process_lock_is_refused_with_the_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v1.0.0");
        let first = Store::open(&dir).unwrap();
        let err = Store::open(&dir).err().unwrap();
        assert_eq!(err.code, "attempt-locked");
        assert!(err.message.contains("pid"));
        drop(first);
        Store::open(&dir).unwrap();
    }
}

/// One controller invocation: its wall-clock start, monotonic duration and
/// how it ended. Interrupted and failed invocations stay in the ledger.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Invocation {
    pub started: DateTime<Utc>,
    pub monotonic_seconds: f64,
    pub outcome: String,
}

/// The acceptance boundary of one release (design 7.12).
pub const TARGET_MINUTES: i64 = 60;

impl Journal {
    /// The start of the hour being measured: the recorded decision time,
    /// otherwise admission of the attempt.
    pub fn clock_start(&self) -> DateTime<Utc> {
        self.decided_at.unwrap_or(self.started)
    }
    /// Minutes since `clock_start` and whether the hour target is missed.
    pub fn target(&self, now: DateTime<Utc>) -> (i64, bool) {
        let minutes = (now - self.clock_start()).num_minutes();
        (minutes, minutes > TARGET_MINUTES)
    }
}
