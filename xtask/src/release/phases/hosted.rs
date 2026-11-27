//! `push`, `dispatch`, `collect` and `native` (design 3.3, 3.4, 7.6, 7.7).

use crate::release::error::{stop, Step, Stop};
use crate::release::forge::{RefWrite, RunInfo};
use crate::release::journal::{Dispatch, KitRecord};
use crate::release::native::{self, NativeJob};
use crate::release::run::Ctx;
use crate::release::util::{read_json, sha256, sha256_file};
use crate::release::world::VerifiedCi;
use std::collections::BTreeMap;

/// Upper bound for hosted work in one invocation; the state is kept.
const HOSTED_LIMIT_MINUTES: i64 = 150;
/// How long a dispatched run may take to become visible.
const VISIBILITY_SECONDS: i64 = 180;

fn branch(ctx: &Ctx) -> String {
    ctx.j.push_ref.trim_start_matches("heads/").to_string()
}

pub fn push(ctx: &mut Ctx) -> Step {
    let r = ctx.j.commit.clone();
    if ctx.w.forge.get_ref(&ctx.j.push_ref)?.as_deref() == Some(r.as_str()) {
        return Ok(());
    }
    match ctx.w.forge.push(&r, &ctx.j.push_ref)? {
        RefWrite::Done(_) => Ok(()),
        RefWrite::Rejected(cur) => Err(Stop::new(
            "state-foreign",
            format!(
                "refs/{} points at {}, not the release commit {}",
                ctx.j.push_ref,
                cur.as_deref().unwrap_or("?"),
                &r[..12]
            ),
        )
        .with_remedy(format!(
            "inspect refs/{}; never force it, then run make release SUPERSEDE=1",
            ctx.j.push_ref
        ))),
    }
}

#[derive(Clone)]
enum Slot {
    Ci,
    Build(String),
}

fn slot<'c>(ctx: &'c mut Ctx, s: &Slot) -> &'c mut Dispatch {
    match s {
        Slot::Ci => &mut ctx.j.ci.dispatch,
        Slot::Build(a) => &mut ctx.j.builds.entry(a.clone()).or_default().dispatch,
    }
}

fn workflow(ctx: &Ctx, s: &Slot) -> String {
    match s {
        Slot::Ci => ctx.plan.ci.workflow.clone(),
        Slot::Build(_) => ctx.plan.release_workflow.clone(),
    }
}

fn key(ctx: &Ctx, s: &Slot) -> String {
    match s {
        Slot::Ci => ctx.j.key("ci"),
        Slot::Build(a) => format!("build-{}-{a}", ctx.j.short()),
    }
}

fn inputs(ctx: &Ctx, s: &Slot) -> BTreeMap<String, String> {
    match s {
        Slot::Ci => {
            let mut i = ctx.plan.ci.inputs.clone();
            i.insert("key".into(), key(ctx, s));
            i
        }
        Slot::Build(a) => BTreeMap::from([
            ("asset".to_string(), a.clone()),
            ("version".to_string(), ctx.j.version.clone()),
            ("commit".to_string(), ctx.j.commit.clone()),
            ("key".to_string(), key(ctx, s)),
        ]),
    }
}

/// Runs carrying this key in their run-name, oldest first. A run for any
/// other commit never matches.
fn matching(ctx: &Ctx, s: &Slot) -> Step<Vec<RunInfo>> {
    let k = key(ctx, s);
    let mut runs: Vec<_> = ctx
        .w
        .forge
        .runs(&workflow(ctx, s), &ctx.j.commit)?
        .into_iter()
        .filter(|r| r.title.split_whitespace().any(|t| t == k))
        .collect();
    runs.sort_by_key(|r| r.id);
    Ok(runs)
}

/// Pin the canonical run (lowest ID); cancel our own duplicates so their
/// artifacts can never mix into the seal.
fn attach(ctx: &mut Ctx, s: &Slot, runs: &[RunInfo]) -> Step {
    let canonical = runs[0].id;
    let dups: Vec<u64> = runs[1..].iter().map(|r| r.id).collect();
    for d in &dups {
        ctx.w.forge.cancel(*d)?;
    }
    let d = slot(ctx, s);
    d.run_id = Some(canonical);
    d.acked = true;
    d.duplicates = dups;
    let name = match s {
        Slot::Ci => "ci",
        Slot::Build(a) => a.as_str(),
    };
    ctx.progress(
        name,
        format!(
            "{name}: run {canonical} https://github.com/{}/actions/runs/{canonical} attached",
            ctx.j.repo
        ),
    );
    ctx.save()
}

/// Reuse authenticated passing CI evidence for R (design 3.3, 7.2).
fn reuse_ci(ctx: &mut Ctx) -> Step<bool> {
    let mut runs: Vec<_> = ctx
        .w
        .forge
        .runs(&ctx.plan.ci.workflow, &ctx.j.commit)?
        .into_iter()
        .filter(|r| r.succeeded())
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.id));
    for run in runs.into_iter().take(5) {
        if let Ok(v) = verify_ci(ctx, &run) {
            ctx.j.ci.dispatch = Dispatch {
                key: key(ctx, &Slot::Ci),
                acked: true,
                run_id: Some(run.id),
                ..Default::default()
            };
            ctx.j.ci.reused = true;
            record_ci(ctx, &run, &v.1);
            ctx.progress(
                "ci",
                format!(
                    "ci: reused verified run {} https://github.com/{}/actions/runs/{}",
                    run.id, ctx.j.repo, run.id
                ),
            );
            ctx.store.log(
                "dispatch",
                &format!("reused CI run {} for {}", run.id, &ctx.j.commit[..12]),
            );
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn dispatch(ctx: &mut Ctx) -> Step {
    if ctx.j.ci.dispatch.run_id.is_none()
        && ctx.j.ci.dispatch.pending_since.is_none()
        && reuse_ci(ctx)?
    {
        ctx.save()?;
    }
    let mut slots = vec![Slot::Ci];
    slots.extend(ctx.plan.assets.iter().map(|a| Slot::Build(a.asset.clone())));
    let git_ref = branch(ctx);
    // Pass 1: attach visible runs, otherwise persist intent, then send.
    for s in &slots {
        if slot(ctx, s).run_id.is_some() {
            continue;
        }
        let found = matching(ctx, s)?;
        if !found.is_empty() {
            attach(ctx, s, &found)?;
            continue;
        }
        let now = ctx.w.clock.now();
        let k = key(ctx, s);
        let d = slot(ctx, s);
        d.key = k;
        d.pending_since = Some(now);
        d.acked = false;
        let name = match s {
            Slot::Ci => "ci",
            Slot::Build(a) => a.as_str(),
        };
        ctx.progress(name, format!("{name}: waiting for Actions run to appear"));
        ctx.save()?;
        let wf = workflow(ctx, s);
        // The revision that will run is the file at the pushed release ref.
        if ctx.w.forge.workflow_revision(&wf, &git_ref)?.is_none() {
            stop!("workflow-missing", "{wf} does not exist at {git_ref}");
        }
        match ctx.w.forge.dispatch(&wf, &git_ref, &inputs(ctx, s)) {
            Ok(()) => {
                slot(ctx, s).acked = true;
                ctx.save()?;
            }
            // Uncertain delivery: keep the intent; visibility decides.
            Err(e) if e.code == "network" => ctx.store.log(
                "dispatch",
                &format!("uncertain dispatch of {}: {}", key(ctx, s), e.message),
            ),
            Err(e) => return Err(e),
        }
    }
    // Pass 2: bounded wait for every run to become visible.
    let started = ctx.w.clock.now();
    loop {
        let mut missing = Vec::new();
        for s in &slots {
            if slot(ctx, s).run_id.is_some() {
                continue;
            }
            let found = matching(ctx, s)?;
            if found.is_empty() {
                missing.push(key(ctx, s));
            } else {
                attach(ctx, s, &found)?;
            }
        }
        if missing.is_empty() {
            break;
        }
        if (ctx.w.clock.now() - started).num_seconds() > VISIBILITY_SECONDS {
            stop!(
                "dispatch-uncertain",
                "no run became visible for {} within {VISIBILITY_SECONDS} s",
                missing.join(", ")
            );
        }
        ctx.w.interrupted()?;
        ctx.w.clock.sleep(5);
    }
    ctx.update_claim(None)
}

fn verify_ci(ctx: &mut Ctx, run: &RunInfo) -> Step<(VerifiedCi, Vec<u8>)> {
    ctx.event(&format!("verifying CI evidence for run {}", run.id));
    let src = ctx.source_dir()?;
    let out = ctx
        .store
        .path(&format!("ci/verified-{}-{}.json", run.id, run.attempt));
    let (code, bytes) = ctx
        .w
        .tools
        .verify_ci(&src, &ctx.plan_path(), &ctx.j.repo, run.id, &out)?;
    match code {
        0 => {}
        1 => stop!(
            "ci-evidence-invalid",
            "run {} evidence failed verification: {}",
            run.id,
            String::from_utf8_lossy(&bytes)
                .lines()
                .find(|l| l.contains("reason_code"))
                .unwrap_or("")
                .trim()
        ),
        _ => stop!(
            "ci-unavailable",
            "CI evidence of run {} is unavailable",
            run.id
        ),
    }
    let v: VerifiedCi = serde_json::from_slice(&bytes).map_err(|e| {
        Stop::new(
            "ci-evidence-invalid",
            format!("verified evidence of run {}: {e}", run.id),
        )
    })?;
    let j = &ctx.j;
    let bound = v.schema == 1
        && v.outcome == "passed"
        && v.repository_id == j.repository_id
        && v.run_id == run.id
        && v.run_attempt == run.attempt
        && v.tree == j.tree
        && v.commit == j.commit
        && v.manifest_sha256 == j.manifest_sha256
        && v.plan_sha256 == j.plan_sha256
        && v.policy_sha256 == j.policy_sha256
        && !v.cells.is_empty();
    if !bound {
        stop!(
            "ci-evidence-invalid",
            "run {} evidence is not bound to this release's repository, tree, plan and policy",
            run.id
        );
    }
    Ok((v, bytes))
}

fn report_hosted(ctx: &Ctx) {
    let mut items = vec![(
        "ci".to_string(),
        ctx.j.ci.dispatch.run_id,
        ctx.j.ci.verified_sha256.is_some(),
    )];
    items.extend(ctx.plan.assets.iter().map(|a| {
        (
            a.asset.clone(),
            ctx.j.builds.get(&a.asset).and_then(|b| b.dispatch.run_id),
            ctx.j
                .builds
                .get(&a.asset)
                .is_some_and(|b| b.validated.is_some()),
        )
    }));
    for (item, id, verified) in items {
        let Some(id) = id else {
            ctx.progress(&item, format!("{item}: waiting for run ID"));
            continue;
        };
        let link = format!("https://github.com/{}/actions/runs/{id}", ctx.j.repo);
        if verified {
            ctx.progress(&item, format!("{item}: run {id} {link} evidence verified"));
            continue;
        }
        // Status reporting must never change the evidence or the release outcome.
        let Some((run, jobs)) = ctx.w.forge.progress_snapshot(id) else {
            continue;
        };
        let mut detail = format!("{item}: run {id} {link} {}", run.status);
        if let Some(conclusion) = &run.conclusion {
            detail.push_str(&format!("/{conclusion}"));
        }
        for job in jobs {
            detail.push_str(&format!(
                "; {} {}",
                super::super::progress::service_label(&job.name),
                job.status
            ));
            if let Some(step) = job.active_step {
                detail.push_str(&format!(
                    " [{}]",
                    super::super::progress::service_label(&step)
                ));
            }
        }
        ctx.progress(&item, detail);
    }
}

fn record_ci(ctx: &mut Ctx, run: &RunInfo, bytes: &[u8]) {
    ctx.j.ci.verified_sha256 = Some(sha256(bytes));
    ctx.j.ci.run_attempt = run.attempt;
    ctx.j.ci.completed = Some(run.updated);
}

/// Actions whose steps only move bytes or talk to a GitHub service. Their
/// failure after successful compilation/tests is transport, not product.
const TRANSPORT_ACTIONS: &[&str] = &[
    "actions/checkout",
    "actions/upload-artifact",
    "actions/download-artifact",
    "actions/attest-build-provenance",
    "actions/cache",
    "dtolnay/rust-toolchain",
];

/// Display names of the transport steps of `workflow` as defined at R. An
/// unnamed step is shown by Actions as `Run <uses>`.
fn transport_steps(ctx: &Ctx, workflow: &str) -> Vec<String> {
    let file = format!(".github/workflows/{workflow}");
    let Ok(Some(bytes)) = ctx.w.source.file_at(&ctx.j.commit, &file) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml::from_slice::<serde_yaml::Value>(&bytes) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let jobs = doc["jobs"].as_mapping().cloned().unwrap_or_default();
    for (_, job) in jobs {
        for step in job["steps"].as_sequence().cloned().unwrap_or_default() {
            let Some(uses) = step["uses"].as_str() else {
                continue;
            };
            let action = uses.split('@').next().unwrap_or_default();
            if TRANSPORT_ACTIONS.contains(&action) {
                names.push(
                    step["name"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Run {uses}")),
                );
            }
        }
    }
    names
}

/// The runner's own setup/teardown steps, and transport steps (also their
/// `Post` halves). A name with an expression matches on its literal prefix.
fn is_transport(step: &str, roles: &[String]) -> bool {
    if matches!(step, "Set up job" | "Complete job") {
        return true;
    }
    let step = step.strip_prefix("Post ").unwrap_or(step);
    roles.iter().any(|role| match role.split_once("${{") {
        Some((prefix, _)) => !prefix.is_empty() && step.starts_with(prefix),
        None => step == role,
    })
}

/// Classify a finished, unsuccessful run (design 7.6). Returns `Ok(())`
/// after the one automatic rerun of an infrastructure failure.
fn failed_run(ctx: &mut Ctx, item: &str, run: &RunInfo, product_code: &str) -> Step {
    let conclusion = run.conclusion.clone().unwrap_or_default();
    if conclusion == "startup_failure" {
        stop!(
            "workflow-rejected",
            "{} run {} was rejected before any job started (startup_failure)",
            run.workflow,
            run.id
        );
    }
    let jobs = ctx.w.forge.jobs(run.id)?;
    let failed: Vec<_> = jobs
        .iter()
        .filter(|j| {
            matches!(
                j.conclusion.as_deref(),
                Some("failure" | "cancelled" | "timed_out")
            )
        })
        .collect();
    // A failed step counts as infrastructure only when the workflow at R
    // (trusted, pinned source) gives it a transport/service role; any other
    // failed step, or an unknown one, is a product failure (fail closed).
    let roles = transport_steps(ctx, &run.workflow);
    let only_transport = |j: &&crate::release::forge::JobInfo| {
        !j.failed_steps.is_empty() && j.failed_steps.iter().all(|s| is_transport(s, &roles))
    };
    if let Some(job) = failed.iter().find(|j| {
        j.conclusion.as_deref() == Some("failure")
            && !j.failed_steps.is_empty()
            && !only_transport(j)
    }) {
        return Err(Stop::new(
            product_code,
            format!(
                "{item}: job `{}` step `{}` failed in run {} attempt {}",
                job.name, job.failed_steps[0], run.id, run.attempt
            ),
        ));
    }
    let infra = !failed.is_empty()
        && failed.iter().all(|j| {
            only_transport(j)
                || (j.failed_steps.is_empty() && j.conclusion.as_deref() != Some("timed_out"))
        });
    if !infra {
        stop!("build-infrastructure", "{item}: run {} ended `{conclusion}` without a classifiable cause; inspect it before retrying", run.id);
    }
    if ctx.retried.insert(item.to_string()) {
        ctx.store.log(
            "collect",
            &format!(
                "{item}: infrastructure failure in run {} attempt {}; rerunning failed jobs once",
                run.id, run.attempt
            ),
        );
        ctx.w.forge.rerun_failed(run.id)?;
        match item {
            "ci" => ctx.j.ci.auto_retried = true,
            a => {
                ctx.j
                    .builds
                    .entry(a.trim_start_matches("build.").to_string())
                    .or_default()
                    .auto_retried = true
            }
        }
        ctx.save()?;
        return Ok(());
    }
    stop!(
        "build-infrastructure",
        "{item}: run {} lost its runner again (attempt {})",
        run.id,
        run.attempt
    )
}

fn ci_evidence_path(ctx: &Ctx) -> std::path::PathBuf {
    ctx.store.path(&format!(
        "ci/verified-{}-{}.json",
        ctx.j.ci.dispatch.run_id.unwrap_or(0),
        ctx.j.ci.run_attempt
    ))
}

/// The retained verified-CI file must still exist with its recorded digest.
/// The journal records completion but is never the proof (design 7.4): a
/// missing or altered file is re-verified from the pinned run's service
/// record; nothing is rerun. The original completion time is kept, so
/// re-verification never resets the evidence age.
fn ensure_ci_evidence(ctx: &mut Ctx) -> Step {
    ctx.event("checking retained CI evidence");
    let Some(recorded) = ctx.j.ci.verified_sha256.clone() else {
        stop!("state-corrupt", "CI evidence was never verified");
    };
    if sha256_file(&ci_evidence_path(ctx)).ok().as_deref() == Some(recorded.as_str()) {
        return Ok(());
    }
    let id = ctx
        .j
        .ci
        .dispatch
        .run_id
        .ok_or_else(|| Stop::new("state-corrupt", "CI run was never pinned"))?;
    ctx.store.log(
        "collect",
        &format!("retained CI evidence of run {id} is missing or altered; re-verifying it"),
    );
    let run = ctx.w.forge.run(id)?;
    if !run.succeeded() || run.attempt != ctx.j.ci.run_attempt {
        stop!(
            "hosted-evidence-missing",
            "retained CI evidence of run {id} attempt {} is gone and the run no longer shows that passing attempt",
            ctx.j.ci.run_attempt
        );
    }
    let (_, bytes) = verify_ci(ctx, &run)?;
    let completed = ctx.j.ci.completed;
    record_ci(ctx, &run, &bytes);
    ctx.j.ci.completed = completed.or(ctx.j.ci.completed);
    ctx.save()
}

/// The retained asset bytes and `build.json` must match their recorded
/// digests; otherwise they are re-downloaded from the pinned artifact ID
/// (never rebuilt) and must reproduce exactly the recorded digests.
fn ensure_build_evidence(ctx: &mut Ctx, asset: &str) -> Step {
    ctx.event(&format!(
        "{asset}: checking retained build bytes and record"
    ));
    let b = ctx.j.builds.get(asset).cloned().unwrap_or_default();
    let (Some(sha), Some(json_sha)) = (b.sha256.clone(), b.build_json_sha256.clone()) else {
        stop!("state-corrupt", "{asset} was never validated");
    };
    let kept = ctx.store.path(&format!("assets/{asset}"));
    let dir = ctx.store.path(&format!("assets/{asset}.d"));
    let build_json = dir.join("build.json");
    let intact = |p: &std::path::Path, want: &str| sha256_file(p).ok().as_deref() == Some(want);
    if intact(&kept, &sha) && intact(&dir.join(asset), &sha) && intact(&build_json, &json_sha) {
        return Ok(());
    }
    let (Some(run_id), Some(artifact_id)) = (b.dispatch.run_id, b.artifact_id) else {
        stop!("state-corrupt", "{asset} has no pinned run or artifact");
    };
    ctx.store.log(
        "collect",
        &format!("retained evidence of {asset} is missing or altered; re-downloading artifact {artifact_id}"),
    );
    let Some(a) = ctx
        .w
        .forge
        .artifacts(run_id)?
        .into_iter()
        .find(|a| a.id == artifact_id && !a.expired)
    else {
        stop!(
            "hosted-evidence-missing",
            "{asset}: retained evidence is gone and pinned artifact {artifact_id} of run {run_id} is no longer available"
        );
    };
    let _ = std::fs::remove_dir_all(&dir);
    ctx.w.forge.download_artifact(run_id, &a, &dir)?;
    if !intact(&dir.join(asset), &sha) || !intact(&build_json, &json_sha) {
        stop!(
            "build-evidence-invalid",
            "{asset}: pinned artifact {artifact_id} no longer reproduces the recorded asset and build.json digests"
        );
    }
    let v: serde_json::Value = read_json(&build_json)
        .map_err(|e| Stop::new("build-evidence-invalid", format!("{asset}: {e:#}")))?;
    check_build_json(ctx, asset, &sha, b.size, &v)?;
    if !intact(&kept, &sha) {
        std::fs::copy(dir.join(asset), &kept).map_err(anyhow::Error::from)?;
    }
    Ok(())
}

/// Every retained hosted proof the seal freezes, re-authenticated.
pub(super) fn verify_hosted_records(ctx: &mut Ctx) -> Step {
    ensure_ci_evidence(ctx)?;
    let assets: Vec<String> = ctx.plan.assets.iter().map(|a| a.asset.clone()).collect();
    for a in &assets {
        ensure_build_evidence(ctx, a)?;
    }
    Ok(())
}

fn step_ci(ctx: &mut Ctx) -> Step<bool> {
    if ctx.j.ci.verified_sha256.is_some() {
        ensure_ci_evidence(ctx)?;
        return Ok(true);
    }
    let id = ctx
        .j
        .ci
        .dispatch
        .run_id
        .ok_or_else(|| Stop::new("state-corrupt", "CI run was never pinned"))?;
    let run = ctx.w.forge.run(id)?;
    if !run.completed() {
        return Ok(false);
    }
    if !run.succeeded() {
        failed_run(ctx, "ci", &run, "ci-failed")?;
        return Ok(false);
    }
    let (_, bytes) = verify_ci(ctx, &run)?;
    record_ci(ctx, &run, &bytes);
    service_stage(ctx, "ci", &run);
    ctx.save()?;
    Ok(true)
}

/// The service-side stage of a hosted run in the timing ledger: when the
/// service created it (dispatch accepted, queue begins) and when its final
/// attempt finished (design 7.12).
fn service_stage(ctx: &Ctx, item: &str, run: &RunInfo) {
    ctx.timing(
        "collect",
        "service",
        &format!(
            "{item} run {} attempt {} created {} finished {} ({} s)",
            run.id,
            run.attempt,
            run.created.to_rfc3339(),
            run.updated.to_rfc3339(),
            (run.updated - run.created).num_seconds()
        ),
    );
}

/// `build.json` must bind the exact bytes, commit, tree and a complete
/// passing smoke inventory (design 7.2).
fn check_build_json(ctx: &Ctx, asset: &str, sha: &str, size: u64, v: &serde_json::Value) -> Step {
    let invalid = |m: &str| {
        Err(Stop::new(
            "build-evidence-invalid",
            format!("{asset}: build.json {m}"),
        ))
    };
    if v["schema"] != 1
        || v["asset"] != asset
        || v["sha256"] != sha
        || v["size"].as_u64() != Some(size)
    {
        return invalid("does not bind these exact bytes");
    }
    if v["commit"] != ctx.j.commit.as_str() || v["tree"] != ctx.j.tree.as_str() {
        return invalid("names another commit or tree");
    }
    let expected: Vec<&str> = v["smoke"]["expected"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    let results = v["smoke"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if expected.is_empty() {
        return invalid("has an empty smoke inventory");
    }
    let mut seen = std::collections::BTreeSet::new();
    for r in &results {
        let t = r["test"].as_str().unwrap_or("");
        if !seen.insert(t) {
            return invalid(&format!("reports {t} twice"));
        }
        if r["outcome"] != "passed" {
            return invalid(&format!("reports {t} as {}", r["outcome"]));
        }
    }
    if seen != expected.iter().copied().collect() || expected.len() != seen.len() {
        return invalid("smoke results differ from the expected inventory");
    }
    if ctx.plan.asset(asset).is_some_and(|a| a.abi.is_some()) && v["abi"]["ok"] != true {
        return invalid("has no passing ABI check");
    }
    Ok(())
}

fn step_build(ctx: &mut Ctx, asset: &str) -> Step<bool> {
    let b = ctx.j.builds.get(asset).cloned().unwrap_or_default();
    if b.validated.is_some() {
        ensure_build_evidence(ctx, asset)?;
        return Ok(true);
    }
    let id = b
        .dispatch
        .run_id
        .ok_or_else(|| Stop::new("state-corrupt", format!("{asset} run was never pinned")))?;
    let run = ctx.w.forge.run(id)?;
    if !run.completed() {
        return Ok(false);
    }
    if !run.succeeded() {
        failed_run(ctx, &format!("build.{asset}"), &run, "build-failed")?;
        return Ok(false);
    }
    let artifacts = ctx.w.forge.artifacts(id)?;
    let find = |name: &str| artifacts.iter().find(|a| a.name == name).cloned();
    let a = find(&format!("release-asset-{asset}")).ok_or_else(|| {
        Stop::new(
            "build-evidence-invalid",
            format!("run {id} has no release-asset-{asset} artifact"),
        )
    })?;
    if b.artifact_id.is_some_and(|pinned| pinned != a.id) {
        stop!(
            "retained-bytes-missing",
            "{asset}: pinned artifact {} is gone from run {id}",
            b.artifact_id.unwrap()
        );
    }
    if a.expired {
        stop!(
            "retained-bytes-missing",
            "{asset}: artifact {} of run {id} expired",
            a.id
        );
    }
    let dir = ctx.store.path(&format!("assets/{asset}.d"));
    ctx.event(&format!(
        "{asset}: downloading and verifying artifact {} from run {id}",
        a.id
    ));
    let _ = std::fs::remove_dir_all(&dir);
    ctx.w.forge.download_artifact(id, &a, &dir)?;
    let file = dir.join(asset);
    if !file.is_file() {
        stop!(
            "build-evidence-invalid",
            "{asset}: artifact {} has no {asset}",
            a.id
        );
    }
    let build_json = dir.join("build.json");
    if !build_json.is_file() {
        stop!(
            "build-evidence-invalid",
            "{asset}: artifact {} has no build.json",
            a.id
        );
    }
    let sha = sha256_file(&file)?;
    let size = std::fs::metadata(&file).map_err(anyhow::Error::from)?.len();
    let v: serde_json::Value = read_json(&build_json)
        .map_err(|e| Stop::new("build-evidence-invalid", format!("{asset}: {e:#}")))?;
    check_build_json(ctx, asset, &sha, size, &v)?;
    let attestation =
        ctx.w
            .forge
            .verify_attestation(&file, &ctx.plan.release_workflow, &ctx.j.commit)?;
    let kept = ctx.store.path(&format!("assets/{asset}"));
    std::fs::copy(&file, &kept).map_err(anyhow::Error::from)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o700))
            .map_err(anyhow::Error::from)?;
    }
    let mut kit = None;
    let mut additional_kits = BTreeMap::new();
    let ids: Vec<_> = ctx
        .plan
        .native
        .iter()
        .filter(|n| n.asset == asset)
        .map(|n| n.id.clone())
        .collect();
    for nid in ids {
        ctx.event(&format!("{asset}: verifying native kit {nid}"));
        let n = ctx
            .plan
            .native(&nid)
            .cloned()
            .ok_or_else(|| Stop::new("manifest-stale", format!("unknown native {nid}")))?;
        let ka = find(&n.kit_artifact).ok_or_else(|| {
            Stop::new(
                "build-evidence-invalid",
                format!("run {id} has no {} artifact", n.kit_artifact),
            )
        })?;
        let kdir = ctx.store.path(&format!("kits/{nid}.d"));
        let _ = std::fs::remove_dir_all(&kdir);
        ctx.w.forge.download_artifact(id, &ka, &kdir)?;
        let tar = kdir.join(format!("{}.tar", n.kit_artifact));
        if !tar.is_file() {
            stop!(
                "kit-invalid",
                "artifact {} has no {}.tar",
                ka.id,
                n.kit_artifact
            );
        }
        // Verify before extracting or executing anything (design 7.3).
        let att =
            ctx.w
                .forge
                .verify_attestation(&tar, &ctx.plan.release_workflow, &ctx.j.commit)?;
        let ksha = sha256_file(&tar)?;
        native::safe_extract(&tar, &ctx.store.path(&format!("kits/{nid}")))?;
        let k = native::check_kit(&ctx.store.path(&format!("kits/{nid}")), &n, &ctx.j.commit)?;
        if k.tree != ctx.j.tree
            || k.manifest_sha256 != ctx.j.manifest_sha256
            || k.plan_sha256 != ctx.j.plan_sha256
            || k.policy_sha256 != ctx.j.policy_sha256
        {
            stop!(
                "kit-invalid",
                "{nid}: kit source or policy differs from the pinned release plan"
            );
        }
        // The kit declares how deep its harness nests sockets (incident I1).
        if let Some(suffix) = k.socket_suffix_bytes {
            if n.host == "local"
                && ctx.w.production
                && !native::LocalHost::new().socket_budget_ok(suffix)
            {
                stop!("mac-host", "the kit needs {suffix} socket path bytes below the private root; the canonical root is too long");
            }
        }
        let record = KitRecord {
            artifact_id: ka.id,
            sha256: ksha,
            metadata_sha256: sha256_file(&ctx.store.path(&format!("kits/{nid}/kit.json")))?,
            attestation: att,
        };
        if ctx.plan.asset(asset).and_then(|a| a.native.as_ref()) == Some(&nid) {
            kit = Some(record);
        } else {
            additional_kits.insert(nid, record);
        }
    }
    let rec = ctx.j.builds.entry(asset.to_string()).or_default();
    rec.artifact_id = Some(a.id);
    rec.sha256 = Some(sha);
    rec.size = size;
    rec.attestation = Some(attestation);
    rec.build_json_sha256 = Some(sha256_file(&build_json)?);
    rec.kit = kit;
    rec.additional_kits = additional_kits;
    rec.run_attempt = run.attempt;
    rec.validated = Some(run.updated);
    service_stage(ctx, &format!("build.{asset}"), &run);
    ctx.save()?;
    Ok(true)
}

fn job(ctx: &Ctx, id: &str) -> Step<Option<NativeJob>> {
    let Some(n) = ctx.plan.native(id).cloned() else {
        return Ok(None);
    };
    let Some(b) = ctx.j.builds.get(&n.asset).filter(|b| b.validated.is_some()) else {
        return Ok(None);
    };
    let kit = if ctx.plan.asset(&n.asset).and_then(|a| a.native.as_deref()) == Some(id) {
        b.kit.as_ref()
    } else {
        b.additional_kits.get(id)
    };
    let Some(kit) = kit else { return Ok(None) };
    let out_dir = ctx.store.path(&format!("native/{id}"));
    crate::release::util::private_dir(&out_dir)?;
    let downloads = if n.host == "local" {
        native::fetch_downloads(ctx.w.forge.as_ref(), &ctx.store.path("fixtures"), &n)?
    } else {
        vec![]
    };
    Ok(Some(NativeJob {
        commit: ctx.j.commit.clone(),
        tree: ctx.j.tree.clone(),
        asset: ctx.store.path(&format!("assets/{}", n.asset)),
        asset_sha256: b.sha256.clone().unwrap_or_default(),
        kit_tar: ctx
            .store
            .path(&format!("kits/{id}.d/{}.tar", n.kit_artifact)),
        // The attested outer archive stays in BuildRecord.kit; kit-run binds
        // the extracted metadata, whose digest binds all runtime files.
        kit_sha256: kit.metadata_sha256.clone(),
        manifest_sha256: ctx.j.manifest_sha256.clone(),
        plan_sha256: ctx.j.plan_sha256.clone(),
        policy_sha256: ctx.j.policy_sha256.clone(),
        kit_dir: ctx.store.path(&format!("kits/{id}")),
        downloads,
        out_dir,
        deadline_seconds: n.budget_seconds + 300,
        plan: n,
    }))
}

/// Recheck durable operator evidence before freezing it into the release.
/// The journal records completion, but cannot replace raw execution proof.
pub(super) fn verify_native_records(ctx: &Ctx) -> Step {
    for n in &ctx.plan.native {
        ctx.event(&format!("{}: checking retained native result", n.id));
        let job = job(ctx, &n.id)?.ok_or_else(|| {
            Stop::new(
                "native-result-invalid",
                format!("{}: missing verified kit", n.id),
            )
        })?;
        let (path, exit) = native::durable(&job).ok_or_else(|| {
            Stop::new(
                "native-result-invalid",
                format!("{}: missing durable execution", n.id),
            )
        })?;
        let actual = native::verify_result(&job, &path, exit)?;
        if n.id == crate::manifest::release_policy::SBX_NATIVE
            && ctx.j.release.seal_sha256.is_none()
        {
            let current = ctx.w.hosts.sbx_qualification()?;
            let recorded: crate::release::sbx::Qualification =
                serde_json::from_value(actual.sbx.clone().unwrap_or_default())
                    .map_err(anyhow::Error::from)?;
            if current != recorded {
                stop!(
                    "native-result-invalid",
                    "SBX qualification changed before seal"
                );
            }
        }
        if ctx.j.native.get(&n.id) != Some(&actual) {
            stop!(
                "native-result-invalid",
                "{}: durable result differs from its recorded digest",
                n.id
            );
        }
    }
    Ok(())
}

/// Start natives whose verified asset and kit exist; poll the running
/// ones. Errors are deferred so hosted work continues.
fn natives_tick(ctx: &mut Ctx) -> Step {
    let ids: Vec<String> = ctx.plan.native.iter().map(|n| n.id.clone()).collect();
    for id in ids {
        if ctx.j.native.get(&id).is_some_and(|r| r.outcome == "passed")
            || ctx
                .deferred
                .iter()
                .any(|d| d.message.starts_with(&format!("{id}:")))
        {
            continue;
        }
        // A recorded product failure is final for this attempt: rerunning
        // until green is not a recovery (design 7.6).
        if let Some(r) = ctx.j.native.get(&id).filter(|r| r.outcome == "failed") {
            let why = r.reason.clone().unwrap_or_default();
            ctx.deferred
                .push(Stop::new("native-failed", format!("{id}: {why}")));
            continue;
        }
        let Some(job) = job(ctx, &id)? else { continue };
        if !ctx.hosts.contains_key(&id) {
            let host = ctx.w.hosts.host(&job.plan, ctx.linux_host.as_deref())?;
            ctx.store
                .log("native", &format!("{id} runs on {}", host.describe()));
            ctx.progress(&id, format!("native {id}: starting"));
            ctx.hosts.insert(id.clone(), host);
        }
        let host = &ctx.hosts[&id];
        let result = host.start(&job, ctx.w).and_then(|_| host.poll(&job, ctx.w));
        match result {
            Ok(None) => {}
            Ok(Some((path, exit))) => match native::verify_result(&job, &path, exit) {
                Ok(rec) => {
                    ctx.progress(&id, format!("native {id}: passed"));
                    ctx.timing("native", "result", &format!("{id} passed"));
                    ctx.j.native.insert(id.clone(), rec);
                    ctx.save()?;
                }
                Err(stop) => {
                    // A failed/invalid result is retained for review but
                    // never counts as passed.
                    let kept = path
                        .with_extension(format!("rejected-{}.json", ctx.w.clock.now().timestamp()));
                    let _ = std::fs::rename(&path, &kept);
                    let _ = std::fs::remove_file(job.out_dir.join("exit"));
                    if stop.kind == crate::release::error::Kind::Failed {
                        ctx.j.native.insert(
                            id.clone(),
                            crate::release::journal::NativeRecord {
                                outcome: "failed".into(),
                                result_sha256: sha256_file(&kept).unwrap_or_default(),
                                asset_sha256: job.asset_sha256.clone(),
                                kit_sha256: job.kit_sha256.clone(),
                                reason: Some(stop.message.clone()),
                                ..Default::default()
                            },
                        );
                        ctx.save()?;
                    }
                    ctx.deferred.push(Stop {
                        message: format!("{id}: {}", stop.message),
                        ..stop
                    });
                }
            },
            Err(stop) if stop.kind == crate::release::error::Kind::Killed => return Err(stop),
            Err(stop) => ctx.deferred.push(Stop {
                message: format!("{id}: {}", stop.message),
                ..stop
            }),
        }
    }
    Ok(())
}

pub fn collect(ctx: &mut Ctx) -> Step {
    let started = ctx.w.clock.now();
    loop {
        ctx.w.interrupted()?;
        let mut done = step_ci(ctx)?;
        let assets: Vec<String> = ctx.plan.assets.iter().map(|a| a.asset.clone()).collect();
        for a in &assets {
            done &= step_build(ctx, a)?;
        }
        natives_tick(ctx)?;
        report_hosted(ctx);
        if done {
            return Ok(());
        }
        if (ctx.w.clock.now() - started).num_minutes() > HOSTED_LIMIT_MINUTES {
            stop!(
                "timeout",
                "hosted work is still unfinished after {HOSTED_LIMIT_MINUTES} min; runs are kept"
            );
        }
        ctx.w.clock.sleep(ctx.w.poll_seconds);
    }
}

pub fn native_phase(ctx: &mut Ctx) -> Step {
    loop {
        ctx.w.interrupted()?;
        natives_tick(ctx)?;
        for n in &ctx.plan.native {
            if let Some(record) = ctx.j.native.get(&n.id) {
                ctx.progress(&n.id, format!("native {}: {}", n.id, record.outcome));
            } else {
                ctx.progress(&n.id, format!("native {}: running", n.id));
            }
        }
        let all = ctx.plan.native.iter().all(|n| {
            ctx.j
                .native
                .get(&n.id)
                .is_some_and(|r| r.outcome == "passed")
        });
        if all {
            return Ok(());
        }
        let open = ctx.plan.native.iter().any(|n| {
            !ctx.j.native.contains_key(&n.id)
                && !ctx
                    .deferred
                    .iter()
                    .any(|d| d.message.starts_with(&format!("{}:", n.id)))
        });
        if !open {
            // Report the first cause; completed checks stay recorded.
            return Err(ctx.deferred.remove(0));
        }
        ctx.w.clock.sleep(ctx.w.poll_seconds.min(10));
    }
}

/// Reconcile retained SBX bytes before replacement under new qualification inputs.
pub(crate) fn verify_sbx_retained(ctx: &Ctx) -> Step {
    let id = crate::manifest::release_policy::SBX_NATIVE;
    let job = job(ctx, id)?.ok_or_else(|| Stop::new("native-result-invalid", "SBX kit missing"))?;
    let (path, exit) = native::durable(&job)
        .ok_or_else(|| Stop::new("native-result-invalid", "SBX result missing"))?;
    let actual = native::verify_result(&job, &path, exit)?;
    if ctx.j.native.get(id) != Some(&actual) {
        stop!("native-result-invalid", "SBX evidence changed");
    }
    Ok(())
}
