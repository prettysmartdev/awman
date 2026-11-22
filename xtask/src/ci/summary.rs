//! The gate's step summary (design 11.6): tier x platform matrix of outcomes,
//! queue versus execution time per job, links to failure evidence, cache
//! statistics, then the aggregator's own details (slowest tests, quarantine,
//! overrides, reproduce lines). Reads only files; writes Markdown to stdout.

use super::args::Args;
use crate::{
    gates::plan::Plan,
    manifest::{self, Manifest},
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, process::ExitCode};

/// Worst outcome wins; ordered from most to least severe.
const ORDER: [&str; 7] = [
    "failed",
    "blocked",
    "missing",
    "inconclusive",
    "skipped",
    "passed",
    "not-selected",
];

fn rank(s: &str) -> usize {
    ORDER.iter().position(|o| *o == s).unwrap_or(0)
}

pub fn matrix(m: &Manifest, plan: &Plan, gate: &Value) -> String {
    let outcomes: BTreeMap<String, String> = gate["cells"]
        .as_array()
        .map(|cells| {
            cells
                .iter()
                .filter_map(|c| {
                    Some((
                        c["cell"].as_str()?.to_owned(),
                        c["outcome"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut grid: BTreeMap<(String, String), (String, u32, u32)> = BTreeMap::new();
    for c in &plan.cells {
        let Some(s) = m.suites.get(&c.cell.suite) else {
            continue;
        };
        let status = if !c.selected {
            "not-selected".to_string()
        } else {
            outcomes
                .get(&c.cell.id)
                .cloned()
                .unwrap_or_else(|| "missing".into())
        };
        let e = grid
            .entry((s.tier.clone(), c.cell.platform.clone()))
            .or_insert((status.clone(), 0, 0));
        if rank(&status) < rank(&e.0) {
            e.0 = status.clone();
        }
        if c.selected {
            e.2 += 1;
            if status == "passed" {
                e.1 += 1;
            }
        }
    }
    let platforms: Vec<_> = grid
        .keys()
        .map(|k| k.1.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let tiers: Vec<_> = grid
        .keys()
        .map(|k| k.0.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut out = format!(
        "| tier \\ platform | {} |\n| --- |{}\n",
        platforms.join(" | "),
        " --- |".repeat(platforms.len())
    );
    for t in &tiers {
        out.push_str(&format!("| {t} |"));
        for p in &platforms {
            match grid.get(&(t.clone(), p.clone())) {
                Some((s, ok, n)) if *n > 0 => out.push_str(&format!(" {s} ({ok}/{n}) |")),
                Some((s, _, _)) => out.push_str(&format!(" {s} |")),
                None => out.push_str(" - |"),
            }
        }
        out.push('\n');
    }
    out
}

fn secs(a: &str, b: &str) -> Option<i64> {
    let a: DateTime<Utc> = a.parse().ok()?;
    let b: DateTime<Utc> = b.parse().ok()?;
    Some((b - a).num_seconds().max(0))
}

/// `| job | conclusion | queued s | ran s |`, slowest first.
pub fn timings(jobs: &Value) -> String {
    let mut rows: Vec<(String, String, i64, i64)> = jobs
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|j| {
                    let name = j["name"].as_str()?.to_owned();
                    let queue = secs(j["created_at"].as_str()?, j["started_at"].as_str()?)?;
                    let run = secs(
                        j["started_at"].as_str()?,
                        j["completed_at"]
                            .as_str()
                            .unwrap_or(j["started_at"].as_str()?),
                    )?;
                    Some((
                        name,
                        j["conclusion"].as_str().unwrap_or("running").to_owned(),
                        queue,
                        run,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by_key(|r| std::cmp::Reverse(r.3));
    let mut out = String::from(
        "| job | conclusion | queued (s) | executed (s) |\n| --- | --- | ---: | ---: |\n",
    );
    for (n, c, q, r) in &rows {
        out.push_str(&format!("| {n} | {c} | {q} | {r} |\n"));
    }
    if let (Some(q), Some(r)) = (
        rows.iter().map(|r| r.2).max(),
        rows.iter().map(|r| r.3).max(),
    ) {
        out.push_str(&format!("\nLongest queue wait {q}s; longest job {r}s.\n"));
    }
    out
}

pub fn failures(artifacts: &Value, run_url: &str) -> String {
    let names: Vec<String> = artifacts
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["name"].as_str())
                .filter(|n| n.starts_with("failure-"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return "No failure records were uploaded.\n".into();
    }
    names
        .iter()
        .map(|n| format!("- `{n}` — [{run_url}#artifacts]({run_url}#artifacts)\n"))
        .collect()
}

fn cache_stats(dir: &Path) -> String {
    fn collect(dir: &Path, out: &mut Vec<Value>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect(&p, out);
            } else if p.file_name().is_some_and(|n| n == "cache-stats.json") {
                if let Ok(v) = serde_json::from_slice(&std::fs::read(&p).unwrap_or_default()) {
                    out.push(v);
                }
            }
        }
    }
    let mut stats = Vec::new();
    collect(dir, &mut stats);
    stats.sort_by_key(|s| s["build"].as_str().unwrap_or("").to_owned());
    let mut out =
        String::from("| build | cache | restored size (MiB) | key |\n| --- | --- | ---: | --- |\n");
    for s in &stats {
        out.push_str(&format!(
            "| {} | {} | {} | `{}` |\n",
            s["build"].as_str().unwrap_or("?"),
            if s["hit"].as_bool() == Some(true) {
                "hit"
            } else {
                "miss"
            },
            s["target_bytes"].as_u64().unwrap_or(0) / 1024 / 1024,
            s["key"].as_str().unwrap_or("?")
        ));
    }
    out
}

fn read_json(path: Option<&str>) -> Value {
    path.and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(
        args,
        &[
            "plan",
            "gate-evidence",
            "jobs",
            "artifacts",
            "details",
            "evidence",
            "run-url",
        ],
        &[],
    )?;
    let m = manifest::load(root)?;
    let plan: Plan =
        serde_json::from_slice(&std::fs::read(a.need("plan")?).context("reading plan")?)?;
    let gate = read_json(a.get("gate-evidence"));
    let verdict = gate["verdict"]
        .as_str()
        .unwrap_or("missing (no gate evidence was written)");
    println!("## Gate `{}`: {verdict}\n", plan.gate);
    println!("### Tier × platform\n\n{}", matrix(&m, &plan, &gate));
    println!(
        "### Queue time versus execution time\n\n{}",
        timings(&read_json(a.get("jobs")))
    );
    println!(
        "### Failure evidence\n\n{}",
        failures(
            &read_json(a.get("artifacts")),
            a.get("run-url").unwrap_or("")
        )
    );
    if let Some(dir) = a.get("evidence") {
        println!("### Cache\n\n{}", cache_stats(Path::new(dir)));
    }
    if let Some(Ok(details)) = a.get("details").map(std::fs::read_to_string) {
        println!("<details><summary>Aggregator details: slowest tests, quarantine, overrides, reproduce lines</summary>\n\n{details}\n\n</details>");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::plan::{create, Request};
    use serde_json::json;

    #[test]
    fn matrix_shows_worst_outcome_and_missing_cells_per_tier_and_platform() {
        let m = manifest::load(&crate::gates::root()).unwrap();
        let req = Request {
            repository_id: "1".into(),
            event: "pull_request".into(),
            head_sha: "a".repeat(40),
            base_sha: Some("b".repeat(40)),
            paths: vec!["Cargo.lock".into()],
            diff_complete: true,
            admitted_platforms: Default::default(),
            suite_durations_ms: Default::default(),
            suite_test_counts: Default::default(),
            main_durations_ms: vec![],
        };
        let plan = create(&m, "pr", req, chrono::Utc::now().date_naive()).unwrap();
        let first = plan
            .cells
            .iter()
            .find(|c| c.selected && m.suites.contains_key(&c.cell.suite))
            .unwrap();
        let gate = json!({"cells": [{"cell": first.cell.id, "outcome": "failed"}]});
        let text = matrix(&m, &plan, &gate);
        assert!(text.contains("failed"), "{text}");
        assert!(
            text.contains("missing"),
            "cells without evidence are shown as missing:\n{text}"
        );
        assert!(text.contains("| tier \\ platform |"));
    }

    #[test]
    fn timings_split_queue_from_execution() {
        let jobs = json!([{"name":"test-unit-linux-x64","conclusion":"success","created_at":"2026-11-11T10:00:00Z","started_at":"2026-11-11T10:00:30Z","completed_at":"2026-11-11T10:03:30Z"}]);
        let t = timings(&jobs);
        assert!(
            t.contains("| test-unit-linux-x64 | success | 30 | 180 |"),
            "{t}"
        );
        assert!(t.contains("Longest queue wait 30s"));
    }

    #[test]
    fn failure_links_list_only_failure_artifacts() {
        let a = json!([{"name":"evidence-pr-x"},{"name":"failure-test-unit-linux-x64-m0"}]);
        let f = failures(&a, "https://example/run/1");
        assert!(f.contains("failure-test-unit-linux-x64-m0") && !f.contains("evidence-pr-x"));
        assert!(failures(&json!([]), "u").contains("No failure records"));
    }
}
