use super::{digest, schema::*};
use crate::{
    gates::plan,
    manifest::{self, Manifest},
};
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Reconstruct from independently acquired event/diff metadata. Downloaded plan
/// is a consistency check, never the coverage authority.
pub fn aggregate(
    m: &Manifest,
    downloaded_plan: &plan::Plan,
    records: &[Evidence],
    ctx: &TrustedContext,
) -> Result<GateEvidence> {
    let authoritative = plan::create(
        m,
        &downloaded_plan.gate,
        ctx.request.clone(),
        ctx.now.date_naive(),
    )?;
    ensure!(
        &authoritative == downloaded_plan,
        "downloaded plan disagrees with independently reconstructed manifest/event plan"
    );
    let plan_digest = authoritative.digest()?;
    ensure!(
        ctx.plan_sha256 == plan_digest,
        "trusted plan digest mismatch"
    );
    let mut gate = GateEvidence {
        schema: 1,
        kind: "gate".into(),
        gate: authoritative.gate.clone(),
        commit: ctx.request.head_sha.clone(),
        manifest_version: m.version().into(),
        plan_sha256: plan_digest.clone(),
        verdict: Outcome::Passed,
        cells: Vec::new(),
        missing: Vec::new(),
        errors: Vec::new(),
        release_critical: Vec::new(),
        quarantine: m
            .quarantine
            .iter()
            .map(|q| format!("{} expires {}", q.test, q.expires))
            .collect(),
        overrides_applied: Vec::new(),
        queue_ms: 0,
        execution_ms: 0,
        p95_budget_minutes: m.gates[&authoritative.gate].budget_minutes,
        generated_at: ctx.now,
        expires_at: None,
    };
    for r in records {
        if !authoritative
            .cells
            .iter()
            .any(|c| c.selected && c.cell.id == r.cell)
        {
            gate.errors
                .push(format!("unknown/unselected evidence cell {}", r.cell));
        }
    }
    let mut seen_artifacts = BTreeSet::new();
    let mut execution_start: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut execution_end: Option<chrono::DateTime<chrono::Utc>> = None;
    // Every pin the gate's cells depend on; a pending one always blocks.
    gate.errors
        .extend(manifest::validate::pins_for_gate(m, &authoritative.gate));
    for job in ["plan", "lint", "build", "test"] {
        if ctx.jobs.get(job) != Some(&Outcome::Passed) {
            gate.errors.push(format!(
                "missing/failed required job {job} from service metadata"
            ));
        }
    }
    for pc in &authoritative.cells {
        if !pc.selected {
            continue;
        }
        let cell = &pc.cell;
        let rs: Vec<_> = records.iter().filter(|r| r.cell == cell.id).collect();
        if rs.is_empty() {
            gate.missing.push(cell.id.clone());
            continue;
        }
        let Some(expected) = ctx.expected.get(&cell.id) else {
            gate.errors
                .push(format!("{} missing independent expected set", cell.id));
            continue;
        };
        let expected_digest = digest::input_digest(expected)?;
        if let Err(e) = partitions(expected, pc.shards) {
            gate.errors.push(format!("{}: {e:#}", cell.id));
        }
        if expected.schema != 1
            || expected.build != cell.build
            || expected.cell != cell.id
            || expected.manifest_version != m.version()
            || expected.tests.is_empty()
        {
            gate.errors.push(format!(
                "{} invalid/empty independent expected set",
                cell.id
            ));
        }
        let mut ids = BTreeSet::new();
        let mut shard_ids = BTreeSet::new();
        let mut results = Vec::new();
        let mut hashes = Vec::new();
        let mut cell_errors = Vec::new();
        for r in rs {
            cell_errors.extend(record_findings(m, r));
            let hash = digest::input_digest(r)?;
            hashes.push(hash.clone());
            let producer = ctx
                .producers
                .iter()
                .find(|p| p.evidence_sha256 == hash && p.identity == r.run);
            match producer {
                Some(p)
                    if p.conclusion == Outcome::Passed && p.subject_sha256 == r.archive_sha256 => {}
                _ => cell_errors.push(
                    "missing authenticated successful producer job/artifact/attestation".into(),
                ),
            }
            if !seen_artifacts.insert((r.run.artifact_id, r.shard.index)) {
                cell_errors.push("duplicate producer artifact/shard".into());
            }
            if r.run.provider != "github-actions"
                || r.run.repository_id != ctx.request.repository_id
                || r.run.attempt == 0
                || r.run.run_id == 0
                || r.run.job_id == 0
                || r.run.artifact_id == 0
                || !digest::is_sha256(&r.run.artifact_service_digest)
            {
                cell_errors
                    .push("local/invalid producer cannot satisfy CI/release evidence".into());
            }
            if r.schema != 1
                || r.kind != "cell"
                || r.gate != authoritative.gate
                || r.commit != ctx.request.head_sha
                || r.manifest_version != m.version()
                || r.plan_sha256 != plan_digest
                || r.suite != cell.suite
                || r.build != cell.build
                || r.platform != cell.platform
            {
                cell_errors.push("stale or substituted evidence identity/plan/commit".into());
            }
            if r.finished_at < r.started_at
                || r.finished_at > ctx.now
                || r.expires_at <= ctx.now
                || r.expires_at
                    > r.finished_at + chrono::Duration::days(m.release.evidence_valid_days)
            {
                cell_errors.push("expired or invalid evidence timestamps".into());
            }
            gate.expires_at = Some(
                gate.expires_at
                    .map_or(r.expires_at, |old| old.min(r.expires_at)),
            );
            if r.expected_set_sha256 != expected_digest
                || r.archive_sha256 != expected.archive_sha256
                || ctx.archive_digests.get(&cell.build) != Some(&r.archive_sha256)
            {
                cell_errors.push("expected set/archive digest mismatch".into());
            }
            if digest::input_digest(&r.inputs)? != r.input_digest
                || ctx.input_digests.get(&cell.build) != Some(&r.input_digest)
            {
                cell_errors.push("input digest mismatch".into());
            }
            cell_errors.extend(input_findings(m, r));
            if r.command_exit != 0 {
                cell_errors.push(format!("command exit {}", r.command_exit));
            }
            for phase in ["setup", "run", "sweep", "upload"] {
                if r.phases.get(phase) != Some(&Outcome::Passed) {
                    cell_errors.push(format!("missing/failed mandatory phase {phase}"));
                }
            }
            if r.outcome != Outcome::Passed {
                cell_errors.push(format!("required suite reported {:?}", r.outcome));
            }
            if r.shard.of != pc.shards
                || r.shard.index == 0
                || r.shard.index > pc.shards
                || !shard_ids.insert(r.shard.index)
            {
                cell_errors.push("duplicate/invalid shard".into());
            }
            if let Some(s) = m.suites.get(&cell.suite) {
                if r.tier != s.tier || r.backend != s.backend {
                    cell_errors.push("tier/backend mismatch".into());
                }
                for cap in m.tiers[&s.tier].guarantees.iter().chain(&s.requires) {
                    if !r.capabilities_probed.contains(cap) {
                        cell_errors.push(format!("required capability {cap} not probed"));
                    }
                }
            } else if !r.capabilities_probed.contains("tempdir") {
                cell_errors.push("build smoke requires a probed private tempdir".into());
            }
            for result in &r.results {
                if !expected
                    .partitions
                    .get(&r.shard.index)
                    .is_some_and(|p| p.contains(&result.test))
                {
                    cell_errors.push(format!("{} is in the wrong shard partition", result.test));
                }
                if !ids.insert(result.test.clone()) {
                    cell_errors.push(format!("duplicate test {} across shards", result.test));
                }
                if !expected.tests.contains(&result.test) {
                    cell_errors.push(format!("unknown substituted test {}", result.test));
                }
                if result.outcome != Outcome::Passed || !result.capabilities_lost.is_empty() {
                    cell_errors.push(format!("required test {} {:?}/capability lost; skip/quarantine cannot make it green",result.test,result.outcome));
                }
                if let Some(contracts) = expected
                    .contracts
                    .get(&result.test)
                    .filter(|c| !c.is_empty())
                {
                    for contract in contracts {
                        if !result.assertions.contains(contract) {
                            cell_errors.push(format!(
                                "{} early-return/inconclusive: missing assertion {contract}",
                                result.test
                            ));
                        }
                    }
                } else {
                    cell_errors.push(format!(
                        "{} has no explicit contract assertions",
                        result.test
                    ));
                }
                results.push(result.clone());
            }
            if r.counts != super::junit::counts(r.results.len(), &r.results) {
                cell_errors.push("reported counts disagree with exact results".into());
            }
            gate.queue_ms = gate.queue_ms.max(r.durations.queue_ms);
            execution_start =
                Some(execution_start.map_or(r.started_at, |old| old.min(r.started_at)));
            execution_end = Some(execution_end.map_or(r.finished_at, |old| old.max(r.finished_at)));
        }
        for id in expected.tests.difference(&ids) {
            cell_errors.push(format!("expected test {id} did not run"));
        }
        if shard_ids.len() != pc.shards as usize {
            cell_errors.push("missing shards".into());
        }
        let passed = cell_errors.is_empty();
        gate.errors
            .extend(cell_errors.into_iter().map(|e| format!("{}: {e}", cell.id)));
        gate.cells.push(GateCell {
            cell: cell.id.clone(),
            outcome: if passed {
                Outcome::Passed
            } else {
                Outcome::Failed
            },
            evidence_sha256: hashes,
            counts: super::junit::counts(expected.tests.len(), &results),
        });
    }
    for s in m
        .release_critical
        .iter()
        .filter(|s| s.gates.contains(&authoritative.gate))
    {
        let applicable: Vec<_> = authoritative
            .cells
            .iter()
            .filter(|c| c.selected && c.cell.suite == s.suite)
            .collect();
        let passed = !applicable.is_empty()
            && applicable.iter().all(|c| {
                gate.cells
                    .iter()
                    .any(|r| r.cell == c.cell.id && r.outcome == Outcome::Passed)
                    && s.tests.iter().all(|t| {
                        records
                            .iter()
                            .filter(|r| r.cell == c.cell.id)
                            .flat_map(|r| &r.results)
                            .any(|r| r.test == *t && r.outcome == Outcome::Passed)
                    })
            });
        gate.release_critical.push(ScenarioResult {
            id: s.id.clone(),
            outcome: if passed {
                Outcome::Passed
            } else {
                Outcome::Failed
            },
        });
        if !passed {
            gate.errors.push(format!(
                "release-critical scenario {} missing or non-green",
                s.id
            ));
        }
    }
    for q in &m.quarantine {
        let deadline = chrono::NaiveDate::parse_from_str(&q.expires, "%Y-%m-%d")?
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        gate.expires_at = Some(gate.expires_at.map_or(deadline, |old| old.min(deadline)));
    }
    for o in &m.overrides {
        let deadline = chrono::NaiveDate::parse_from_str(&o.expires, "%Y-%m-%d")?
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        gate.expires_at = Some(gate.expires_at.map_or(deadline, |old| old.min(deadline)));
    }
    gate.execution_ms = execution_start
        .zip(execution_end)
        .map_or(0, |(start, end)| {
            (end - start).num_milliseconds().max(0) as u64
        });
    gate.errors.extend(budget_findings(
        &gate.gate,
        &ctx.request.main_durations_ms,
        gate.execution_ms,
        m.gates["main"].budget_minutes.unwrap_or(20) * 60_000,
    ));
    if !gate.errors.is_empty() {
        gate.verdict = Outcome::Failed;
    } else if !gate.missing.is_empty() {
        gate.verdict = Outcome::Blocked;
    }
    Ok(gate)
}
/// Permit recovery runs to measure an improvement instead of deadlocking planning.
pub fn budget_findings(
    gate: &str,
    history: &[u64],
    execution_ms: u64,
    budget_ms: u64,
) -> Vec<String> {
    if gate == "main"
        && history.len() >= 3
        && history.iter().rev().take(3).all(|n| *n > budget_ms)
        && execution_ms > budget_ms
    {
        vec!["main remains over budget after three consecutive regressions".into()]
    } else {
        Vec::new()
    }
}
/// Structural checks shared by the CI and release controllers. Authentication
/// and the independently discovered expected set remain controller obligations.
pub fn record_findings(m: &Manifest, r: &Evidence) -> Vec<String> {
    let mut errors = Vec::new();
    if r.command_exit != 0 {
        errors.push(format!("command exit {}", r.command_exit));
    }
    if r.shard.of == 0 || r.shard.index == 0 || r.shard.index > r.shard.of {
        errors.push("invalid shard".into());
    }
    if r.run.provider != "github-actions"
        || r.run.run_id == 0
        || r.run.attempt == 0
        || r.run.job_id == 0
        || r.run.artifact_id == 0
        || !digest::is_sha256(&r.run.artifact_service_digest)
    {
        errors.push("invalid producer identity".into());
    }
    if r.schema != 1 || r.kind != "cell" {
        errors.push("invalid cell schema".into());
    }
    if r.counts != super::junit::counts(r.results.len(), &r.results)
        || r.counts.expected != r.results.len() as u64
        || r.results.is_empty()
    {
        errors.push("reported counts disagree with exact results".into());
    }
    if digest::input_digest(&r.inputs).ok().as_deref() != Some(r.input_digest.as_str()) {
        errors.push("input digest mismatch".into());
    }
    if !digest::is_sha256(&r.expected_set_sha256) || !digest::is_sha256(&r.archive_sha256) {
        errors.push("invalid expected set/archive digest".into());
    }
    if r.results.iter().any(|t| t.assertions.is_empty()) {
        errors.push("missing contract assertions".into());
    }
    if let Some(suite) = m.suites.get(&r.suite) {
        if r.tier != suite.tier || r.backend != suite.backend {
            errors.push("tier/backend mismatch".into());
        }
        for cap in m.tiers[&suite.tier]
            .guarantees
            .iter()
            .chain(&suite.requires)
        {
            if !r.capabilities_probed.contains(cap) {
                errors.push(format!("required capability {cap} not probed"));
            }
        }
    }
    errors
}
pub fn input_findings(m: &Manifest, r: &Evidence) -> Vec<String> {
    let mut errors = Vec::new();
    let i = &r.inputs;
    let Some(b) = m.builds.get(&r.build) else {
        return vec![format!("unknown build in evidence inputs: {}", r.build)];
    };
    let p = &m.platforms[&b.platform];
    if i.schema != 1
        || !plan::is_commit(&i.source_tree)
        || i.source_tree != r.tree
        || i.toolchain != p.toolchain
        || i.profile != b.profile
        || i.target != *b.target.as_ref().unwrap_or(&p.triple)
        || i.runner_image != p.image
        || i.features != m.features[&b.features].cargo_args
    {
        errors.push(
            "incomplete/substituted source, toolchain, target, profile, image or features".into(),
        );
    }
    for path in [
        "Cargo.toml",
        "xtask/Cargo.toml",
        "crates/awman-cli/Cargo.toml",
        "crates/awman-host-shims/Cargo.toml",
        "crates/awman-wsp/Cargo.toml",
        "crates/awman-test-support/Cargo.toml",
    ] {
        if !i
            .cargo_manifests
            .get(path)
            .is_some_and(|v| digest::is_sha256(v))
        {
            errors.push(format!("build input missing Cargo manifest digest {path}"));
        }
    }
    if !digest::is_sha256(&i.cargo_lock)
        || !digest::is_sha256(&i.cargo_config)
        || i.tool_digests.is_empty()
        || i.tool_digests.values().any(|v| !digest::is_sha256(v))
    {
        errors.push("build inputs missing lock/config/tool digests".into());
    }
    if !i
        .build_scripts
        .get("build.rs")
        .is_some_and(|v| digest::is_sha256(v))
    {
        errors.push("build inputs missing build-script/dependency identity".into());
    }
    if b.features == "builtin"
        && !i
            .payload_manifest
            .as_ref()
            .is_some_and(|v| digest::is_sha256(v))
    {
        errors.push("builtin inputs missing verified payload digest".into());
    }
    if let Some(s) = m.suites.get(&r.suite) {
        let caps: BTreeSet<_> = m.tiers[&s.tier]
            .guarantees
            .iter()
            .chain(&s.requires)
            .map(String::as_str)
            .collect();
        if caps.contains("guest-fixtures")
            && (i.fixtures.is_empty() || !i.fixtures.values().all(|v| digest::is_sha256(v)))
        {
            errors.push("guest/artifact inputs missing fixture digests".into());
        }
        if caps.contains("previous-release")
            && !i
                .previous_release
                .as_ref()
                .is_some_and(|v| digest::is_sha256(v))
        {
            errors.push("guest/artifact inputs missing previous-release digest".into());
        }
    }
    if b.kind == "release"
        && !i
            .release_notes
            .as_ref()
            .is_some_and(|v| digest::is_sha256(v))
    {
        errors.push("release inputs missing release-notes bytes".into());
    }
    if b.kind == "release"
        && p.triple.contains("apple")
        && i.signing.as_ref().is_none_or(|s| {
            !digest::is_sha256(&s.entitlements_sha256)
                || s.command.is_empty()
                || s.identity.is_empty()
        })
    {
        errors.push("signed asset inputs missing entitlements/signing command/identity".into());
    }
    errors
}
pub fn partitions(expected: &ExpectedSet, shards: u32) -> Result<()> {
    ensure!(
        shards > 0 && expected.partitions.len() == shards as usize,
        "missing/extra expected shard partitions"
    );
    let mut union = BTreeSet::new();
    for index in 1..=shards {
        let part = expected
            .partitions
            .get(&index)
            .ok_or_else(|| anyhow::anyhow!("missing expected shard {index}"))?;
        ensure!(!part.is_empty(), "empty expected shard {index}");
        for test in part {
            ensure!(
                union.insert(test.clone()),
                "duplicate expected test {test} across shard partitions"
            );
        }
    }
    ensure!(
        union == expected.tests,
        "expected shard union differs from independently listed tests"
    );
    Ok(())
}
/// Expected-vs-ran check also used by the local writer. Required skips always fail.
pub fn compare(expected: &ExpectedSet, results: &[ResultRecord]) -> Result<()> {
    ensure!(!expected.tests.is_empty(), "empty required suite");
    let mut ids = BTreeSet::new();
    for r in results {
        ensure!(ids.insert(&r.test), "duplicate result {}", r.test);
        ensure!(
            expected.tests.contains(&r.test),
            "unknown substituted test {}",
            r.test
        );
        ensure!(
            r.outcome == Outcome::Passed && r.capabilities_lost.is_empty(),
            "required test {} is {:?} or lost capability",
            r.test,
            r.outcome
        );
        let contracts = expected
            .contracts
            .get(&r.test)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{} has no contract assertions", r.test))?;
        for c in contracts {
            ensure!(
                r.assertions.contains(c),
                "{} early-return/inconclusive: missing assertion {c}",
                r.test
            );
        }
    }
    let missing: Vec<_> = expected.tests.iter().filter(|t| !ids.contains(t)).collect();
    ensure!(
        missing.is_empty(),
        "expected tests did not run: {missing:?}"
    );
    Ok(())
}
/// Explicit nulls and a schema version keep cross-target candidate hashes stable.
pub fn candidate_digest(
    inputs: &BTreeMap<String, String>,
    artifacts: &BTreeMap<String, String>,
) -> Result<String> {
    digest::input_digest(&serde_json::json!({"schema":1,"inputs":inputs,"artifacts":artifacts}))
}
