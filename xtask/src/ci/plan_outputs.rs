//! Turns `plan.json` into the outputs the static job graph consumes:
//!
//! * `gate`, `builds` (selected build ids), `jobs` (selected test-job ids),
//! * `shards`: job id -> matrix entries `{i, n, suites}`; one entry per
//!   (shard, shard count, suites) the plan emitted, so a serial suite is never
//!   split and `--suite` receives exactly the selected suites.
//!
//! Job ids come from the renderer's own `test_job_id`, so the workflow and the
//! plan cannot disagree about names.

use super::args::Args;
use crate::{
    gates::{plan::Plan, render::workflows::test_job_id},
    manifest::{self, Manifest},
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::ExitCode,
};

pub fn outputs(m: &Manifest, plan: &Plan) -> Result<BTreeMap<String, Value>> {
    let builds: BTreeSet<_> = plan.builds.iter().map(|b| b.id.clone()).collect();
    let mut jobs: BTreeSet<String> = BTreeSet::new();
    let mut shards: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for t in &plan.tests {
        let id = test_job_id(m, &t.tier, &t.platform, &t.build);
        jobs.insert(id.clone());
        let entry = json!({"i": t.shard, "n": t.shards, "suites": t.suites, "host_resources": t.host_resources});
        let list = shards.entry(id).or_default();
        // Entries with the same (shard, count) merge their suites.
        if let Some(existing) = list
            .iter_mut()
            .find(|e| e["i"] == entry["i"] && e["n"] == entry["n"])
        {
            let mut all: BTreeSet<String> = existing["suites"]
                .as_str()
                .unwrap_or("")
                .split(',')
                .chain(t.suites.split(','))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            existing["suites"] = Value::String(
                std::mem::take(&mut all)
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(","),
            );
        } else {
            list.push(entry);
        }
    }
    if plan
        .cells
        .iter()
        .any(|c| c.selected && c.cell.suite == "web")
    {
        jobs.insert("web".into());
    }
    Ok(BTreeMap::from([
        ("gate".to_string(), Value::String(plan.gate.clone())),
        ("builds".to_string(), json!(builds)),
        ("jobs".to_string(), json!(jobs)),
        ("shards".to_string(), json!(shards)),
    ]))
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["plan", "hardware"], &[])?;
    let m = manifest::load(root)?;
    let plan: Plan =
        serde_json::from_slice(&std::fs::read(a.need("plan")?).context("reading plan")?)?;
    anyhow::ensure!(
        plan.manifest_version == m.version(),
        "plan was made from a different manifest version"
    );
    let _hardware = a.get("hardware").unwrap_or("false");
    for (k, v) in outputs(&m, &plan)? {
        match v {
            Value::String(s) => println!("{k}={s}"),
            other => println!("{k}={}", serde_json::to_string(&other)?),
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::plan::{create, Request};

    fn request(event: &str) -> Request {
        Request {
            repository_id: "1".into(),
            event: event.into(),
            head_sha: "a".repeat(40),
            base_sha: Some("b".repeat(40)),
            paths: vec!["Cargo.lock".into()],
            diff_complete: true,
            admitted_platforms: Default::default(),
            suite_durations_ms: Default::default(),
            suite_test_counts: Default::default(),
            main_durations_ms: vec![],
        }
    }

    #[test]
    fn pr_plan_names_exactly_the_rendered_jobs() {
        let root = crate::gates::root();
        let m = manifest::load(&root).unwrap();
        let today = chrono::Utc::now().date_naive();
        let plan = create(&m, "pr", request("pull_request"), today).unwrap();
        let o = outputs(&m, &plan).unwrap();
        let rendered: BTreeSet<String> = crate::gates::render::workflows::ci_test_jobs(&m)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .chain(["web".to_string()])
            .collect();
        let planned: BTreeSet<String> = serde_json::from_value(o["jobs"].clone()).unwrap();
        assert!(!planned.is_empty());
        assert!(
            planned.is_subset(&rendered),
            "plan names a job the workflow does not have: {:?}",
            planned.difference(&rendered).collect::<Vec<_>>()
        );
        for required in [
            "test-unit-linux-x64",
            "test-component-macos-arm64",
            "test-component-windows-x64",
            "boundary-linux-docker",
            "builtin-hermetic-linux-arm64",
        ] {
            assert!(
                planned.contains(required),
                "{required} missing from {planned:?}"
            );
        }
        // Guest cells never appear in a pull-request plan.
        assert!(planned.iter().all(|j| !j.contains("guest")));
        let builds: BTreeSet<String> = serde_json::from_value(o["builds"].clone()).unwrap();
        assert!(builds.contains("linux-x64-musl-static"));
    }

    #[test]
    fn main_plan_selects_guest_jobs_and_shards_have_entries() {
        let m = manifest::load(&crate::gates::root()).unwrap();
        let plan = create(&m, "main", request("push"), chrono::Utc::now().date_naive()).unwrap();
        let o = outputs(&m, &plan).unwrap();
        let jobs: BTreeSet<String> = serde_json::from_value(o["jobs"].clone()).unwrap();
        assert!(jobs.contains("test-guest-macos-arm64-ci-guest"));
        let shards = o["shards"].as_object().unwrap();
        for job in &jobs {
            if job != "web" {
                let entries = shards[job].as_array().unwrap();
                assert!(!entries.is_empty());
                assert!(entries
                    .iter()
                    .all(|e| !e["suites"].as_str().unwrap().is_empty()));
            }
        }
    }
    #[test]
    fn docker_shards_carry_an_explicit_engine_endpoint() {
        let m = manifest::load(&crate::gates::root()).unwrap();
        let plan = create(
            &m,
            "pr",
            request("pull_request"),
            chrono::Utc::now().date_naive(),
        )
        .unwrap();
        let output = outputs(&m, &plan).unwrap();
        let entries = output["shards"]["boundary-linux-docker"]
            .as_array()
            .unwrap();
        assert!(entries.iter().any(|e| e["host_resources"]
            .as_str()
            .unwrap()
            .contains("docker=unix:///var/run/docker.sock")));
        assert!(entries.iter().any(|e| e["host_resources"]
            .as_str()
            .unwrap()
            .contains("local-registry=127.0.0.1:5000")));
        assert!(entries.iter().any(|e| e["host_resources"]
            .as_str()
            .unwrap()
            .contains("git-transports")));
        let unit_entries = output["shards"]["test-unit-linux-x64"].as_array().unwrap();
        assert!(unit_entries.iter().all(|e| {
            e["suites"]
                .as_str()
                .unwrap()
                .split(',')
                .all(|suite| m.suites[suite].kind == "nextest")
        }));
    }
}
