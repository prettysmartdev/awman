//! Plan input from the durable timing store: per-suite p50 durations and test
//! counts (for shard counts), and the recent `main` gate durations (for the
//! three-consecutive-overruns rule). Diagnostic history never authorizes a
//! gate; it only sizes shards and warns.

use super::args::Args;
use crate::evidence::history::{percentile, read, Sample};
use anyhow::Result;
use serde_json::json;
use std::{collections::BTreeMap, path::Path, process::ExitCode};

pub fn timings(samples: &[Sample]) -> serde_json::Value {
    let mut durations: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut runs: BTreeMap<u64, u64> = BTreeMap::new();
    for s in samples {
        let suite = s.cell.split('@').next().unwrap_or(&s.cell).to_owned();
        durations
            .entry(suite.clone())
            .or_default()
            .push(s.execution_ms);
        let c = counts.entry(suite).or_default();
        *c = (*c).max(s.tests.len() as u32);
        if s.gate == "main" {
            let longest = runs.entry(s.run_id).or_default();
            *longest = (*longest).max(s.execution_ms);
        }
    }
    json!({
        "suite_durations_ms": durations.iter().filter_map(|(k, v)| Some((k.clone(), percentile(v, 50)?))).collect::<BTreeMap<_, _>>(),
        "suite_test_counts": counts,
        // run ids grow over time, so the BTreeMap order is chronological
        "main_durations_ms": runs.values().collect::<Vec<_>>(),
    })
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["store", "out"], &[])?;
    let samples = read(Path::new(a.need("store")?))?;
    std::fs::write(
        a.need("out")?,
        serde_json::to_vec_pretty(&timings(&samples))?,
    )?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::schema::Outcome;

    fn sample(cell: &str, gate: &str, run: u64, ms: u64, tests: usize) -> Sample {
        Sample {
            schema: 1,
            gate: gate.into(),
            cell: cell.into(),
            tier: "unit".into(),
            run_id: run,
            attempt: 1,
            manifest_version: "m".into(),
            commit: "c".into(),
            tests: (0..tests)
                .map(|i| (format!("t{i}"), Outcome::Passed))
                .collect(),
            queue_ms: 0,
            execution_ms: ms,
            outcome: Outcome::Passed,
            cache_hit: false,
            rebuilds: None,
            manual_interventions: None,
        }
    }

    #[test]
    fn timing_input_is_keyed_by_suite_and_accepted_by_the_plan_request() {
        let t = timings(&[
            sample("unit-awman@linux-x64-default", "main", 1, 100, 5),
            sample("unit-awman@macos-arm64-default", "main", 1, 300, 5),
            sample("unit-awman@linux-x64-default", "main", 2, 200, 6),
        ]);
        assert_eq!(t["suite_durations_ms"]["unit-awman"], 200);
        assert_eq!(t["suite_test_counts"]["unit-awman"], 6);
        assert_eq!(t["main_durations_ms"], json!([300, 200]));
        // The plan CLI reads exactly these field names.
        #[derive(serde::Deserialize)]
        struct Shape {
            #[allow(dead_code)]
            suite_durations_ms: BTreeMap<String, u64>,
            #[allow(dead_code)]
            suite_test_counts: BTreeMap<String, u32>,
            #[allow(dead_code)]
            main_durations_ms: Vec<u64>,
        }
        serde_json::from_value::<Shape>(t).unwrap();
    }
}
