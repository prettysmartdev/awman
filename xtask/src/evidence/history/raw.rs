//! Diagnostics from current producer records, never an admission authority.
use super::{write, Sample};
use crate::evidence::{actions, junit, schema::*};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

pub fn append_run(store: &Path, file: &Path, gate: &str) -> Result<()> {
    let raw: Value = serde_json::from_slice(&fs::read(file)?)?;
    ensure!(raw["schema"] == 1, "unsupported run record schema");
    let shard = match raw["shard"].as_str() {
        Some(value) => {
            let (index, of) = value.split_once('/').context("shard must be i/N")?;
            let shard = Shard {
                index: index.parse()?,
                of: of.parse()?,
            };
            ensure!(shard.index > 0 && shard.index <= shard.of, "invalid shard");
            Some(shard)
        }
        None => None,
    };
    let dir = file.parent().context("run directory")?;
    let mut results = BTreeMap::new();
    for file in fs::read_dir(dir)? {
        let path = file?.path();
        if path.extension().is_some_and(|e| e == "xml") {
            for result in junit::parse(&fs::read_to_string(path)?)? {
                ensure!(
                    results.insert(result.test, result.outcome).is_none(),
                    "duplicate diagnostic result"
                );
            }
        }
    }
    for inv in raw["invocations"].as_array().context("invocations")? {
        let suites = inv["suites"].as_array().context("suites")?;
        ensure!(
            suites.len() == 1,
            "timings require one invocation per suite"
        );
        let id = suites[0].as_str().context("suite ID")?;
        let tests = if let Some(name) = inv["expected"].as_str() {
            let expected: Value = serde_json::from_slice(&fs::read(actions::file_in(dir, name)?)?)?;
            actions::expected(&expected)?
                .into_iter()
                .map(|id| {
                    let outcome = results.get(&id).copied().unwrap_or(Outcome::Blocked);
                    (id, outcome)
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        let outcome = if tests.values().any(|o| *o == Outcome::Failed) {
            Outcome::Failed
        } else if tests.values().any(|o| *o != Outcome::Passed) {
            Outcome::Blocked
        } else if inv["exit"] == 0 {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        write(
            store,
            &Sample {
                schema: 1,
                gate: gate.into(),
                cell: format!("{id}@{}", raw["build"].as_str().context("build")?),
                tier: inv["tier"].as_str().context("tier")?.into(),
                run_id: raw["producer"]["run_id"]
                    .as_str()
                    .context("run ID")?
                    .parse()?,
                attempt: raw["producer"]["attempt"]
                    .as_str()
                    .context("attempt")?
                    .parse()?,
                shard: shard.clone(),
                manifest_version: raw["manifest_version"].as_str().context("manifest")?.into(),
                commit: raw["source"]["commit"].as_str().context("commit")?.into(),
                tests,
                // Queue and cache belong to the job, not its individual invocations.
                queue_ms: None,
                execution_ms: inv["execution_ms"]
                    .as_u64()
                    .context("missing measured invocation time")?,
                outcome,
                cache_hit: None,
                rebuilds: None,
                manual_interventions: None,
            },
        )?;
    }
    Ok(())
}

pub fn append_gate(store: &Path, file: &Path) -> Result<()> {
    let gate: GateEvidence = serde_json::from_slice(&fs::read(file)?)?;
    write(
        store,
        &Sample {
            schema: 1,
            gate: gate.gate,
            cell: "gate".into(),
            tier: "gate".into(),
            run_id: std::env::var("GITHUB_RUN_ID")?.parse()?,
            attempt: std::env::var("GITHUB_RUN_ATTEMPT")?.parse()?,
            shard: None,
            manifest_version: gate.manifest_version,
            commit: gate.commit,
            tests: BTreeMap::new(),
            queue_ms: Some(gate.queue_ms),
            execution_ms: gate.execution_ms,
            outcome: gate.verdict,
            cache_hit: None,
            rebuilds: None,
            manual_interventions: None,
        },
    )
}

/// Longest predecessor queue and wall span from first job start to last finish.
/// In-progress aggregation and skipped jobs have no completed duration.
pub fn job_times(jobs: &[Value]) -> Result<(u64, u64)> {
    // Reusable workflows share the provider's job list with their caller and
    // siblings. Use the already-validated Plan job's prefix to isolate this gate.
    let scope = |name: &str| {
        name.rsplit_once(" / ")
            .map_or("", |(prefix, _)| prefix)
            .to_owned()
    };
    let plan = jobs
        .iter()
        .find_map(|j| {
            j["name"]
                .as_str()
                .filter(|name| name.rsplit(" / ").next() == Some("Plan"))
        })
        .context("no Plan job for timing scope")?;
    let prefix = scope(plan);
    let mut queue = 0;
    let mut start: Option<DateTime<Utc>> = None;
    let mut end: Option<DateTime<Utc>> = None;
    for job in jobs.iter().filter(|j| {
        j["status"] == "completed"
            && j["conclusion"] != "skipped"
            && j["name"].as_str().is_some_and(|name| scope(name) == prefix)
    }) {
        let time = |key: &str| -> Result<DateTime<Utc>> {
            Ok(job[key]
                .as_str()
                .with_context(|| format!("job lacks {key}"))?
                .parse()?)
        };
        let created = time("created_at")?;
        let started = time("started_at")?;
        let completed = time("completed_at")?;
        ensure!(
            completed >= started && started >= created,
            "inverted job timestamps"
        );
        queue = queue.max((started - created).num_milliseconds() as u64);
        start = Some(start.map_or(started, |s| s.min(started)));
        end = Some(end.map_or(completed, |e| e.max(completed)));
    }
    let span = start.zip(end).context("no completed job timings")?;
    Ok((queue, (span.1 - span.0).num_milliseconds() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_records_preserve_failures_and_unknown_measurements() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run.json");
        let store = dir.path().join("history.jsonl");
        fs::write(dir.path().join("expected.json"), json!({"rust-suites":{"s":{"binary-id":"p","testcases":{"a":{"ignored":false,"filter-match":{"status":"matches"}},"b":{"ignored":false,"filter-match":{"status":"matches"}}}}}}).to_string()).unwrap();
        fs::write(
            dir.path().join("junit.xml"),
            "<testsuite><testcase classname=\"p\" name=\"a\"><failure/></testcase></testsuite>",
        )
        .unwrap();
        fs::write(&run, json!({"schema":1,"shard":"1/2","build":"linux","manifest_version":"m","source":{"commit":"c"},"producer":{"run_id":"42","attempt":"1"},"invocations":[{"suites":["s"],"tier":"unit","exit":1,"expected":"expected.json","execution_ms":123}]}).to_string()).unwrap();
        append_run(&store, &run, "pr").unwrap();
        let rows = super::super::read(&store).unwrap();
        assert_eq!(rows[0].tests["p a"], Outcome::Failed);
        assert_eq!(rows[0].tests["p b"], Outcome::Blocked);
        assert_eq!(rows[0].execution_ms, 123);
        assert_eq!(rows[0].queue_ms, None);
        assert_eq!(rows[0].cache_hit, None);
        assert_eq!(rows[0].shard, Some(Shard { index: 1, of: 2 }));
        assert_eq!(rows[0].outcome, Outcome::Failed);
        // Missing JUnit is blocked evidence, never a passing invocation.
        fs::remove_file(dir.path().join("junit.xml")).unwrap();
        append_run(&store, &run, "pr").unwrap();
        let rows = super::super::read(&store).unwrap();
        assert_eq!(rows[1].outcome, Outcome::Blocked);
        assert!(super::super::report(&rows).contains("0/0 known"));
        // Old stores had neither shard identity nor nullable measurements.
        let mut old = serde_json::to_value(&rows[0]).unwrap();
        old.as_object_mut().unwrap().remove("shard");
        old["queue_ms"] = json!(0);
        old["cache_hit"] = json!(false);
        let old: Sample = serde_json::from_value(old).unwrap();
        assert_eq!(old.shard, None);
        assert_eq!(old.cache_hit, Some(false));
    }

    #[test]
    fn gate_span_uses_provider_timestamps_and_excludes_unfinished_jobs() {
        let job = |name, created, started, completed| json!({"name":name,"status":"completed","conclusion":"success","created_at":created,"started_at":started,"completed_at":completed});
        let jobs = vec![
            job(
                "full / Plan",
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:05Z",
                "2026-01-01T00:00:10Z",
            ),
            job(
                "full / Tests",
                "2026-01-01T00:00:10Z",
                "2026-01-01T00:00:30Z",
                "2026-01-01T00:00:40Z",
            ),
            json!({"status":"in_progress"}),
            json!({"status":"completed","conclusion":"skipped"}),
            job(
                "other / Tests",
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
                "2026-01-01T01:00:00Z",
            ),
        ];
        assert_eq!(job_times(&jobs).unwrap(), (20_000, 35_000));
        assert!(job_times(&[]).is_err());
    }
}
