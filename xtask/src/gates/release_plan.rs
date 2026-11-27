//! Stable JSON boundary consumed by the independent release controller.
use crate::{manifest::release_policy::native_tests, manifest::Manifest};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub fn create(m: &Manifest) -> Result<Value> {
    crate::manifest::validate::validate(m, chrono::Utc::now().date_naive())?;
    let mut assets = Vec::new();
    for asset in &m.release.assets {
        let (id, b) = m
            .builds
            .iter()
            .find(|(_, b)| b.asset.as_ref() == Some(asset))
            .context("missing asset build")?;
        let p = &m.platforms[&b.platform];
        ensure!(
            b.kind == "release" && b.builder == "hosted",
            "invalid shipping build {id}"
        );
        let mut cargo = vec![
            "build".to_string(),
            "--locked".into(),
            "--release".into(),
            "-p".into(),
            "awman-cli".into(),
            "--bin".into(),
            "awman".into(),
        ];
        // Shipping features exclude guest-drivers, which belong to test kits.
        if b.features == "builtin" {
            cargo.extend(["--features".into(), "builtin-runtime".into()]);
        }
        if let Some(target) = &b.target {
            cargo.extend(["--target".into(), target.clone()]);
        }
        let dir = b
            .target
            .as_ref()
            .map(|t| format!("target/{t}/release"))
            .unwrap_or_else(|| "target/release".into());
        assets.push(json!({"asset":asset,"build":id,"triple":p.triple,"runs_on":p.runs_on,
            "toolchain":p.toolchain,"cargo":cargo,"binary":format!("{dir}/awman{}",if p.triple.contains("windows") {".exe"} else {""}),
            "sign":p.triple.contains("apple"),"abi":if asset == "awman-linux-amd64" {Some("glibc-2.35")} else {None},
            "builder":"hosted","smoke_suites":m.release.smoke_suites,
            "native":m.release.native.iter().find(|(_,n)| n.required && &n.asset == asset).map(|(id,_)| id)}));
    }
    let native = m.release.native.iter().filter(|(_, n)| n.required).map(|(id,n)| -> Result<Value> {
        let b = m.builds.values().find(|b| b.asset.as_ref() == Some(&n.asset)).context("native asset missing")?;
        let fixtures = n.fixtures.iter().map(|id| -> Result<Value> {
            let f = m.fixtures.get(id).context("native fixture missing")?;
            Ok(json!({"id":id,"file":f.file,"arch":f.arch,"sha256":f.sha256,"url":f.source,"pending":f.pending}))
        }).collect::<Result<Vec<_>>>()?;
        Ok(json!({"id":id,"asset":n.asset,"host":n.host,"triple":m.platforms[&b.platform].triple,
            "suites":n.suites,"tests":native_tests(id)?,"fixtures":fixtures,"previous_release":n.previous_release.as_ref().map(|t| {
                let p=&m.previous_release[t];
                json!({"triple":t,"version":p.version,"asset":p.asset,"sha256":p.sha256,
                    "url":format!("https://github.com/prettysmartdev/awman/releases/download/{}/{}",p.version,p.asset),"pending":p.pending})
            }),
            "budget_seconds":n.budget_seconds,"required":n.required,"kit_artifact":format!("native-kit-{id}")}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"schema":1,"manifest_sha256":m.version(),"policy_sha256":crate::manifest::release_policy::digest(m)?,"evidence_validity_days":m.release.evidence_valid_days,"gate":"release",
        "ci":{"workflow":"ci.yml","inputs":{"gate":"release"},"evidence_artifact_prefix":"gate-evidence-","evidence_file":"gate-evidence.json"},
        "release_workflow":"release.yml","checksums":m.release.checksums,"evidence":m.release.evidence,"assets":assets,"native":native}),
    )
}

/// Exact bytes of the CLI plan, including its single trailing newline. Native
/// kits and the controller bind this same representation, not a second JSON hash.
pub fn encoded(m: &Manifest) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&create(m)?)?;
    bytes.push(b'\n');
    Ok(bytes)
}
