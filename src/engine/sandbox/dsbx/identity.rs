//! Host-side identity record of awman-owned sandboxes (contract §8).
//!
//! `<data home>/sbx/sandboxes/<name>.json`, mode 0600, written atomically.
//! It records which kit and composition awman created a sandbox from, so a
//! later launch reuses the sandbox only when it would get the same thing.
//! The guest-side check (`awman-apply` comparing the baked `kit-id`) is the
//! authority for the kit; this record additionally catches a changed
//! disposition, workload, handle or workspace, and a creation that never
//! finished.
//!
//! The record holds no secret and no environment value.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::agent::sbx_contract::{
    sha256_hex, EmittedKit, SbxCliVersion, SbxDisposition, SbxRoute, APPLY_PROTOCOL,
};
use crate::engine::error::EngineError;

pub(super) const RECORD_SCHEMA: u32 = 1;
pub(super) const DISPOSITION_PUBLISHED: &str = "published-workload-with-mixin";
pub(super) const DISPOSITION_OWNED: &str = "owned-workload";

/// Creation progress. A record left in `Creating` means `sbx create` was
/// started but awman never saw it finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum RecordState {
    Creating,
    Created,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SandboxRecord {
    pub schema: u32,
    pub state: RecordState,
    pub sandbox: String,
    pub agent: String,
    pub disposition: String,
    pub workload: Option<String>,
    pub kit_handle: String,
    pub kit_digest: String,
    pub apply_protocol: u32,
    pub workspace: String,
    pub sbx_cli: String,
    pub created_at: String,
    /// SHA-256 of `sbx inspect --json` right after creation. Evidence of
    /// what `latest` resolved to; not a match input.
    pub inspect_sha256: Option<String>,
    /// The first `kits` value in that inspect output. Evidence only.
    pub kits: Option<serde_json::Value>,
}

/// What this launch intends to run, compared against an existing record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IntendedSandbox {
    pub sandbox: String,
    pub agent: String,
    pub disposition: &'static str,
    pub workload: Option<String>,
    pub kit_handle: String,
    pub kit_digest: String,
    pub workspace: String,
}

impl IntendedSandbox {
    pub(super) fn new(
        sandbox: &str,
        route: &SbxRoute,
        kit: &EmittedKit,
        workspace: &Path,
    ) -> Result<Self, EngineError> {
        let (disposition, workload) = match route.disposition {
            SbxDisposition::PublishedWorkloadWithMixin { workload } => {
                (DISPOSITION_PUBLISHED, Some(workload.to_string()))
            }
            SbxDisposition::OwnedWorkload => (DISPOSITION_OWNED, None),
            SbxDisposition::Refused { reason } => {
                return Err(EngineError::Sandbox(format!(
                    "agent '{}' {reason}",
                    route.agent
                )))
            }
        };
        Ok(Self {
            sandbox: sandbox.to_string(),
            agent: route.agent.to_string(),
            disposition,
            workload,
            kit_handle: kit.manifest.handle.clone(),
            kit_digest: kit.digest.clone(),
            workspace: workspace.display().to_string(),
        })
    }

    /// The record written just before `sbx create` runs.
    pub(super) fn creating_record(&self, cli: SbxCliVersion) -> SandboxRecord {
        SandboxRecord {
            schema: RECORD_SCHEMA,
            state: RecordState::Creating,
            sandbox: self.sandbox.clone(),
            agent: self.agent.clone(),
            disposition: self.disposition.to_string(),
            workload: self.workload.clone(),
            kit_handle: self.kit_handle.clone(),
            kit_digest: self.kit_digest.clone(),
            apply_protocol: APPLY_PROTOCOL,
            workspace: self.workspace.clone(),
            sbx_cli: cli.to_string(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            inspect_sha256: None,
            kits: None,
        }
    }
}

impl SandboxRecord {
    /// Mark creation complete, recording the inspect output as evidence.
    pub(super) fn created(mut self, inspect_stdout: Option<&str>) -> Self {
        self.state = RecordState::Created;
        self.inspect_sha256 = inspect_stdout.map(|s| sha256_hex(s.as_bytes()));
        self.kits = inspect_stdout.and_then(super::cli::inspect_kits);
        self
    }
}

/// The launch decision of contract §8's match table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReuseDecision {
    /// No sandbox of this name exists: create it. `discard_record` is true
    /// when an old record must be replaced first.
    Create { discard_record: bool },
    /// Every recorded field matches; reuse (the guest check still runs).
    Reuse,
    /// Do not touch the sandbox; report `reason`.
    Stale { reason: String },
}

/// Evaluate the §8 table. `record` is the host record for this name, if any.
pub(super) fn decide(
    exists: bool,
    record: Option<&SandboxRecord>,
    intended: &IntendedSandbox,
) -> ReuseDecision {
    if !exists {
        return ReuseDecision::Create {
            discard_record: record.is_some(),
        };
    }
    let Some(record) = record else {
        return ReuseDecision::Stale {
            reason: "it was not created by this awman (it predates awman's v3 kits, or was \
                     made outside awman)"
                .into(),
        };
    };
    if record.state != RecordState::Created {
        return ReuseDecision::Stale {
            reason: "its creation did not complete".into(),
        };
    }
    let mismatches: Vec<String> =
        [
            ("agent", record.agent.as_str(), intended.agent.as_str()),
            (
                "disposition",
                record.disposition.as_str(),
                intended.disposition,
            ),
            (
                "workload",
                record.workload.as_deref().unwrap_or("none"),
                intended.workload.as_deref().unwrap_or("none"),
            ),
            (
                "kit",
                record.kit_handle.as_str(),
                intended.kit_handle.as_str(),
            ),
            (
                "workspace",
                record.workspace.as_str(),
                intended.workspace.as_str(),
            ),
        ]
        .into_iter()
        .filter(|(_, had, want)| had != want)
        .map(|(field, had, want)| format!("{field} '{had}' (this launch needs '{want}')"))
        .chain((record.apply_protocol != APPLY_PROTOCOL).then(|| {
            format!(
                "apply protocol {} (this awman speaks {APPLY_PROTOCOL})",
                record.apply_protocol
            )
        }))
        .chain((record.kit_digest != intended.kit_digest).then(|| {
            "kit contents (the awman kit changed since the sandbox was created)".to_string()
        }))
        .collect();
    if mismatches.is_empty() {
        ReuseDecision::Reuse
    } else {
        ReuseDecision::Stale {
            reason: format!("it was created with a different {}", mismatches.join(", ")),
        }
    }
}

/// The stale-sandbox refusal for `sandbox`.
pub(super) fn stale_error(sandbox: &str, reason: String, workspace: &Path) -> EngineError {
    EngineError::SbxSandboxStale {
        sandbox: sandbox.to_string(),
        reason,
        workspace: workspace.display().to_string(),
    }
}

/// Directory of identity records.
pub(super) fn records_dir(data_home: &Path) -> PathBuf {
    data_home.join("sbx").join("sandboxes")
}

fn record_path(data_home: &Path, sandbox: &str) -> PathBuf {
    records_dir(data_home).join(format!("{sandbox}.json"))
}

/// Read the record for `sandbox`. A missing file is `None`; an unreadable or
/// unparsable one is also `None`, which makes an existing sandbox stale
/// rather than trusted.
pub(super) fn load(data_home: &Path, sandbox: &str) -> Option<SandboxRecord> {
    let bytes = std::fs::read(record_path(data_home, sandbox)).ok()?;
    let record: SandboxRecord = serde_json::from_slice(&bytes).ok()?;
    (record.schema == RECORD_SCHEMA && record.sandbox == sandbox).then_some(record)
}

/// Write `record` atomically with mode 0600.
pub(super) fn store(data_home: &Path, record: &SandboxRecord) -> Result<(), EngineError> {
    let dir = records_dir(data_home);
    super::lock::create_private_dir(&dir)?;
    let path = record_path(data_home, &record.sandbox);
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| EngineError::Sandbox(format!("serialize sandbox record: {e}")))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        record.sandbox,
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        use std::io::Write;
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(EngineError::io(&path, e));
    }
    Ok(())
}

/// Remove the record for a sandbox that no longer exists.
pub(super) fn discard(data_home: &Path, sandbox: &str) -> Result<(), EngineError> {
    let path = record_path(data_home, sandbox);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(EngineError::io(&path, e)),
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn intended() -> IntendedSandbox {
        IntendedSandbox {
            sandbox: "awman-1a2b3c4d-claude".into(),
            agent: "claude".into(),
            disposition: DISPOSITION_PUBLISHED,
            workload: Some("claude".into()),
            kit_handle: "awman-claude-mixin".into(),
            kit_digest: "d1".repeat(32),
            workspace: "/repo".into(),
        }
    }

    #[test]
    fn reuse_only_when_every_recorded_field_matches() {
        let want = intended();
        let created = want
            .creating_record(SbxCliVersion::new(0, 54, 1))
            .created(Some(r#"{"kits":["docker.io/sbx/claude"]}"#));
        assert_eq!(
            created.kits,
            Some(serde_json::json!(["docker.io/sbx/claude"]))
        );
        assert_eq!(created.inspect_sha256.as_ref().map(String::len), Some(64));

        assert_eq!(
            decide(false, None, &want),
            ReuseDecision::Create {
                discard_record: false
            }
        );
        assert_eq!(
            decide(false, Some(&created), &want),
            ReuseDecision::Create {
                discard_record: true
            }
        );
        assert_eq!(decide(true, Some(&created), &want), ReuseDecision::Reuse);

        // A different sbx client is not a mismatch.
        let mut newer_cli = created.clone();
        newer_cli.sbx_cli = "0.60.0".into();
        assert_eq!(decide(true, Some(&newer_cli), &want), ReuseDecision::Reuse);

        let stale = |record: &SandboxRecord| match decide(true, Some(record), &want) {
            ReuseDecision::Stale { reason } => reason,
            other => panic!("{other:?}"),
        };
        // A pre-v3 or hand-made sandbox with the same deterministic name.
        assert!(
            matches!(decide(true, None, &want), ReuseDecision::Stale { reason } if reason.contains("not created by this awman"))
        );
        let creating = want.creating_record(SbxCliVersion::new(0, 54, 1));
        assert!(stale(&creating).contains("creation did not complete"));
        let mut changed_kit = created.clone();
        changed_kit.kit_digest = "ff".repeat(32);
        assert!(stale(&changed_kit).contains("kit contents"));
        let mut changed_kind = created.clone();
        changed_kind.disposition = DISPOSITION_OWNED.into();
        changed_kind.workload = None;
        let reason = stale(&changed_kind);
        assert!(reason.contains("disposition"), "{reason}");
        assert!(reason.contains("workload"), "{reason}");
        let mut moved = created.clone();
        moved.workspace = "/elsewhere".into();
        assert!(stale(&moved).contains("workspace '/elsewhere'"));
        let mut protocol = created;
        protocol.apply_protocol = 1;
        assert!(stale(&protocol).contains("apply protocol 1"));
    }

    #[test]
    fn records_round_trip_privately_and_hold_no_environment() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(
            "engine::sandbox::dsbx::identity::identity_tests::records_round_trip_privately_and_hold_no_environment",
        ));
        let home = sandbox.awman_home();
        let want = intended();
        assert_eq!(load(&home, &want.sandbox), None);
        let record = want
            .creating_record(SbxCliVersion::new(0, 54, 1))
            .created(None);
        store(&home, &record).unwrap();
        assert_eq!(load(&home, &want.sandbox), Some(record.clone()));
        let path = records_dir(&home).join(format!("{}.json", want.sandbox));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&text)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut keys = keys;
        keys.sort();
        assert_eq!(
            keys,
            [
                "agent",
                "apply_protocol",
                "created_at",
                "disposition",
                "inspect_sha256",
                "kit_digest",
                "kit_handle",
                "kits",
                "sandbox",
                "sbx_cli",
                "schema",
                "state",
                "workload",
                "workspace"
            ]
        );
        // A corrupt record is not trusted.
        std::fs::write(&path, "{").unwrap();
        assert_eq!(load(&home, &want.sandbox), None);
        discard(&home, &want.sandbox).unwrap();
        discard(&home, &want.sandbox).unwrap();
        sandbox.finish();
    }
}
