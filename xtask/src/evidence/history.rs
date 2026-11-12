//! Append-only diagnostic JSONL; not an authority for CI or release success.
use super::schema::*;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write, path::Path};
pub mod raw;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub schema: u32,
    pub gate: String,
    pub cell: String,
    pub tier: String,
    pub run_id: u64,
    pub attempt: u32,
    #[serde(default)]
    pub shard: Option<Shard>,
    pub manifest_version: String,
    pub commit: String,
    pub tests: BTreeMap<String, Outcome>,
    pub queue_ms: Option<u64>,
    pub execution_ms: u64,
    pub outcome: Outcome,
    pub cache_hit: Option<bool>,
    pub rebuilds: Option<u32>,
    pub manual_interventions: Option<u32>,
}
pub fn append(store: &Path, e: &Evidence) -> Result<()> {
    let sample = Sample {
        schema: 1,
        gate: e.gate.clone(),
        cell: e.cell.clone(),
        tier: e.tier.clone(),
        run_id: e.run.run_id,
        attempt: e.run.attempt,
        shard: Some(e.shard.clone()),
        manifest_version: e.manifest_version.clone(),
        commit: e.commit.clone(),
        tests: e
            .results
            .iter()
            .map(|r| (r.test.clone(), r.outcome))
            .collect(),
        queue_ms: Some(e.durations.queue_ms),
        execution_ms: e.durations.run_ms,
        outcome: e.outcome,
        cache_hit: e.cache.as_ref().map(|c| c.hit),
        rebuilds: None,
        manual_interventions: None,
    };
    write(store, &sample)
}
pub fn write(store: &Path, sample: &Sample) -> Result<()> {
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store)?;
    file.write_all(format!("{}\n", serde_json::to_string(&sample)?).as_bytes())?;
    Ok(())
}
pub fn read(store: &Path) -> Result<Vec<Sample>> {
    let mut rows = Vec::new();
    for line in std::fs::read_to_string(store)?.lines() {
        let s: Sample = serde_json::from_str(line)?;
        ensure!(s.schema == 1, "unsupported timing history schema");
        rows.push(s);
    }
    Ok(rows)
}
pub fn percentile(values: &[u64], percent: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    Some(
        v[(v.len() * percent)
            .div_ceil(100)
            .saturating_sub(1)
            .min(v.len() - 1)],
    )
}
pub fn report(samples: &[Sample]) -> String {
    let mut groups: BTreeMap<(&str, &str), Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        groups.entry((&s.gate, &s.tier)).or_default().push(s);
    }
    let mut out = String::from(
        "Gate / Tier | Samples | Execution p50/p95 ms | Queue p50/p95 ms | Non-green | Cache hits\n",
    );
    for ((gate, tier), rows) in groups {
        let times: Vec<_> = rows.iter().map(|s| s.execution_ms).collect();
        let queue: Vec<_> = rows.iter().filter_map(|s| s.queue_ms).collect();
        out.push_str(&format!(
            "{gate} / {tier} | {} | {:?}/{:?} | {:?}/{:?} | {} | {}/{} known\n",
            rows.len(),
            percentile(&times, 50),
            percentile(&times, 95),
            percentile(&queue, 50),
            percentile(&queue, 95),
            rows.iter().filter(|s| s.outcome != Outcome::Passed).count(),
            rows.iter().filter(|s| s.cache_hit == Some(true)).count(),
            rows.iter().filter(|s| s.cache_hit.is_some()).count()
        ));
        let mut repeated: BTreeMap<(&str, &str, &str, &str), (bool, bool)> = BTreeMap::new();
        for row in &rows {
            for (test, outcome) in &row.tests {
                let flags = repeated
                    .entry((&row.cell, &row.commit, &row.manifest_version, test))
                    .or_default();
                flags.0 |= *outcome == Outcome::Passed;
                flags.1 |= *outcome == Outcome::Failed;
            }
        }
        let mixed = repeated
            .values()
            .filter(|(passed, failed)| *passed && *failed)
            .count();
        out.push_str(&format!("  Same-commit mixed pass/fail test histories: {mixed}/{}. Infrastructure blocks are reported separately.\n",repeated.len()));
    }
    out
}
/// Three consecutive main runs above budget block further planning until reviewed.
pub fn budget_regression(samples: &[Sample], budget_ms: u64) -> bool {
    let runs = main_runs(samples);
    let last: Vec<_> = runs.values().rev().take(3).collect();
    last.len() == 3 && last.iter().all(|(_, ms)| *ms > budget_ms)
}

/// A retry replaces a run's measurement; three attempts are not three runs.
pub fn main_runs(samples: &[Sample]) -> BTreeMap<u64, (u32, u64)> {
    let mut runs = BTreeMap::new();
    for s in samples
        .iter()
        .filter(|s| s.gate == "main" && s.tier == "gate")
    {
        let row = runs.entry(s.run_id).or_insert((0, 0));
        if s.attempt >= row.0 {
            *row = (s.attempt, s.execution_ms);
        }
    }
    runs
}
