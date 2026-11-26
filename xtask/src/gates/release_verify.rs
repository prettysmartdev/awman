//! Authenticate service evidence independently before authorizing release reuse.
use crate::{
    evidence::{actions, digest, schema::Outcome},
    manifest,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::ExitCode,
};

pub fn run(root: &Path, plan_file: &Path, repo: &str, run_id: u64, out: &Path) -> Result<ExitCode> {
    let mut envelope = json!({"schema":1,"outcome":"blocked","reason_code":"evidence-unavailable",
        "repository_id":null,"run_id":run_id,"run_attempt":null,"commit":null,"tree":null,
        "manifest_sha256":null,"plan_sha256":null,"policy_sha256":null,"workflow":null,"cells":[],"artifacts":[]});
    let result = verify(root, plan_file, repo, run_id, &mut envelope);
    let code = match result {
        Ok(()) => {
            envelope["outcome"] = json!("passed");
            envelope["reason_code"] = Value::Null;
            0
        }
        Err(e) => {
            // API/transport and missing infrastructure remain distinguishable
            // from an authenticated failed assertion or integrity violation.
            let unavailable = e.chain().any(|e| e.is::<std::io::Error>())
                || e.to_string().starts_with("service unavailable:");
            envelope["outcome"] = json!(if unavailable { "blocked" } else { "failed" });
            envelope["reason_code"] = json!(if unavailable {
                "evidence-unavailable"
            } else {
                "evidence-invalid"
            });
            envelope["reason"] = json!(format!("{e:#}"));
            if unavailable {
                2
            } else {
                1
            }
        }
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, serde_json::to_vec_pretty(&envelope)?)?;
    Ok(ExitCode::from(code))
}
/// Authenticate the exact matrix job that produced one retained shard artifact.
///
/// `jobs` is the service listing for `producer_attempt` of run `run_id`. Only
/// this shard's own cell job must have completed successfully in that attempt:
/// a sibling that failed there was rerun in a later attempt and is credited
/// through its own artifact. Returns the producing job's service ID.
pub(crate) fn producer_job(
    jobs: &[Value],
    title: &str,
    index: usize,
    run_id: u64,
    producer_attempt: u64,
    verified_attempt: u64,
) -> Result<u64> {
    ensure!(
        (1..=verified_attempt).contains(&producer_attempt),
        "producer attempt {producer_attempt} is outside run attempts 1..={verified_attempt}"
    );
    let expected = format!("{title} (cell {index})");
    let matching: Vec<_> = jobs
        .iter()
        .filter(|j| {
            j["name"]
                .as_str()
                .is_some_and(|n| n.rsplit(" / ").next() == Some(expected.as_str()))
        })
        .collect();
    ensure!(
        matching.len() == 1,
        "producer job {expected:?} missing or ambiguous in attempt {producer_attempt}"
    );
    let job = matching[0];
    ensure!(
        job["status"] == "completed" && job["conclusion"] == "success",
        "producer job {expected:?} did not succeed in attempt {producer_attempt}"
    );
    ensure!(
        job["run_id"] == run_id && job["run_attempt"] == producer_attempt,
        "producer job {expected:?} belongs to another run or attempt"
    );
    actions::number(job, "id")
}

fn verify(root: &Path, plan_file: &Path, repo: &str, run_id: u64, out: &mut Value) -> Result<()> {
    ensure!(
        repo.split('/').count() == 2
            && repo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)),
        "invalid repository"
    );
    let m = manifest::load(root)?;
    let release_plan: Value = serde_json::from_slice(&std::fs::read(plan_file)?)?;
    ensure!(
        release_plan == super::release_plan::create(&m)?,
        "release plan differs from pinned source manifest"
    );
    let plan_sha = digest::sha256_file(plan_file)?;
    let source = actions::source(root)?;
    ensure!(source["clean"] == true, "pinned source is dirty");
    let repository = actions::api(&format!("repos/{repo}"))?;
    let run = actions::api(&format!("repos/{repo}/actions/runs/{run_id}"))?;
    let attempt = actions::number(&run, "run_attempt")?;
    ensure!(
        run["repository"]["id"] == repository["id"],
        "wrong repository identity"
    );
    ensure!(
        run["path"] == ".github/workflows/ci.yml"
            && run["status"] == "completed"
            && run["conclusion"] == "success",
        "CI workflow not successful"
    );
    let artifacts = actions::pages(
        &format!("repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"),
        "artifacts",
    )?;
    let jobs = actions::pages(
        &format!("repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"),
        "jobs",
    )?;
    actions::job_passed(&jobs, "Plan", 1)?;
    actions::job_passed(&jobs, "gate", 1)?;
    let scratch = tempfile::tempdir()?;
    // First locate the unique newest gate upload. It is not proof by itself:
    // every selected release cell is rechecked against raw authenticated bytes.
    let candidates: Vec<_> = artifacts
        .iter()
        .filter(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("gate-evidence-"))
        })
        .collect();
    let names: BTreeSet<_> = candidates
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    ensure!(names.len() == 1, "missing or ambiguous gate evidence");
    let gate_artifact =
        actions::artifact(&artifacts, names.first().context("missing gate artifact")?)?;
    let gate_dir = scratch.path().join("gate");
    actions::download(repo, gate_artifact, &gate_dir)?;
    let gate: crate::evidence::schema::GateEvidence =
        serde_json::from_slice(&std::fs::read(gate_dir.join("gate-evidence.json"))?)?;
    ensure!(
        gate.schema == 1
            && gate.verdict == Outcome::Passed
            && gate.errors.is_empty()
            && gate.missing.is_empty(),
        "gate is not a complete pass"
    );
    ensure!(
        gate.expires_at.is_some_and(|t| t > chrono::Utc::now()),
        "gate evidence expired"
    );
    ensure!(gate.manifest_version == m.version(), "manifest differs");
    let commit = gate.commit.as_str();
    ensure!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid tested commit"
    );
    let tested = actions::api(&format!("repos/{repo}/git/commits/{commit}"))?;
    let head = actions::api(&format!(
        "repos/{repo}/git/commits/{}",
        actions::text(&run, "head_sha")?
    ))?;
    ensure!(
        tested["tree"]["sha"] == source["tree"],
        "tested source tree differs from R"
    );
    // Dispatch override/PR merge must have a service-authenticated relationship
    // to the run's head. Arbitrary unrelated artifact commits cannot be borrowed.
    if run["event"] == "pull_request" {
        ensure!(
            tested["parents"]
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| p["sha"] == run["head_sha"])),
            "PR merge source is unrelated to service head"
        );
    } else {
        ensure!(
            head["tree"]["sha"] == tested["tree"]["sha"],
            "run head/tree mismatch"
        );
    }
    let prefix = format!("plan-{}-", gate.gate);
    let plan_names: BTreeSet<_> = artifacts
        .iter()
        .filter_map(|a| a["name"].as_str())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    ensure!(plan_names.len() == 1, "missing or ambiguous plan");
    let plan_artifact = actions::artifact(&artifacts, plan_names.first().context("missing plan")?)?;
    let plan_dir = scratch.path().join("plan");
    actions::download(repo, plan_artifact, &plan_dir)?;
    let p: super::plan::Plan = serde_json::from_slice(&std::fs::read(plan_dir.join("plan.json"))?)?;
    ensure!(
        p.schema == 1
            && p.request.head_sha == commit
            && p.request.repository_id == actions::number(&repository, "id")?.to_string(),
        "plan source/repository mismatch"
    );
    ensure!(
        p.digest()? == gate.plan_sha256 && p.manifest_version == m.version(),
        "plan digest/manifest mismatch"
    );
    let mut request = p.request.clone();
    request.event = actions::text(&run, "event")?.into();
    if let Some(base) = &request.base_sha {
        request.paths = super::plan::changed_paths(root, base, commit)?;
    }
    request.admitted_platforms.clear();
    ensure!(
        super::plan::create(&m, &p.gate, request, chrono::Utc::now().date_naive())? == p,
        "plan differs from recomputed source selection"
    );
    let summary_cells: BTreeSet<_> = gate.cells.iter().map(|c| &c.cell).collect();
    ensure!(
        summary_cells.len() == gate.cells.len()
            && summary_cells == p.cells.iter().map(|c| &c.cell.id).collect(),
        "gate summary inventory has missing, extra or duplicate cells"
    );
    let required = manifest::validate::cells(&m, "release")?;
    for cell in &required {
        ensure!(
            p.cells.iter().any(|c| c.selected && &c.cell == cell),
            "required release cell unselected: {}",
            cell.id
        );
        ensure!(
            gate.cells
                .iter()
                .any(|c| c.cell == cell.id && c.outcome == Outcome::Passed),
            "required release cell did not pass: {}",
            cell.id
        );
    }
    let mut evidence_artifacts = vec![];
    let mut verified_cells = vec![];
    let matrices = crate::ci::plan_outputs::outputs(&m, &p)?;
    let mode = actions::text(plan_artifact, "name")?
        .strip_prefix(&prefix)
        .context("plan prefix")?
        .rsplit_once('-')
        .context("plan mode")?
        .0;
    let mut actual = crate::evidence::inventory::Inventories::new();
    // Service job listings per attempt, fetched once; the verified attempt's is already known.
    let mut attempt_jobs = BTreeMap::from([(attempt, jobs.clone())]);
    // Each retained shard must be credited to its own distinct producing job.
    let mut producers = BTreeSet::new();
    let mut tested_source = source.clone();
    tested_source["commit"] = json!(commit);
    let (independent, inventory_receipts) = crate::evidence::inventory::authenticate(
        repo,
        &artifacts,
        &p,
        &m,
        &tested_source,
        mode,
        scratch.path(),
    )?;
    evidence_artifacts.extend(inventory_receipts);
    for (job, matrix) in matrices["shards"].as_object().context("missing shards")? {
        let t = p
            .tests
            .iter()
            .find(|t| {
                crate::gates::render::workflows::test_job_id(&m, &t.tier, &t.platform, &t.build)
                    == *job
            })
            .context("unknown job")?;
        let entries = matrix.as_array().context("invalid shards")?;
        actions::job_passed(
            &jobs,
            &format!("Test {} on {}", t.tier, t.platform),
            entries.len(),
        )?;
        for (index, entry) in entries.iter().enumerate() {
            let suites: BTreeSet<String> = actions::text(entry, "suites")?
                .split(',')
                .map(str::to_owned)
                .collect();
            if !required
                .iter()
                .any(|c| c.build == t.build && suites.contains(&c.suite))
            {
                continue;
            }
            let name = format!("evidence-{}-{mode}-{job}-m{index}", p.gate);
            let a = actions::artifact(&artifacts, &name)?;
            let dir = scratch.path().join(&name);
            actions::download(repo, a, &dir)?;
            let producer =
                json!({"run_id":run_id.to_string(),"attempt":attempt.to_string(),"job":job});
            let shard = format!(
                "{}/{}",
                actions::number(entry, "i")?,
                actions::number(entry, "n")?
            );
            actions::check_run(
                &dir,
                &suites,
                &t.build,
                Some(&shard),
                &tested_source,
                &producer,
                &m,
            )?;
            crate::evidence::inventory::merge(
                &mut actual,
                &t.build,
                crate::evidence::inventory::raw(&dir)?,
                &independent,
            )?;
            let raw: Value = serde_json::from_slice(&std::fs::read(dir.join("run.json"))?)?;
            let producer_attempt = actions::text(&raw["producer"], "attempt")?.parse::<u64>()?;
            ensure!(
                (1..=attempt).contains(&producer_attempt),
                "{name}: producer attempt {producer_attempt} is outside run attempts 1..={attempt}"
            );
            if let std::collections::btree_map::Entry::Vacant(slot) =
                attempt_jobs.entry(producer_attempt)
            {
                slot.insert(actions::pages(
                    &format!("repos/{repo}/actions/runs/{run_id}/attempts/{producer_attempt}/jobs?per_page=100"),
                    "jobs",
                )?);
            }
            let title = format!("Test {} on {}", t.tier, t.platform);
            let job_id = producer_job(
                &attempt_jobs[&producer_attempt],
                &title,
                index,
                run_id,
                producer_attempt,
                attempt,
            )
            .with_context(|| name.clone())?;
            ensure!(
                producers.insert(job_id),
                "{name}: service job {job_id} already credited to another shard"
            );
            evidence_artifacts.push(json!({"id":a["id"],"service_digest":a["digest"],"downloaded_digest":a["digest"],"run_id":run_id,"job_id":job_id,"producer_attempt":producer_attempt}));
            for inv in raw["invocations"]
                .as_array()
                .context("invocations missing")?
            {
                let suite = inv["suites"][0].as_str().context("suite missing")?;
                if !required
                    .iter()
                    .any(|c| c.build == t.build && c.suite == suite)
                {
                    continue;
                }
                let file = actions::file_in(&dir, actions::text(inv, "expected")?)?;
                let inventory: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
                let tests = actions::expected(&inventory)?;
                verified_cells.push(json!({"suite":suite,"build":t.build,"platform":t.platform,"shard":shard,"expected_inventory_sha256":digest::sha256_file(&file)?,"tests":tests,"job_id":job_id,"producer_attempt":producer_attempt}));
            }
        }
    }
    for c in &required {
        crate::evidence::inventory::complete(
            &actual,
            &independent,
            &format!("{}@{}", c.suite, c.build),
        )?;
    }
    let counts: BTreeMap<_, _> = required
        .iter()
        .map(|c| {
            (
                c.id.clone(),
                verified_cells
                    .iter()
                    .filter(|v| v["suite"] == c.suite && v["build"] == c.build)
                    .count(),
            )
        })
        .collect();
    ensure!(
        required.iter().all(|c| p
            .cells
            .iter()
            .any(|pc| pc.cell == *c && counts[&c.id] == pc.shards as usize)),
        "incomplete release shard inventory"
    );
    *out = json!({"schema":1,"outcome":"passed","reason_code":null,"repository_id":repository["id"],"run_id":run_id,"run_attempt":attempt,"commit":commit,"source_commit":source["commit"],"tree":source["tree"],"manifest_sha256":m.version(),"plan_sha256":plan_sha,"policy_sha256":crate::manifest::release_policy::digest(&m)?,"workflow":{"path":".github/workflows/ci.yml","revision":run["head_sha"]},"cells":verified_cells,"artifacts":evidence_artifacts});
    Ok(())
}
