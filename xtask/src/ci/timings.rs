//! Plan input from the durable timing store: per-suite p50 durations and test
//! counts (for shard counts), and the recent `main` gate durations (for the
//! three-consecutive-overruns rule). Diagnostic history never authorizes a
//! gate; it only sizes shards and warns.

use super::args::Args;
use crate::evidence::history::{main_runs, percentile, read, Sample};
use crate::evidence::schema::Outcome;
use anyhow::Result;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::ExitCode,
};

/// Sum a complete partition's work, without counting duplicate downloads or
/// overlapping tests. Failed/incomplete executions cannot size future shards.
fn whole_suite(rows: &[&Sample]) -> Option<(u64, u32)> {
    let mut shards = BTreeMap::new();
    let total = rows.first()?.shard.as_ref().map_or(1, |s| s.of);
    for row in rows {
        let (index, of) = row.shard.as_ref().map_or((1, 1), |s| (s.index, s.of));
        if of != total || index == 0 || index > total || row.outcome != Outcome::Passed {
            return None;
        }
        if let Some(previous) = shards.insert(index, *row) {
            if previous != *row {
                return None;
            }
        }
    }
    if shards.len() != total as usize {
        return None;
    }
    let mut tests = BTreeSet::new();
    let mut ms = 0u64;
    for row in shards.values() {
        ms = ms.checked_add(row.execution_ms)?;
        for (test, outcome) in &row.tests {
            if *outcome != Outcome::Passed || !tests.insert(test) {
                return None;
            }
        }
    }
    Some((ms, tests.len().try_into().ok()?))
}

pub fn timings(samples: &[Sample]) -> serde_json::Value {
    let mut durations: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut groups: BTreeMap<_, Vec<&Sample>> = BTreeMap::new();
    let mut attempts = BTreeMap::new();
    for s in samples {
        if s.tier == "gate" {
            continue;
        }
        let run = (s.gate.as_str(), s.run_id);
        let latest = attempts.entry(run).or_insert(0);
        *latest = (*latest).max(s.attempt);
        groups
            .entry((run, s.attempt, &s.cell, &s.commit, &s.manifest_version))
            .or_default()
            .push(s);
    }
    for ((run, attempt, cell, _, _), rows) in groups {
        if attempt != attempts[&run] {
            continue;
        }
        let Some((ms, count)) = whole_suite(&rows) else {
            continue;
        };
        let suite = cell.split('@').next().unwrap_or(cell).to_owned();
        durations.entry(suite.clone()).or_default().push(ms);
        let c = counts.entry(suite).or_default();
        *c = (*c).max(count);
    }
    json!({
        "suite_durations_ms": durations.iter().filter_map(|(k, v)| Some((k.clone(), percentile(v, 50)?))).collect::<BTreeMap<_, _>>(),
        "suite_test_counts": counts,
        // run ids grow over time, so the BTreeMap order is chronological
        "main_durations_ms": main_runs(samples).values().map(|(_, ms)| ms).collect::<Vec<_>>(),
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
            shard: None,
            manifest_version: "m".into(),
            commit: "c".into(),
            tests: (0..tests)
                .map(|i| (format!("t{i}"), Outcome::Passed))
                .collect(),
            queue_ms: None,
            execution_ms: ms,
            outcome: Outcome::Passed,
            cache_hit: None,
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
        // Suite durations are not whole-gate wall time.
        assert_eq!(t["main_durations_ms"], json!([]));
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

    #[test]
    fn sharded_work_is_complete_disjoint_and_counted_once() {
        use crate::evidence::schema::Shard;
        let mut a = sample("suite@linux", "pr", 1, 90_000, 2);
        a.shard = Some(Shard { index: 1, of: 2 });
        let mut b = a.clone();
        b.shard.as_mut().unwrap().index = 2;
        b.tests = [
            ("t2".into(), Outcome::Passed),
            ("t3".into(), Outcome::Passed),
        ]
        .into();
        let t = timings(&[a.clone(), b.clone(), a.clone()]);
        assert_eq!(t["suite_durations_ms"]["suite"], 180_000);
        assert_eq!(t["suite_test_counts"]["suite"], 4);
        assert_eq!(timings(&[a.clone()])["suite_durations_ms"], json!({}));
        // Never mix platforms, attempts, or overlapping inventories to fill a partition.
        for bad in [
            Sample {
                cell: "suite@macos".into(),
                ..b.clone()
            },
            Sample {
                attempt: 2,
                ..b.clone()
            },
            Sample {
                tests: a.tests.clone(),
                ..b.clone()
            },
            Sample {
                outcome: Outcome::Failed,
                ..b.clone()
            },
        ] {
            assert_eq!(timings(&[a.clone(), bad])["suite_durations_ms"], json!({}));
        }
        let conflict = Sample {
            execution_ms: 1,
            ..a.clone()
        };
        assert_eq!(timings(&[a, b, conflict])["suite_durations_ms"], json!({}));
    }

    #[test]
    fn gate_wall_time_uses_latest_attempt_of_distinct_runs() {
        let mut a = sample("gate", "main", 1, 300, 0);
        a.tier = "gate".into();
        let b = Sample {
            attempt: 2,
            execution_ms: 200,
            ..a.clone()
        };
        let c = Sample {
            attempt: 3,
            execution_ms: 100,
            ..a.clone()
        };
        let rows = [c.clone(), a.clone(), b];
        assert_eq!(timings(&rows)["main_durations_ms"], json!([100]));
        assert!(!crate::evidence::history::budget_regression(&rows, 50));
        let rows = [
            a,
            Sample {
                run_id: 2,
                ..c.clone()
            },
            Sample { run_id: 3, ..c },
        ];
        assert!(crate::evidence::history::budget_regression(&rows, 50));
        assert_eq!(timings(&rows)["suite_durations_ms"], json!({}));
    }
}
