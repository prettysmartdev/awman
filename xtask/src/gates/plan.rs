use crate::manifest::{self, Cell, Manifest};
use anyhow::{ensure, Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository_id: String,
    pub event: String,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub paths: Vec<String>,
    pub diff_complete: bool,
    /// Obtained by the trusted CI controller's server-side runner admission.
    #[serde(default)]
    pub admitted_platforms: BTreeSet<String>,
    #[serde(default)]
    pub suite_durations_ms: BTreeMap<String, u64>,
    #[serde(default)]
    pub suite_test_counts: BTreeMap<String, u32>,
    #[serde(default)]
    pub main_durations_ms: Vec<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedCell {
    pub cell: Cell,
    pub selected: bool,
    pub reason: String,
    pub shards: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: u32,
    pub gate: String,
    pub manifest_version: String,
    pub request: Request,
    pub cells: Vec<PlannedCell>,
    pub builds: Vec<BuildMatrix>,
    pub tests: Vec<TestMatrix>,
    pub quarantine: Vec<String>,
    pub overrides: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildMatrix {
    pub id: String,
    pub runs_on: Vec<String>,
    pub triple: String,
    pub toolchain: String,
    pub kind: String,
    pub timeout: u64,
    pub blocked_reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestMatrix {
    pub job: String,
    pub tier: String,
    pub build: String,
    pub platform: String,
    pub runs_on: Vec<String>,
    pub suites: String,
    pub shard: u32,
    pub shards: u32,
    pub timeout: u64,
    pub self_hosted: bool,
    pub host_resources: String,
    pub blocked_reason: Option<String>,
}
fn routing(m: &Manifest, platform: &str, request: &Request) -> (Vec<String>, Option<String>) {
    let p = &m.platforms[platform];
    if p.trust != "hosted"
        && (!request.admitted_platforms.contains(platform) || request.event == "local")
    {
        (
            m.platforms["linux-x64"].runs_on.clone(),
            Some(format!(
                "blocked: {platform} has no server-verified runner admission (P1/P3)"
            )),
        )
    } else {
        (p.runs_on.clone(), None)
    }
}
impl Plan {
    pub fn digest(&self) -> Result<String> {
        crate::evidence::digest::input_digest(self)
    }
}

pub fn create(m: &Manifest, gate: &str, request: Request, today: NaiveDate) -> Result<Plan> {
    manifest::validate::validate(m, today)?;
    // Timing history is evidence for the final gate, never a reason to stop
    // the fixing PR (or its first main run) from executing and measuring itself.
    ensure!(
        [
            "local",
            "pull_request",
            "merge_group",
            "push",
            "schedule",
            "workflow_dispatch",
            "workflow_call"
        ]
        .contains(&request.event.as_str()),
        "unknown event {}",
        request.event
    );
    if ["pull_request", "merge_group"].contains(&request.event.as_str()) {
        ensure!(
            gate == "pr",
            "PR/merge-group may plan only the hosted PR gate"
        );
    }
    ensure!(
        request.diff_complete,
        "incomplete or truncated diff: plan cannot authorize a gate"
    );
    if request.event != "local" {
        ensure!(
            request.repository_id != "local" && !request.repository_id.is_empty(),
            "plan missing repository ID"
        );
        ensure!(is_commit(&request.head_sha), "plan missing exact head SHA");
        if request.event == "pull_request" {
            ensure!(
                request.base_sha.as_ref().is_some_and(|s| is_commit(s)),
                "PR plan missing base SHA"
            );
        }
    }
    for p in &request.paths {
        ensure!(
            !p.starts_with('/') && !p.split('/').any(|s| s == "..") && !p.contains('\\'),
            "invalid changed path {p}"
        );
    }
    let full = super::selection::full_reason(m, gate, &request.event, &request.paths);
    let mut cells = Vec::new();
    for cell in manifest::validate::cells(m, gate)? {
        let (selected, reason) = if let Some(s) = m.suites.get(&cell.suite) {
            if super::selection::mandatory(&cell.suite, s) {
                (true, "always: unit/lint/parity or explicit policy".into())
            } else if let Some(reason) = &full {
                (true, reason.clone())
            } else if let Some(path) = request
                .paths
                .iter()
                .find(|path| s.paths.iter().any(|p| super::selection::matches(p, path)))
            {
                (true, format!("path match: {path}"))
            } else {
                (false, "not selected: no dependency path changed".into())
            }
        } else {
            (true, "always: build smoke".into())
        };
        let mut shards = request
            .suite_durations_ms
            .get(&cell.suite)
            .map_or(1, |ms| ms.div_ceil(120_000).clamp(1, 32) as u32);
        shards = shards.min(
            request
                .suite_test_counts
                .get(&cell.suite)
                .copied()
                .unwrap_or(1)
                .max(1),
        );
        if m.suites.get(&cell.suite).is_some_and(|s| s.serial) {
            shards = 1;
        }
        cells.push(PlannedCell {
            cell,
            selected,
            reason,
            shards,
        });
    }
    let build_ids: BTreeSet<_> = cells
        .iter()
        .filter(|c| c.selected)
        .map(|c| c.cell.build.clone())
        .collect();
    let builds = build_ids
        .into_iter()
        .map(|id| {
            let b = &m.builds[&id];
            let p = &m.platforms[&b.platform];
            let (runs_on, blocked_reason) = routing(m, &b.platform, &request);
            BuildMatrix {
                id,
                runs_on,
                triple: b.target.clone().unwrap_or_else(|| p.triple.clone()),
                toolchain: p.toolchain.clone(),
                kind: if b.kind == "test" {
                    "test-archive".into()
                } else {
                    b.kind.clone()
                },
                timeout: 30,
                blocked_reason,
            }
        })
        .collect();
    let mut groups: BTreeMap<(String, String, String, u32, u32), Vec<String>> = BTreeMap::new();
    for c in cells.iter().filter(|c| c.selected) {
        if let Some(s) = m.suites.get(&c.cell.suite).filter(|s| s.kind == "nextest") {
            for shard in 1..=c.shards {
                groups
                    .entry((
                        s.tier.clone(),
                        c.cell.build.clone(),
                        c.cell.platform.clone(),
                        shard,
                        c.shards,
                    ))
                    .or_default()
                    .push(c.cell.suite.clone());
            }
        }
    }
    let tests = groups
        .into_iter()
        .map(|((tier, build, platform, shard, shards), suites)| {
            let p = &m.platforms[&platform];
            let (runs_on, blocked_reason) = routing(m, &platform, &request);
            let caps: BTreeSet<_> = suites
                .iter()
                .flat_map(|id| {
                    m.tiers[&tier]
                        .guarantees
                        .iter()
                        .chain(&m.suites[id].requires)
                })
                .cloned()
                .collect();
            let host_resources = caps
                .into_iter()
                .filter(|c| {
                    [
                        "docker",
                        "local-registry",
                        "git-transports",
                        "apple-containers",
                        "docker-sbx",
                        "hypervisor",
                    ]
                    .contains(&c.as_str())
                })
                .map(|c| {
                    if c == "docker" {
                        "docker=unix:///var/run/docker.sock".to_owned()
                    } else if c == "local-registry" {
                        "local-registry=127.0.0.1:5000".to_owned()
                    } else {
                        c
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            TestMatrix {
                job: format!("{tier}-{build}-{platform}-{shard}"),
                tier,
                build,
                platform,
                runs_on,
                suites: suites.join(","),
                shard,
                shards,
                timeout: 15,
                self_hosted: p.trust != "hosted" && blocked_reason.is_none(),
                host_resources,
                blocked_reason,
            }
        })
        .collect();
    Ok(Plan {
        schema: 1,
        gate: gate.into(),
        manifest_version: m.version().into(),
        request,
        cells,
        builds,
        tests,
        quarantine: m.quarantine.iter().map(|q| q.test.clone()).collect(),
        overrides: m.overrides.iter().map(|o| o.cell.clone()).collect(),
    })
}
pub fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}
pub fn changed_paths(root: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    ensure!(
        is_commit(base) && is_commit(head),
        "base/head must be full commit SHAs"
    );
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", "-z", base, head, "--"])
        .output()
        .context("listing changed paths")?;
    ensure!(
        output.status.success(),
        "git diff failed; no reduced plan may be produced: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.stdout.is_empty() || output.stdout.ends_with(&[0]),
        "truncated git diff"
    );
    output
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| Ok(std::str::from_utf8(s)?.to_owned()))
        .collect()
}
