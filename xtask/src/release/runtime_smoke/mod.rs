//! Required lightweight SBX obligations join the existing release seal.
mod cleanup;
pub(crate) mod fake;
pub(crate) use cleanup::cleanup_attempt;
pub(crate) mod sbx;
use super::{
    error::{Step, Stop},
    plan::Plan,
    run::Ctx,
    util::{random_id, sha256_file},
};
use crate::manifest::release_policy::{SBX_SMOKE_LINUX, SBX_SMOKE_MAC, SBX_SMOKE_STEPS};
use sbx::{SbxSelection, SbxSmokeBinding, SbxSmokeJob, SbxSmokeRecord, Target};
use std::collections::BTreeMap;

pub(crate) fn validate_plan(plan: &Plan) -> Step {
    let bad = || Stop::new("manifest-stale", "invalid lightweight SBX release policy");
    for (id, asset, host, suite) in [
        (
            crate::manifest::release_policy::APPLE_NATIVE,
            "awman-macos-arm64",
            "local",
            "artifact-apple-smoke",
        ),
        (
            crate::manifest::release_policy::DOCKER_NATIVE,
            "awman-linux-amd64",
            "ssh",
            "artifact-docker-smoke",
        ),
    ] {
        let n = plan.native(id).ok_or_else(|| {
            Stop::new(
                "manifest-stale",
                format!("missing required runtime obligation {id}"),
            )
        })?;
        if n.asset != asset
            || n.host != host
            || n.suites != [suite]
            || !n.required
            || n.budget_seconds != 480
            || n.tests != crate::manifest::release_policy::native_tests(id)?
        {
            return Err(Stop::new(
                "manifest-stale",
                format!("invalid runtime selection {id}"),
            ));
        }
    }
    if plan.sbx_smoke.len() != 2 {
        return Err(bad());
    }
    for (id, asset, triple, host, selection) in [
        (
            SBX_SMOKE_MAC,
            "awman-macos-arm64",
            "aarch64-apple-darwin",
            "local",
            "always",
        ),
        (
            SBX_SMOKE_LINUX,
            "awman-linux-amd64",
            "x86_64-unknown-linux-gnu",
            "ssh",
            "explicit-linux-host",
        ),
    ] {
        let p = plan.sbx_smoke.iter().find(|p| p.id == id).ok_or_else(bad)?;
        if p.asset != asset
            || p.triple != triple
            || p.host != host
            || p.selection != selection
            || p.steps != SBX_SMOKE_STEPS
            || p.ready_agent != "claude"
            || p.min_cli_version != "0.54.0"
            || p.lifecycle_budget_seconds != 600
            || p.ready_budget_seconds != 420
            || p.budget_seconds != 1080
            || plan.asset(asset).is_none()
        {
            return Err(bad());
        }
    }
    Ok(())
}

pub(crate) fn selection(explicit: Option<&str>) -> Step<SbxSelection> {
    sbx::select(SBX_SMOKE_MAC, SBX_SMOKE_LINUX, explicit)
        .map_err(|e| Stop::new("sbx-selection-changed", e.to_string()))
}
fn frozen<'a>(ctx: &'a Ctx<'_>) -> Step<&'a SbxSelection> {
    let s = ctx.j.sbx_selection.as_ref().ok_or_else(|| {
        Stop::new(
            "state-corrupt",
            "attempt has no frozen SBX selection; supersede the old attempt",
        )
    })?;
    let explicit = s
        .hosts
        .iter()
        .find(|h| h.host == "ssh")
        .and_then(|h| h.destination.as_deref());
    if *s != selection(explicit)? {
        return Err(Stop::new("state-corrupt", "invalid frozen SBX selection"));
    }
    Ok(s)
}
fn target(h: &sbx::SbxHostSelection) -> Target {
    match &h.destination {
        Some(destination) => Target::Ssh {
            destination: destination.clone(),
        },
        None => Target::Local,
    }
}
pub(crate) fn selection_check(ctx: &Ctx, explicit: Option<&str>) -> Step {
    let s = frozen(ctx)?;
    // Only verified complete evidence permits omission of an already completed Linux host.
    let mut passed = std::collections::BTreeSet::new();
    for (id, r) in &ctx.j.sbx_smoke {
        if r.outcome == "passed" {
            if ctx.j.release.seal_sha256.is_none() {
                verify_one(ctx, id)?;
            }
            passed.insert(id.clone());
        }
    }
    s.admits(&selection(explicit)?, &passed)
        .map_err(|m| Stop::new("sbx-selection-changed", m))
}
pub(crate) fn preflight(
    w: &super::world::World,
    plan: &Plan,
    selected: &SbxSelection,
    records: &BTreeMap<String, SbxSmokeRecord>,
) -> Step {
    for h in &selected.hosts {
        if records
            .get(&h.id)
            .is_some_and(|r| r.outcome == "passed" || r.outcome == "failed")
        {
            continue;
        }
        let p = plan
            .sbx_smoke
            .iter()
            .find(|p| p.id == h.id)
            .ok_or_else(|| Stop::new("manifest-stale", "SBX selection missing from plan"))?;
        println!(
            "release SBX selection: {} on {}",
            h.id,
            h.destination.as_deref().unwrap_or("this Mac")
        );
        w.hosts
            .sbx_smoke_probe(&target(h), &p.min_cli_version)
            .map_err(|s| Stop {
                message: format!("{}: {}", h.id, s.message),
                ..s
            })?;
    }
    Ok(())
}
pub(crate) fn job(ctx: &Ctx, id: &str) -> Step<Option<SbxSmokeJob>> {
    let selected = frozen(ctx)?;
    let h = selected
        .hosts
        .iter()
        .find(|h| h.id == id)
        .ok_or_else(|| Stop::new("sbx-smoke-invalid", "unselected SBX obligation"))?;
    let p = ctx
        .plan
        .sbx_smoke
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| Stop::new("manifest-stale", "missing SBX plan"))?;
    let Some(b) = ctx.j.builds.get(&p.asset).filter(|b| b.validated.is_some()) else {
        return Ok(None);
    };
    let sha = b
        .sha256
        .clone()
        .ok_or_else(|| Stop::new("sbx-smoke-invalid", "asset has no digest"))?;
    if b.attestation
        .as_ref()
        .is_none_or(|a| a.subject_sha256 != sha || a.source_digest != ctx.j.commit || !a.hosted)
    {
        return Err(Stop::new(
            "sbx-smoke-invalid",
            "SBX asset lacks matching attestation",
        ));
    }
    let asset_path = ctx.store.path(&format!("assets/{}", p.asset));
    if sha256_file(&asset_path)? != sha {
        return Err(Stop::new("retained-bytes-missing", "SBX asset changed"));
    }
    let binding = SbxSmokeBinding {
        schema: sbx::SCHEMA,
        obligation: id.into(),
        triple: p.triple.clone(),
        commit: ctx.j.commit.clone(),
        tree: ctx.j.tree.clone(),
        manifest_sha256: ctx.j.manifest_sha256.clone(),
        plan_sha256: ctx.j.plan_sha256.clone(),
        policy_sha256: ctx.j.policy_sha256.clone(),
        asset: p.asset.clone(),
        asset_sha256: sha,
        selection_sha256: selected.sha256.clone(),
        destination: h.destination.clone(),
        steps: p.steps.clone(),
        ready_agent: p.ready_agent.clone(),
        min_cli_version: p.min_cli_version.clone(),
        lifecycle_budget_seconds: p.lifecycle_budget_seconds,
        ready_budget_seconds: p.ready_budget_seconds,
        budget_seconds: p.budget_seconds,
    };
    Ok(Some(SbxSmokeJob::new(
        binding,
        target(h),
        asset_path,
        ctx.store.path(&format!("sbx-smoke/{id}")),
        ctx.w.interrupted.clone(),
    )))
}
fn verify_one(ctx: &Ctx, id: &str) -> Step {
    let j = job(ctx, id)?.ok_or_else(|| Stop::new("sbx-smoke-invalid", "missing SBX asset"))?;
    let record = ctx.w.hosts.sbx_smoke_verify(&j)?;
    if ctx.j.sbx_smoke.get(id) != Some(&record) {
        return Err(Stop::new(
            "sbx-smoke-invalid",
            format!("{id}: journal differs from raw SBX evidence"),
        ));
    }
    Ok(())
}
fn outcome(id: &str, r: &SbxSmokeRecord) -> Step {
    if r.outcome == "passed" {
        return Ok(());
    }
    let code = if r.outcome == "failed" {
        "sbx-smoke-failed"
    } else {
        "sbx-smoke-blocked"
    };
    Err(Stop::new(
        code,
        format!(
            "{id}: {} [{}] {}",
            r.outcome.to_uppercase(),
            r.reason_code.as_deref().unwrap_or("unknown"),
            r.reason.as_deref().unwrap_or("")
        ),
    ))
}
pub(crate) fn verify(ctx: &Ctx) -> Step {
    if ctx.j.sbx_smoke.len() != frozen(ctx)?.hosts.len() {
        return Err(Stop::new(
            "sbx-smoke-invalid",
            "missing or unexpected SBX obligation",
        ));
    }
    for h in &frozen(ctx)?.hosts {
        verify_one(ctx, &h.id)?;
        outcome(&h.id, &ctx.j.sbx_smoke[&h.id])?;
        let current = ctx.w.hosts.sbx_smoke_identity(&target(h))?;
        let r = &ctx.j.sbx_smoke[&h.id];
        if r.cli_sha256.as_ref() != Some(&current.sha256)
            || r.cli_version.as_ref() != Some(&current.version)
        {
            return Err(Stop::new(
                "sbx-smoke-blocked",
                format!(
                    "{}: CLI identity changed; resume to run a fresh check",
                    h.id
                ),
            ));
        }
    }
    Ok(())
}
pub(crate) fn resume(ctx: &mut Ctx) -> Step {
    let hosts = frozen(ctx)?.hosts.clone();
    if ctx.j.release.seal_sha256.is_some() {
        return Ok(());
    }
    for h in &hosts {
        if ctx
            .j
            .sbx_smoke
            .get(&h.id)
            .is_none_or(|r| r.outcome != "passed")
        {
            continue;
        }
        verify_one(ctx, &h.id)?;
        let cli = ctx.w.hosts.sbx_smoke_identity(&target(h))?;
        let r = &ctx.j.sbx_smoke[&h.id];
        if r.cli_version.as_ref() != Some(&cli.version)
            || r.cli_sha256.as_ref() != Some(&cli.sha256)
        {
            let j = job(ctx, &h.id)?.expect("verified job");
            std::fs::rename(
                &j.out_dir,
                ctx.store
                    .path(&format!("sbx-smoke/{}-previous-{}", h.id, random_id()?)),
            )
            .map_err(anyhow::Error::from)?;
            ctx.j.sbx_smoke.remove(&h.id);
            for p in ["native", "seal", "confirm"] {
                ctx.j.phases.remove(p);
            }
            ctx.save()?;
        }
    }
    selection_check(ctx, ctx.sbx_explicit_host.as_deref())?;
    preflight(ctx.w, &ctx.plan, frozen(ctx)?, &ctx.j.sbx_smoke)
}

pub(crate) fn tick(ctx: &mut Ctx) -> Step {
    let hosts = frozen(ctx)?.hosts.clone();
    for h in hosts {
        let id = &h.id;
        if ctx
            .j
            .sbx_smoke
            .get(id)
            .is_some_and(|r| r.outcome == "passed")
            || ctx
                .deferred
                .iter()
                .any(|d| d.message.starts_with(&format!("{id}:")))
        {
            continue;
        }
        if let Some(r) = ctx.j.sbx_smoke.get(id).filter(|r| r.outcome == "failed") {
            ctx.deferred.push(outcome(id, r).unwrap_err());
            continue;
        }
        if let Some(worker) = ctx.sbx_workers.get(id) {
            if !worker.is_finished() {
                continue;
            }
            let result = ctx
                .sbx_workers
                .remove(id)
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err(Stop::new("sbx-smoke-invalid", "SBX worker panicked")));
            match result {
                Ok(r) => {
                    ctx.timing("native", "result", &format!("{id} {}", r.outcome));
                    let result = outcome(id, &r);
                    ctx.j.sbx_smoke.insert(id.clone(), r);
                    ctx.save()?;
                    if let Err(e) = result {
                        ctx.deferred.push(e);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind,
                        super::error::Kind::Killed | super::error::Kind::Interrupted
                    ) =>
                {
                    return Err(e)
                }
                Err(e) => ctx.deferred.push(Stop {
                    message: format!("{id}: {}", e.message),
                    ..e
                }),
            }
            continue;
        }
        // Two VM consumers per host. Linux SBX additionally waits for Docker.
        let active = ctx
            .hosts
            .keys()
            .filter(|n| {
                ctx.plan.native(n).is_some_and(|p| p.host == h.host)
                    && !ctx
                        .j
                        .native
                        .get(*n)
                        .is_some_and(|r| matches!(r.outcome.as_str(), "passed" | "failed"))
                    && !ctx
                        .deferred
                        .iter()
                        .any(|d| d.message.starts_with(&format!("{n}:")))
            })
            .count();
        let docker = crate::manifest::release_policy::DOCKER_NATIVE;
        if active >= 2
            || (h.host == "ssh"
                && ctx.plan.native(docker).is_some()
                && !ctx
                    .j
                    .native
                    .get(docker)
                    .is_some_and(|r| matches!(r.outcome.as_str(), "passed" | "failed"))
                && !ctx
                    .deferred
                    .iter()
                    .any(|d| d.message.starts_with(&format!("{docker}:"))))
        {
            continue;
        }
        let Some(j) = job(ctx, id)? else { continue };
        ctx.timing(
            "native",
            "start",
            &format!(
                "{id} on {} (1080 s deadline)",
                h.destination.as_deref().unwrap_or("this Mac")
            ),
        );
        match ctx.w.hosts.sbx_smoke_start(j) {
            Ok(worker) => {
                ctx.sbx_workers.insert(id.clone(), worker);
            }
            Err(e) => ctx.deferred.push(Stop {
                message: format!("{id}: {}", e.message),
                ..e
            }),
        }
    }
    Ok(())
}
pub(crate) fn complete(ctx: &Ctx) -> bool {
    frozen(ctx).is_ok_and(|s| {
        s.hosts.iter().all(|h| {
            ctx.j
                .sbx_smoke
                .get(&h.id)
                .is_some_and(|r| r.outcome == "passed")
        })
    })
}
pub(crate) fn pending(ctx: &Ctx) -> bool {
    frozen(ctx).is_ok_and(|s| {
        s.hosts.iter().any(|h| {
            !ctx.j
                .sbx_smoke
                .get(&h.id)
                .is_some_and(|r| r.outcome == "passed" || r.outcome == "failed")
                && !ctx
                    .deferred
                    .iter()
                    .any(|d| d.message.starts_with(&format!("{}:", h.id)))
        })
    })
}
// Stop and join the bounded worker pipelines before returning an interrupted invocation.
pub(crate) fn drain(ctx: &mut Ctx) {
    if !ctx.sbx_workers.is_empty() {
        ctx.w
            .interrupted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for (_, worker) in std::mem::take(&mut ctx.sbx_workers) {
            let _ = worker.join();
        }
    }
}

pub(crate) mod rehearsal;
