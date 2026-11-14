//! Consistent sample facts for the pipeline decision tests.
use super::*;
use crate::evidence::schema::{Artifact, Durations, Inputs, ResultRecord, Run, Shard};
use crate::release::state::hub::AssetFact;
use crate::release::state::inputs::{self, CandidateInputs};
use crate::release::state::record::{ArtifactRecord, GateRecord, Producer, Runs};
use chrono::Duration;
use std::collections::BTreeMap;

pub const COMMIT: &str = "1111111111111111111111111111111111111111";

pub fn manifest() -> Manifest {
    crate::manifest::load(&crate::gates::root()).expect("repository manifest")
}

pub fn now() -> DateTime<Utc> {
    "2026-11-11T12:00:00Z".parse().unwrap()
}

pub fn inputs(m: &Manifest) -> CandidateInputs {
    CandidateInputs {
        schema: 1,
        version: "v0.14.0".into(),
        commit: COMMIT.into(),
        tree: Some("t".repeat(40)),
        manifest_version: m.version().into(),
        cargo_lock: "a".repeat(64),
        cargo_manifests: BTreeMap::from([("Cargo.toml".into(), "b".repeat(64))]),
        rust_toolchain_file: None,
        cargo_config: Some("c".repeat(64)),
        build_scripts: BTreeMap::new(),
        toolchain: "1.94.0".into(),
        payloads: BTreeMap::from([(
            "third_party/msb-payloads/manifest.toml".into(),
            "d".repeat(64),
        )]),
        fixtures: BTreeMap::from([("guest-oci-base".into(), Some("e".repeat(64)))]),
        previous_release: BTreeMap::from([("aarch64-apple-darwin".into(), Some("f".repeat(64)))]),
        entitlements_sha256: Some("1".repeat(64)),
        signing_script_sha256: Some("2".repeat(64)),
        signing_command: vec!["bash".into(), inputs::SIGN_SCRIPT.into(), "<asset>".into()],
        release_notes_sha256: Some("3".repeat(64)),
        targets: inputs::targets(m).unwrap(),
    }
}

pub fn record() -> ReleaseRecord {
    let m = manifest();
    let ins = inputs(&m);
    let artifacts = ins
        .targets
        .iter()
        .map(|t| ArtifactRecord {
            name: t.asset.clone(),
            target: t.triple.clone(),
            build: t.build.clone(),
            sha256: crate::evidence::digest::sha256(t.asset.as_bytes()),
            unsigned_sha256: crate::evidence::digest::sha256(format!("u{}", t.asset).as_bytes()),
            bytes: t.asset.len() as u64,
            toolchain: "rustc 1.94.0".into(),
            input_digest: ins.target_digest(&t.asset).unwrap(),
            attestation: Some("https://example.invalid/attestation".into()),
            entitlements_sha256: t.sign.then(|| "1".repeat(64)),
            signing_command: None,
            producer: Producer {
                run_id: 7,
                attempt: 1,
                job: "build".into(),
            },
            asset_id: None,
        })
        .collect();
    let gate = |id: &str| GateRecord {
        id: id.into(),
        verdict: "passed".into(),
        gate_evidence_sha256: format!("{id}-sha"),
        run_id: 7,
        commit: COMMIT.into(),
        manifest_version: m.version().into(),
        expires_at: now() + Duration::days(10),
        overrides_applied: Vec::new(),
    };
    let mut rec = ReleaseRecord {
        schema: 1,
        version: "v0.14.0".into(),
        candidate: 1,
        namespace: PRODUCTION.into(),
        repository_id: "42".into(),
        request_id: "r".into(),
        commit: COMMIT.into(),
        state: ReleaseState::Validated,
        manifest_version: m.version().into(),
        input_digest: ins.digest().unwrap(),
        candidate_digest: None,
        dry_run: false,
        inputs: ins,
        artifacts,
        gates: vec![gate("main"), gate("candidate")],
        failing: Vec::new(),
        operator_record: serde_json::json!({"local_tests": "skipped by operator"}),
        runs: Runs {
            candidate: 7,
            validation: Some(7),
            promote: None,
        },
        release_id: Some(500),
        created_at: now() - Duration::days(1),
        expires_at: now() + Duration::days(13),
    };
    rec.candidate_digest = Some(rec.compute_candidate_digest().unwrap());
    rec
}

pub fn release(rec: &ReleaseRecord) -> ReleaseFact {
    let mut assets: Vec<AssetFact> = rec
        .artifacts
        .iter()
        .enumerate()
        .map(|(i, a)| AssetFact {
            id: 600 + i as u64,
            name: a.name.clone(),
            size: a.bytes,
            digest: Some(format!("sha256:{}", a.sha256)),
        })
        .collect();
    for (i, n) in [STATE_ASSET, "SHA256SUMS"].iter().enumerate() {
        assets.push(AssetFact {
            id: 700 + i as u64,
            name: (*n).into(),
            size: 1,
            digest: None,
        });
    }
    ReleaseFact {
        id: 500,
        tag: rec.version.clone(),
        name: super::super::record::draft_name(&rec.version, rec.candidate),
        draft: true,
        target_commitish: rec.commit.clone(),
        body: run_marker(rec.runs.candidate),
        url: "https://example.invalid/release".into(),
        assets,
    }
}

pub fn current(rec: &ReleaseRecord) -> Current {
    Current {
        now: now(),
        main_contains_commit: true,
        policy: Policy::of(&rec.inputs),
    }
}

pub fn existing(rec: &ReleaseRecord) -> Existing {
    Existing {
        release: release(rec),
        record: Some(rec.clone()),
    }
}

/// A passing, authenticated record for one candidate cell, bound to the bytes.
pub fn cell(m: &Manifest, rec: &ReleaseRecord, cell: &crate::manifest::Cell) -> CellRecord {
    let suite = &m.suites[&cell.suite];
    let mut tests: Vec<String> = m
        .release_critical
        .iter()
        .filter(|s| s.suite == cell.suite)
        .flat_map(|s| s.tests.clone())
        .collect();
    if tests.is_empty() {
        tests.push(format!("awman-cli::{} {}::case", suite.tier, cell.suite));
    }
    let asset = m.builds[&cell.build].asset.clone();
    let artifacts = asset
        .and_then(|a| rec.artifact(&a))
        .map(|a| {
            vec![Artifact {
                name: a.name.clone(),
                sha256: a.sha256.clone(),
                bytes: a.bytes,
            }]
        })
        .unwrap_or_default();
    let results: Vec<ResultRecord> = tests
        .iter()
        .map(|t| ResultRecord {
            test: t.clone(),
            outcome: Outcome::Passed,
            duration_ms: 5,
            classification: None,
            reason: None,
            capabilities_lost: Vec::new(),
            assertions: vec!["contract".into()],
        })
        .collect();
    let mut evidence = Evidence {
        schema: 1,
        kind: "cell".into(),
        gate: "candidate".into(),
        cell: cell.id.clone(),
        suite: cell.suite.clone(),
        tier: suite.tier.clone(),
        backend: suite.backend.clone(),
        build: cell.build.clone(),
        platform: cell.platform.clone(),
        shard: Shard { index: 1, of: 1 },
        commit: rec.commit.clone(),
        tree: "t".repeat(40),
        manifest_version: rec.manifest_version.clone(),
        input_digest: "i".repeat(64),
        inputs: Inputs {
            schema: 1,
            source_tree: "t".into(),
            cargo_manifests: BTreeMap::new(),
            cargo_lock: "a".repeat(64),
            toolchain: "1.94.0".into(),
            tool_digests: BTreeMap::new(),
            rust_toolchain_file: None,
            cargo_config: "c".into(),
            build_scripts: BTreeMap::new(),
            payload_manifest: None,
            features: Vec::new(),
            target: "t".into(),
            profile: "test".into(),
            runner_image: "img".into(),
            fixtures: BTreeMap::new(),
            previous_release: None,
            signing: None,
            release_notes: None,
        },
        expected_set_sha256: "a".repeat(64),
        archive_sha256: "b".repeat(64),
        plan_sha256: rec.compute_candidate_digest().unwrap(),
        artifacts,
        counts: crate::evidence::junit::counts(results.len(), &results),
        results,
        release_critical: Vec::new(),
        outcome: Outcome::Passed,
        classification: None,
        durations: Durations::default(),
        cache: None,
        reproduce: "cargo xtask test run".into(),
        run: Run {
            provider: "github-actions".into(),
            repository_id: "42".into(),
            workflow_path: ".github/workflows/candidate.yml".into(),
            workflow_revision: "w".repeat(40),
            run_id: 7,
            attempt: 1,
            job: "artifact".into(),
            job_id: 9,
            artifact_id: 10,
            artifact_service_digest: "c".repeat(64),
            url: "u".into(),
        },
        started_at: now() - Duration::hours(1),
        finished_at: now() - Duration::minutes(30),
        expires_at: now() + Duration::days(13),
        command_exit: 0,
        phases: ["setup", "run", "sweep", "upload"]
            .iter()
            .map(|p| (p.to_string(), Outcome::Passed))
            .collect(),
        capabilities_probed: m.tiers[&suite.tier]
            .guarantees
            .iter()
            .chain(&suite.requires)
            .cloned()
            .collect(),
    };
    evidence.input_digest = crate::evidence::digest::input_digest(&evidence.inputs).unwrap();
    CellRecord {
        sha256: crate::evidence::digest::input_digest(&evidence).unwrap(),
        evidence,
        authenticated: true,
    }
}

pub fn all_cells(m: &Manifest, rec: &ReleaseRecord) -> Vec<CellRecord> {
    candidate_cells(m)
        .unwrap()
        .iter()
        .map(|c| cell(m, rec, c))
        .collect()
}

pub fn main_gate(rec: &ReleaseRecord) -> MainGate {
    MainGate {
        evidence: GateEvidence {
            schema: 1,
            kind: "gate".into(),
            gate: "main".into(),
            commit: rec.commit.clone(),
            manifest_version: rec.manifest_version.clone(),
            plan_sha256: "p".into(),
            verdict: Outcome::Passed,
            cells: Vec::new(),
            missing: Vec::new(),
            errors: Vec::new(),
            release_critical: Vec::new(),
            quarantine: Vec::new(),
            overrides_applied: Vec::new(),
            queue_ms: 0,
            execution_ms: 0,
            p95_budget_minutes: None,
            generated_at: now(),
            expires_at: Some(now() + Duration::days(12)),
        },
        sha256: "main-sha".into(),
        authenticated: true,
    }
}
