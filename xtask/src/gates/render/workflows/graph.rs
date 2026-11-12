//! The job graph of `ci.yml`, derived from the manifest. Job identifiers are
//! functions of manifest ids, so adding a platform or a build to the manifest
//! adds its jobs on the next render and a renamed build renames its job.

use crate::manifest::{self, Manifest};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Gates whose cells `ci.yml` runs (the candidate/promote gates belong to the
/// release workflows).
pub const CI_GATES: [&str; 3] = ["pr", "main", "nightly"];

/// Identifier of the job that builds (and archives) one build cell.
pub fn build_job_id(m: &Manifest, build: &str) -> String {
    let b = &m.builds[build];
    if b.kind == "build-smoke" {
        if b.target.as_deref().is_some_and(|t| t.contains("musl")) {
            "static-musl".into()
        } else {
            format!("smoke-{build}")
        }
    } else {
        format!("build-{build}")
    }
}

/// Identifier of the job that runs the nextest suites of one tier on one build.
pub fn test_job_id(m: &Manifest, tier: &str, platform: &str, build: &str) -> String {
    let b = &m.builds[build];
    if tier == "boundary" && platform == "linux-x64" {
        "boundary-linux-docker".into()
    } else if m.features[&b.features]
        .cargo_args
        .iter()
        .any(|a| a.contains("builtin"))
        && platform.starts_with("linux-arm64")
        && ["unit", "component"].contains(&tier)
    {
        // The hermetic builtin-feature unit suites, native on arm64 hosted Linux.
        format!("builtin-hermetic-{platform}")
    } else {
        format!("test-{tier}-{platform}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestJob {
    pub id: String,
    pub tier: String,
    pub platform: String,
    pub build: String,
}

/// Builds `ci.yml` compiles: test-archive builds and build smokes of its gates.
pub fn ci_builds(m: &Manifest) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for gate in CI_GATES {
        for cell in manifest::validate::cells(m, gate)? {
            if !m.suites.contains_key(&cell.suite) || m.builds[&cell.build].kind == "test" {
                out.insert(cell.build);
            }
        }
        for smoke in &m.gates[gate].build_smokes {
            out.insert(smoke.clone());
        }
    }
    Ok(out)
}

/// Nextest test jobs, keyed by job id. Release-kind builds are the release
/// workflows' business and never appear here.
pub fn ci_test_jobs(m: &Manifest) -> Result<Vec<TestJob>> {
    let mut jobs: BTreeMap<String, TestJob> = BTreeMap::new();
    for gate in CI_GATES {
        for cell in manifest::validate::cells(m, gate)? {
            let Some(suite) = m.suites.get(&cell.suite) else {
                continue;
            };
            if suite.kind != "nextest" || m.builds[&cell.build].kind != "test" {
                continue;
            }
            let id = test_job_id(m, &suite.tier, &cell.platform, &cell.build);
            let job = TestJob {
                id: id.clone(),
                tier: suite.tier.clone(),
                platform: cell.platform.clone(),
                build: cell.build.clone(),
            };
            match jobs.get(&id) {
                Some(existing) if *existing != job => bail!(
                    "test job {id} would run two different builds ({} and {}); name them apart in the renderer",
                    existing.build,
                    job.build
                ),
                _ => {
                    jobs.insert(id, job);
                }
            }
        }
    }
    Ok(jobs.into_values().collect())
}

/// Suites of kind `command`/`doctest` that run in the single `lint` job.
pub fn lint_suites(m: &Manifest) -> Vec<String> {
    m.gates["pr"]
        .cells
        .iter()
        .flat_map(|c| c.suites.iter())
        .filter(|s| {
            m.suites
                .get(*s)
                .is_some_and(|s| s.tier == "unit" && (s.kind == "command" || s.kind == "doctest"))
        })
        .cloned()
        .collect()
}

/// Whether a platform runs on a self-hosted machine (anything not `hosted`).
pub fn self_hosted(m: &Manifest, platform: &str) -> bool {
    m.platforms[platform].trust != "hosted"
}
