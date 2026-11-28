//! Fault-world packets. Only FakeHosts uses this seam; production verifies the
//! lifecycle module's raw service receipts. No fake evidence is native proof.
use super::sbx::{SbxSmokeJob, SbxSmokeRecord};
use crate::release::{
    error::{Step, Stop},
    forge_dir::FakeHosts,
    util::{read_json, sha256_file, write_json},
    world::Hosts,
};
use serde_json::{json, Value};

pub(crate) fn run(hosts: &FakeHosts, job: &SbxSmokeJob) -> Step<SbxSmokeRecord> {
    let id = &job.binding.obligation;
    if job.out_dir.join("report.json").exists() {
        let r = verify(job)?;
        if r.outcome != "blocked" {
            return Ok(r);
        }
        std::fs::rename(
            &job.out_dir,
            job.out_dir
                .with_extension(crate::release::util::random_id()?),
        )
        .map_err(anyhow::Error::from)?;
    }
    crate::release::util::private_dir(&job.out_dir)?;
    let intent = job.out_dir.join("intent.json");
    if intent.exists() {
        hosts.dir.call(format!("sbx-smoke-reconcile {id}"));
    }
    write_json(&intent, &json!({"id":id,"binding":job.binding.sha256()?}))?;
    hosts.dir.call(format!("sbx-smoke-run {id}"));
    if hosts.dir.fault(&format!("sbx_smoke:{id}")).as_deref() == Some("crash") {
        return Err(Stop::new("killed", "interrupted SBX check"));
    }
    let s = hosts.dir.load();
    let script = s.script.sbx_smoke.get(id).cloned().unwrap_or_default();
    let cli = hosts.sbx_smoke_identity(&job.target)?;
    let mut steps = job.binding.steps.clone();
    if script.omit_test {
        steps.pop();
    }
    let raw = json!({"binding":job.binding,"steps":steps,"outcome":script.outcome.unwrap_or("passed".into()),"reason":script.reason_code,"cli":cli,"finished":s.clock});
    write_json(&job.out_dir.join("raw.json"), &raw)?;
    let r = record(job, &raw)?;
    write_json(&job.out_dir.join("report.json"), &r)?;
    verify(job)
}
fn record(job: &SbxSmokeJob, v: &Value) -> Step<SbxSmokeRecord> {
    Ok(SbxSmokeRecord {
        outcome: v["outcome"].as_str().unwrap_or("invalid").into(),
        report_sha256: sha256_file(&job.out_dir.join("raw.json"))?,
        binding_sha256: job.binding.sha256()?,
        asset_sha256: job.binding.asset_sha256.clone(),
        selection_sha256: job.binding.selection_sha256.clone(),
        destination: job.binding.destination.clone(),
        cli_version: v["cli"]["version"].as_str().map(Into::into),
        cli_sha256: v["cli"]["sha256"].as_str().map(Into::into),
        reason: v["reason"].as_str().map(Into::into),
        reason_code: v["reason"].as_str().map(Into::into),
        finished: serde_json::from_value(v["finished"].clone()).map_err(anyhow::Error::from)?,
        ..Default::default()
    })
}
pub(crate) fn verify(job: &SbxSmokeJob) -> Step<SbxSmokeRecord> {
    let check = || -> anyhow::Result<SbxSmokeRecord> {
        let v: Value = read_json(&job.out_dir.join("raw.json"))?;
        anyhow::ensure!(
            v["binding"] == serde_json::to_value(&job.binding)?,
            "stale SBX binding"
        );
        if v["outcome"] == "passed" {
            anyhow::ensure!(
                v["steps"] == json!(job.binding.steps),
                "short/duplicate SBX inventory"
            );
        }
        let r = record(job, &v).map_err(|s| anyhow::anyhow!(s.to_string()))?;
        let stored: SbxSmokeRecord = read_json(&job.out_dir.join("report.json"))?;
        anyhow::ensure!(r == stored, "tampered SBX packet");
        Ok(r)
    };
    check().map_err(|e| Stop::new("sbx-smoke-invalid", e.to_string()))
}
pub(crate) fn native(job: &crate::release::native::NativeJob, r: &mut Value) -> Step {
    let apple = job.plan.id == crate::manifest::release_policy::APPLE_NATIVE;
    let runtime = if apple { "apple" } else { "docker" };
    let root = "/tmp/awr-fake/tmp";
    let owner = "awr-0123456789abcdef";
    let project = "awsmoke-0123456789abcdef";
    let engine = (!apple).then_some("unix:///var/run/docker.sock");
    r["host"]["hostname_sha256"] = json!("f".repeat(64));
    r["service"] = json!({"kind":if apple {"apple-containers"} else {"docker"},"cli_path":"/fake/runtime","cli_sha256":"d".repeat(64),"cli_version":"1.0","service_version":"1.0","server_version":"1.0","engine":engine});
    for (file, key, value) in [
        (
            "runtime-owned.json",
            "runtime_owned_sha256",
            json!({"schema":1,"armed":true,"runtime":runtime,"owner":owner,"project":project,"root":root,"cli_path":"/fake/runtime","cli_sha256":"d".repeat(64),"engine":engine}),
        ),
        (
            "runtime-reconcile.json",
            "runtime_reconcile_sha256",
            json!({"schema":1,"owner":owner,"leaked":0,"remaining":0,"verified":true,"removal_errors":[]}),
        ),
        (
            "runtime-smoke.json",
            "runtime_smoke_sha256",
            json!({"schema":1,"runtime":runtime,"owner":owner,"project":project,"artifact_sha256":job.asset_sha256,"ready_exit":0,"task_exit":37,"guest_os":"Linux","prompt_observed":true,"stderr_observed":true,"removed":true,"verified_absent":true,"mount_sources":[format!("{root}/repo")],"containers":["owned"]}),
        ),
    ] {
        write_json(&job.out_dir.join(file), &value)?;
        r[key] = json!(sha256_file(&job.out_dir.join(file))?);
    }
    Ok(())
}
