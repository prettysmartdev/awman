//! Preserve ownership until unfinished runtime resources are reconciled, including
//! when the operator abandons or supersedes an attempt rather than resuming it.
use crate::release::{
    error::{Step, Stop},
    journal::{Journal, Store},
    plan::Plan,
    run::Ctx,
    util::read_json,
    world::World,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn cleanup_attempt(w: &World, store: &Store, journal: &Journal) -> Step {
    let (plan, digest) =
        Plan::parse(&std::fs::read(store.path("plan.json")).map_err(anyhow::Error::from)?)?;
    if digest != journal.plan_sha256 {
        return Err(Stop::new(
            "state-corrupt",
            "cleanup plan differs from pinned attempt",
        ));
    }
    let ctx = Ctx {
        w,
        store: Store::view(&store.dir),
        j: journal.clone(),
        plan,
        repo: w.forge.repo()?,
        linux_host: None,
        sbx_explicit_host: None,
        previous_owners: vec![],
        deferred: vec![],
        retried: BTreeSet::new(),
        controller_sha256: String::new(),
        hosts: BTreeMap::new(),
        invoked: std::time::Instant::now(),
        sbx_workers: BTreeMap::new(),
        progress: None,
        fresh: false,
    };
    for n in &ctx.plan.native {
        let out = store.path(&format!("native/{}", n.id));
        let marker = ["lease.json", "intent.json"]
            .iter()
            .map(|s| out.join(s))
            .find(|p| p.exists());
        let Some(marker) = marker else { continue };
        let v: serde_json::Value = read_json(&marker)?;
        let host = if n.host == "ssh" {
            // Production leases retain their original destination. Fake leases
            // need no transport; use the diagnostic host only in that world.
            v["host"]
                .as_str()
                .map(str::to_string)
                .or_else(|| (!w.production).then(|| "build@kvm1".into()))
        } else {
            None
        };
        if n.host == "ssh"
            && !host
                .as_deref()
                .is_some_and(crate::release::util::valid_ssh_destination)
        {
            return Err(Stop::new(
                "state-corrupt",
                "cleanup has no authenticated SSH destination",
            ));
        }
        let job = crate::release::phases::hosted::job(&ctx, &n.id)?
            .ok_or_else(|| Stop::new("native-result-invalid", "cleanup has no bound kit"))?;
        w.hosts.host(n, host.as_deref())?.cancel(&job, w)?;
    }
    for h in &super::frozen(&ctx)?.hosts {
        let out = store.path(&format!("sbx-smoke/{}", h.id));
        if !out.join("intent.json").exists()
            || journal
                .sbx_smoke
                .get(&h.id)
                .is_some_and(|r| r.outcome == "passed")
        {
            continue;
        }
        let job = super::job(&ctx, &h.id)?
            .ok_or_else(|| Stop::new("sbx-smoke-invalid", "cleanup has no bound SBX asset"))?;
        w.hosts.sbx_smoke_reconcile(&job)?;
    }
    Ok(())
}
