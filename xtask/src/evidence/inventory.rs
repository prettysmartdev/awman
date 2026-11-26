//! Independently compiled suite inventories, authenticated as build artifacts.
use super::actions;
use crate::{gates::plan::Plan, manifest::Manifest};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
pub type Inventories = BTreeMap<String, BTreeSet<String>>;
pub fn raw(dir: &Path) -> Result<Inventories> {
    let run: Value = serde_json::from_slice(&std::fs::read(dir.join("run.json"))?)?;
    let mut out = BTreeMap::new();
    for inv in run["invocations"]
        .as_array()
        .context("missing invocations")?
    {
        if let Some(name) = inv["expected"].as_str() {
            let tests = actions::expected(&serde_json::from_slice(&std::fs::read(
                actions::file_in(dir, name)?,
            )?)?)?;
            let suite = inv["suites"][0].as_str().context("missing suite")?;
            ensure!(
                out.insert(suite.into(), tests).is_none(),
                "duplicate suite inventory"
            );
        }
    }
    Ok(out)
}
pub fn authenticate(
    repo: &str,
    artifacts: &[Value],
    p: &Plan,
    m: &Manifest,
    source: &Value,
    mode: &str,
    scratch: &Path,
) -> Result<(Inventories, Vec<Value>)> {
    let mut out = BTreeMap::new();
    let mut receipts = Vec::new();
    for b in p.builds.iter().filter(|b| m.builds[&b.id].kind == "test") {
        let name = format!("test-inventory-{}-{mode}-{}", p.gate, b.id);
        let a = actions::artifact(artifacts, &name)?;
        let dir = scratch.join(&name);
        actions::download(repo, a, &dir)?;
        let v: Value = serde_json::from_slice(&std::fs::read(dir.join("suite-inventories.json"))?)?;
        ensure!(
            v["schema"] == 1
                && v["build"] == b.id
                && v["manifest_sha256"] == m.version()
                && &v["source"] == source,
            "build inventory identity mismatch"
        );
        let run_id = actions::number(&a["workflow_run"], "id")?;
        ensure!(
            actions::text(&v["producer"], "run_id")? == run_id.to_string(),
            "build inventory producer run differs from service"
        );
        let attempt = actions::text(&v["producer"], "attempt")?.parse::<u64>()?;
        ensure!(
            attempt > 0
                && v["producer"]["job"] == crate::gates::render::workflows::build_job_id(m, &b.id),
            "build inventory producer job/attempt mismatch"
        );
        let jobs = actions::pages(
            &format!("repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"),
            "jobs",
        )?;
        let title = format!("Build {}", b.id);
        actions::job_passed(&jobs, &title, 1)?;
        let job = jobs
            .iter()
            .find(|j| {
                j["name"]
                    .as_str()
                    .is_some_and(|n| n.rsplit(" / ").next() == Some(title.as_str()))
            })
            .context("build inventory service job missing")?;
        receipts.push(serde_json::json!({"id":a["id"],"service_digest":a["digest"],"downloaded_digest":a["digest"],"run_id":run_id,"job_id":actions::number(job,"id")?,"producer_attempt":attempt}));
        for (suite, tests) in v["suites"]
            .as_object()
            .context("missing build suite inventories")?
        {
            let tests = tests.as_array().context("invalid build inventory")?;
            let unique: BTreeSet<_> = tests
                .iter()
                .map(|t| t.as_str().map(str::to_owned).context("invalid test ID"))
                .collect::<Result<_>>()?;
            ensure!(
                !unique.is_empty() && unique.len() == tests.len(),
                "empty or duplicate build inventory"
            );
            out.insert(format!("{suite}@{}", b.id), unique);
        }
    }
    Ok((out, receipts))
}
pub fn merge(
    all: &mut Inventories,
    build: &str,
    selected: Inventories,
    expected: &Inventories,
) -> Result<()> {
    for (suite, tests) in selected {
        let key = format!("{suite}@{build}");
        let want = expected
            .get(&key)
            .context("suite lacks independently compiled build inventory")?;
        ensure!(
            tests.is_subset(want),
            "run inventory has tests outside the build inventory"
        );
        let got = all.entry(key).or_default();
        ensure!(got.is_disjoint(&tests), "test appears in multiple shards");
        got.extend(tests);
    }
    Ok(())
}
pub fn complete(all: &Inventories, expected: &Inventories, cell: &str) -> Result<()> {
    ensure!(
        all.get(cell).is_some() && all.get(cell) == expected.get(cell),
        "incomplete independently compiled inventory for {cell}"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_passing_subset_cannot_hide_an_omitted_test_or_duplicate_shard() {
        let expected = BTreeMap::from([(
            "suite@build".into(),
            BTreeSet::from(["a".into(), "b".into()]),
        )]);
        let mut got = BTreeMap::new();
        let shard = BTreeMap::from([("suite".into(), BTreeSet::from(["a".into()]))]);
        merge(&mut got, "build", shard.clone(), &expected).unwrap();
        assert!(complete(&got, &expected, "suite@build").is_err());
        assert!(merge(&mut got, "build", shard, &expected).is_err());
        merge(
            &mut got,
            "build",
            BTreeMap::from([("suite".into(), BTreeSet::from(["b".into()]))]),
            &expected,
        )
        .unwrap();
        complete(&got, &expected, "suite@build").unwrap();
    }
}
